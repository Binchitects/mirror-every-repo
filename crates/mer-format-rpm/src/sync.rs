//! Collector-side sync of RPM repositories into one repo snapshot.
use crate::repodata::{decompress, filelists_pkgid, for_each_package, gzip, parse_package, parse_repomd, with_location, Package, RepomdEntry};
use crate::resolve::{resolve, Index};
use mer_blob::Expect;
use mer_core::{Digest, EntryRole};
use mer_fetch::{host_of, FetchError, HttpClient};
use mer_format_apt::{verify_detached, SiteKey};
use mer_storage::{Storage, StorageError};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::PathBuf;

const MAX_REPOMD: u64 = 16 << 20;
const MAX_METADATA: u64 = 2 << 30;
const MAX_PACKAGE: u64 = 8 << 30;

#[derive(Debug, thiserror::Error)]
pub enum RpmError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("upstream repository: {0}")]
    Repo(String),
    #[error("upstream signature: {0}")]
    Signature(String),
    #[error("signing: {0}")]
    Sign(String),
    #[error("{0}")]
    Config(String),
    #[error("package {0} is not in the upstream repositories")]
    UnknownPackage(String),
}

impl From<mer_meta::MetaError> for RpmError {
    fn from(e: mer_meta::MetaError) -> Self {
        RpmError::Storage(e.into())
    }
}

#[derive(Clone, Debug, Default)]
pub struct RpmConfig {
    /// The upstream repositories (each the directory that holds
    /// `repodata/`). Dependencies are resolved across all of them, and all
    /// of it becomes one repository on the vault.
    pub repo_urls: Vec<String>,
    /// Packages to mirror: names, or anything a package provides
    /// (`/usr/bin/git`, `libfoo.so.1()(64bit)`).
    pub packages: Vec<String>,
    /// Architectures of the machines inside the air gap, best first.
    /// `noarch` packages are always accepted.
    pub archs: Vec<String>,
    /// Also mirror what packages recommend, as dnf installs by default.
    pub recommends: bool,
    /// Keys of the upstream repositories. With it, the signature of each
    /// `repomd.xml` is required and verified.
    pub keyring: Option<PathBuf>,
    /// GnuPG home of the site key. With it, the metadata of the mirrored
    /// repository is signed, for clients that set `repo_gpgcheck=1`.
    pub sign_home: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RpmReport {
    /// Packages the upstream repositories list for the accepted architectures.
    pub available: usize,
    pub packages: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub reused: usize,
    /// Whether the upstream metadata signatures were verified.
    pub signatures_verified: bool,
    pub warnings: Vec<String>,
    pub snapshot: Option<u64>,
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// A package file path the mirror will store and serve.
fn safe_href(href: &str) -> bool {
    href.ends_with(".rpm")
        && !href.starts_with('/')
        && !href.contains("://")
        && href.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
        && href.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_' | b'+' | b'~' | b'^' | b':'))
}

struct Upstream {
    base: String,
    primary: (String, Vec<u8>),
    filelists: Option<RepomdEntry>,
    /// Downloaded on first need: it is large and often not needed.
    filelists_body: Option<Vec<u8>>,
    timestamp: u64,
}

fn fetch_verified(client: &HttpClient, base: &str, entry: &RepomdEntry, warnings: &mut Vec<String>) -> Result<Vec<u8>, RpmError> {
    if entry.href.contains("..") || entry.href.contains("://") || entry.href.starts_with('/') {
        return Err(RpmError::Repo(format!("{base}: metadata location {:?} is not acceptable", entry.href)));
    }
    let url = format!("{base}/{}", entry.href);
    let body = client.get(&url, &[])?.bytes(&url, MAX_METADATA)?;
    if entry.checksum_type == "sha256" {
        let actual = sha256_hex(&body);
        if actual != entry.checksum {
            return Err(RpmError::Repo(format!("{url}: repomd.xml states SHA-256 {}, the file has {actual}", entry.checksum)));
        }
    } else {
        warnings.push(format!("{url}: its checksum is {}, which is not checked here", entry.checksum_type));
    }
    Ok(body)
}

/// Mirror the configured packages and what they need into a new snapshot.
///
/// The chain of trust: the TLS connection (and, with a keyring, the
/// signature) for `repomd.xml`; its SHA-256 for the package list; the
/// SHA-256 in the package list for each package. The packages themselves
/// are stored unchanged, so their own signatures still verify on the
/// clients (`gpgcheck=1`).
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &RpmConfig) -> Result<RpmReport, RpmError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "rpm" {
        return Err(RpmError::Config(format!("repo {repo} has format {}, not rpm", repo_row.format)));
    }
    if cfg.repo_urls.is_empty() || cfg.packages.is_empty() || cfg.archs.is_empty() {
        return Err(RpmError::Config("at least one repository URL, one package and one architecture are needed".into()));
    }
    let mut report = RpmReport::default();
    let mut archs = cfg.archs.clone();
    if !archs.iter().any(|a| a == "noarch") {
        archs.push("noarch".into());
    }

    // 1. The metadata of every upstream repository.
    let mut upstreams: Vec<Upstream> = Vec::new();
    for url in &cfg.repo_urls {
        if host_of(url).is_none() {
            return Err(RpmError::Config(format!("{url:?} is not an http(s) URL")));
        }
        let base = url.trim_end_matches('/').to_string();
        let repomd_url = format!("{base}/repodata/repomd.xml");
        let repomd = client.get(&repomd_url, &[])?.bytes(&repomd_url, MAX_REPOMD)?;
        if let Some(keyring) = &cfg.keyring {
            let sig_url = format!("{repomd_url}.asc");
            let signature = match client.get(&sig_url, &[]) {
                Ok(r) => r.bytes(&sig_url, 1 << 20)?,
                Err(FetchError::NotFound(_)) => return Err(RpmError::Signature(format!("{base} does not sign its metadata (no repomd.xml.asc); without --keyring it can be mirrored unsigned"))),
                Err(e) => return Err(e.into()),
            };
            verify_detached(keyring, &repomd, &signature).map_err(|e| RpmError::Signature(format!("{repomd_url}: {e}")))?;
            report.signatures_verified = true;
        }
        let entries = parse_repomd(&String::from_utf8_lossy(&repomd)).map_err(|e| RpmError::Repo(format!("{repomd_url}: {e}")))?;
        let primary = entries.iter().find(|e| e.kind == "primary").ok_or_else(|| RpmError::Repo(format!("{repomd_url} lists no package list")))?;
        let body = fetch_verified(client, &base, primary, &mut report.warnings)?;
        upstreams.push(Upstream {
            base,
            primary: (primary.href.clone(), body),
            filelists: entries.iter().find(|e| e.kind == "filelists").cloned(),
            filelists_body: None,
            timestamp: primary.timestamp,
        });
    }

    // 2. Every package of an accepted architecture, from all of them.
    let mut packages: Vec<Package> = Vec::new();
    let mut source: Vec<usize> = Vec::new(); // which upstream each package is from
    let mut seen_ids: BTreeSet<String> = BTreeSet::new();
    let mut modular = 0usize;
    for (u, up) in upstreams.iter().enumerate() {
        let reader = decompress(&up.primary.0, &up.primary.1).map_err(RpmError::Repo)?;
        for_each_package::<RpmError>(reader, |fragment| {
            let p = parse_package(fragment).map_err(|e| RpmError::Repo(format!("{}: a package entry cannot be read: {e}", up.base)))?;
            if !archs.contains(&p.arch) {
                return Ok(());
            }
            if p.evr.release.contains(".module+") || p.evr.release.contains(".module_") {
                modular += 1; // belongs to a module stream: see the warning below
                return Ok(());
            }
            if seen_ids.insert(p.pkgid.clone()) {
                packages.push(p);
                source.push(u);
            }
            Ok(())
        })?;
    }
    if modular > 0 {
        report.warnings.push(format!("{modular} packages belong to module streams and are left out: modules are not mirrored"));
    }
    report.available = packages.len();
    let mut index = Index::new(packages);

    // 3. What is needed. A file that is required and that the package lists
    //    do not place is looked up in the complete file lists, once.
    let mut resolution = resolve(&index, &cfg.packages, &archs, cfg.recommends).map_err(RpmError::UnknownPackage)?;
    if !resolution.unplaced_files.is_empty() {
        let wanted: Vec<String> = resolution.unplaced_files.iter().cloned().collect();
        let mut found: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for up in upstreams.iter_mut() {
            let Some(entry) = up.filelists.clone() else { continue };
            let body = fetch_verified(client, &up.base, &entry, &mut report.warnings)?;
            let reader = decompress(&entry.href, &body).map_err(RpmError::Repo)?;
            for_each_package::<RpmError>(reader, |fragment| {
                for path in &wanted {
                    if fragment.contains(&format!(">{path}</file>")) {
                        if let Some(id) = filelists_pkgid(fragment) {
                            found.entry(path.clone()).or_default().insert(id.to_ascii_lowercase());
                        }
                    }
                }
                Ok(())
            })?;
            up.filelists_body = Some(body);
        }
        index.add_files(&found);
        resolution = resolve(&index, &cfg.packages, &archs, cfg.recommends).map_err(RpmError::UnknownPackage)?;
    }
    report.warnings.append(&mut resolution.warnings);

    // 4. The package files.
    let mut entries: BTreeMap<String, Digest> = BTreeMap::new();
    let mut location: BTreeMap<String, String> = BTreeMap::new(); // package id -> path in the mirror
    for &i in &resolution.selected {
        let p = &index.packages[i];
        let file = p.href.rsplit('/').next().unwrap_or("");
        let first = file.chars().next().map(|c| c.to_ascii_lowercase()).filter(char::is_ascii_alphanumeric);
        let (Some(first), true) = (first, safe_href(&p.href)) else {
            report.warnings.push(format!("{} {}: its file name {:?} is not something this mirror can store; skipped", p.name, p.evr, p.href));
            continue;
        };
        let path = format!("Packages/{first}/{file}");
        let expected = (p.checksum_type == "sha256").then(|| Digest::from_hex(&p.pkgid).ok()).flatten();
        let digest = match expected {
            Some(d) if st.has_blob(&d)? => {
                report.reused += 1;
                d
            }
            _ => {
                let url = format!("{}/{}", upstreams[source[i]].base, p.href);
                let mut body = client.get(&url, &[])?.into_reader().take(MAX_PACKAGE + 1);
                let (d, size) = st.ingest(&mut body, Expect { digest: expected, size: None })?;
                if expected.is_none() {
                    report.warnings.push(format!("{file}: its checksum is {}, which is not checked here", p.checksum_type));
                }
                report.downloaded += 1;
                report.downloaded_bytes += size;
                d
            }
        };
        entries.insert(path.clone(), digest);
        location.insert(p.pkgid.clone(), path);
    }
    report.packages = location.len();

    // 5. Metadata that lists exactly what was mirrored: the upstream
    //    entries, unchanged but for where the file is.
    let mut primary = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata xmlns=\"http://linux.duke.edu/metadata/common\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\" packages=\"{}\">\n",
        location.len()
    );
    let mut written: BTreeSet<String> = BTreeSet::new();
    for up in &upstreams {
        let reader = decompress(&up.primary.0, &up.primary.1).map_err(RpmError::Repo)?;
        for_each_package::<RpmError>(reader, |fragment| {
            // Cheap test first: is one of the wanted ids in this entry at all?
            let Some(id) = fragment.find("pkgid=\"YES\">").and_then(|at| fragment[at + 12..].split('<').next()).map(|s| s.trim().to_ascii_lowercase()) else { return Ok(()) };
            if let Some(path) = location.get(&id) {
                if written.insert(id) {
                    primary.push_str(&with_location(fragment, path).ok_or_else(|| RpmError::Repo("a package entry has no location".into()))?);
                    primary.push('\n');
                }
            }
            Ok(())
        })?;
    }
    primary.push_str("</metadata>\n");

    let mut filelists = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<filelists xmlns=\"http://linux.duke.edu/metadata/filelists\" packages=\"{}\">\n", location.len());
    let mut listed: BTreeSet<String> = BTreeSet::new();
    for up in upstreams.iter_mut() {
        let Some(entry) = up.filelists.clone() else { continue };
        let body = match up.filelists_body.take() {
            Some(b) => b,
            None => fetch_verified(client, &up.base, &entry, &mut report.warnings)?,
        };
        let reader = decompress(&entry.href, &body).map_err(RpmError::Repo)?;
        for_each_package::<RpmError>(reader, |fragment| {
            if let Some(id) = filelists_pkgid(fragment).map(str::to_ascii_lowercase) {
                if location.contains_key(&id) && listed.insert(id) {
                    filelists.push_str(fragment);
                    filelists.push('\n');
                }
            }
            Ok(())
        })?;
    }
    filelists.push_str("</filelists>\n");

    let timestamp = upstreams.iter().map(|u| u.timestamp).max().unwrap_or(0);
    let mut repomd = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<repomd xmlns=\"http://linux.duke.edu/metadata/repo\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\">\n  <revision>{timestamp}</revision>\n");
    let mut data = vec![("primary", primary)];
    if listed.len() == location.len() {
        data.push(("filelists", filelists));
    } else {
        report.warnings.push("the upstream file lists do not cover every mirrored package; the mirror is published without file lists".into());
    }
    for (kind, xml) in data {
        let packed = gzip(xml.as_bytes());
        let (sum, open_sum) = (sha256_hex(&packed), sha256_hex(xml.as_bytes()));
        let href = format!("repodata/{sum}-{kind}.xml.gz");
        repomd.push_str(&format!(
            "  <data type=\"{kind}\">\n    <checksum type=\"sha256\">{sum}</checksum>\n    <open-checksum type=\"sha256\">{open_sum}</open-checksum>\n    <location href=\"{href}\"/>\n    <timestamp>{timestamp}</timestamp>\n    <size>{}</size>\n    <open-size>{}</open-size>\n  </data>\n",
            packed.len(),
            xml.len()
        ));
        let (d, _) = st.ingest_bytes(&packed)?;
        entries.insert(href, d);
    }
    repomd.push_str("</repomd>\n");
    let (repomd_digest, _) = st.ingest_bytes(repomd.as_bytes())?;
    entries.insert("repodata/repomd.xml".into(), repomd_digest);

    let previous: BTreeMap<String, Digest> = match st.head(repo)? {
        Some(h) => st.meta().read(|t| t.entries_at(repo_row.id, h.seq))?.into_iter().map(|e| (e.path, e.digest)).collect(),
        None => BTreeMap::new(),
    };
    if let Some(home) = &cfg.sign_home {
        let key = SiteKey::open(home.clone()).map_err(|e| RpmError::Sign(e.to_string()))?;
        // A signature carries the time it was made. Unchanged metadata keeps
        // the signature it has, so that nothing changes when nothing changed.
        let unchanged = previous.get("repodata/repomd.xml") == Some(&repomd_digest);
        let signature = match (unchanged, previous.get("repodata/repomd.xml.asc")) {
            (true, Some(d)) => *d,
            _ => st.ingest_bytes(&key.detach_sign(repomd.as_bytes()).map_err(|e| RpmError::Sign(e.to_string()))?)?.0,
        };
        entries.insert("repodata/repomd.xml.asc".into(), signature);
        let (public, _) = st.ingest_bytes(&key.public_armored().map_err(|e| RpmError::Sign(e.to_string()))?)?;
        entries.insert("keys/site.asc".into(), public);
    }

    let mut writer = st.begin(repo)?;
    for (path, digest) in &entries {
        writer.put(path, EntryRole::Artifact, digest)?;
    }
    for gone in previous.keys().filter(|p| !entries.contains_key(*p)) {
        writer.remove(gone)?;
    }
    report.snapshot = writer.seal(None)?.map(|s| s.seq);
    Ok(report)
}
