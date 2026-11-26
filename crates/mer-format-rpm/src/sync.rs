//! Collector-side sync of RPM repositories (yum/dnf) into one repo snapshot.
use crate::meta::{parse_filelists, parse_primary, parse_repomd, parse_rich, DataRef, Package, Rich};
use mer_core::{Digest, EntryRole};
use mer_fetch::{host_of, FetchError, HttpClient};
use mer_storage::{Storage, StorageError};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;

const MAX_REPOMD: u64 = 16 << 20;
const MAX_METADATA: u64 = 2 << 30;
const MAX_RPM: u64 = 8 << 30;

#[derive(Debug, thiserror::Error)]
pub enum RpmError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("upstream repository: {0}")]
    Upstream(String),
    #[error("signature: {0}")]
    Signature(String),
    #[error("{0}")]
    Config(String),
}

impl From<mer_meta::MetaError> for RpmError {
    fn from(e: mer_meta::MetaError) -> Self {
        RpmError::Storage(e.into())
    }
}

#[derive(Clone, Debug)]
pub struct RpmConfig {
    /// Repository base URLs, the directories holding `repodata/` (several are
    /// merged into one repo, as BaseOS and AppStream need each other).
    pub upstreams: Vec<String>,
    /// Package names to mirror, with what they require.
    pub packages: Vec<String>,
    /// Architectures to keep, for example `x86_64` and `noarch`.
    pub arches: Vec<String>,
    /// OpenPGP keyring (binary or armored) that `repomd.xml.asc` must verify
    /// against. Without it, metadata is trusted on TLS alone.
    pub keyring: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RpmReport {
    pub packages: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub reused: usize,
    pub warnings: Vec<String>,
    pub snapshot: Option<u64>,
}

fn up(msg: impl Into<String>) -> RpmError {
    RpmError::Upstream(msg.into())
}

fn sha256_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

/// Does `body` hash to `want` under `algo`? Fedora states SHA-512, most others SHA-256.
fn matches(algo: &str, want: &str, body: &[u8]) -> Option<bool> {
    let got = match algo {
        "sha256" => sha256_hex(body),
        "sha512" => hex::encode(sha2::Sha512::digest(body)),
        _ => return None,
    };
    Some(got == want.to_ascii_lowercase())
}

/// A package location: relative, `/`-separated, no `.`/`..`, ends in `.rpm`.
fn valid_href(h: &str) -> bool {
    h.ends_with(".rpm") && !h.starts_with("repodata/") && mer_core::validate_path(h).is_ok() && h.bytes().all(|b| b.is_ascii_graphic() && b != b'\\' && b != b'%' && b != b'?' && b != b'#')
}

/// Check a detached signature with `gpgv`.
fn verify_detached(keyring: &std::path::Path, data: &[u8], sig: &[u8]) -> Result<(), RpmError> {
    let dir = tempfile::tempdir().map_err(|e| RpmError::Signature(e.to_string()))?;
    let raw = std::fs::read(keyring).map_err(|e| RpmError::Signature(format!("keyring {}: {e}", keyring.display())))?;
    let ring = dir.path().join("keyring.gpg");
    if raw.starts_with(b"-----BEGIN") {
        let out = Command::new("gpg")
            .args(["--batch", "--homedir"])
            .arg(dir.path())
            .args(["--dearmor", "--output"])
            .arg(&ring)
            .arg(keyring)
            .output()
            .map_err(|e| RpmError::Signature(format!("running gpg: {e}")))?;
        if !out.status.success() {
            return Err(RpmError::Signature(format!("keyring {} is not an OpenPGP key", keyring.display())));
        }
    } else {
        std::fs::write(&ring, &raw).map_err(|e| RpmError::Signature(e.to_string()))?;
    }
    let (d, s) = (dir.path().join("repomd.xml"), dir.path().join("repomd.xml.asc"));
    std::fs::write(&d, data).and_then(|_| std::fs::write(&s, sig)).map_err(|e| RpmError::Signature(e.to_string()))?;
    let out = Command::new("gpgv").arg("--keyring").arg(&ring).arg(&s).arg(&d).output().map_err(|e| RpmError::Signature(format!("running gpgv: {e}")))?;
    if !out.status.success() {
        return Err(RpmError::Signature(format!("repomd.xml does not verify against {}: {}", keyring.display(), String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(())
}

/// Fetch one metadata file named by repomd, check it, and unpack it.
fn metadata(client: &HttpClient, base: &str, d: &DataRef) -> Result<String, RpmError> {
    let url = format!("{base}/{}", d.href);
    if !d.href.starts_with("repodata/") || mer_core::validate_path(&d.href).is_err() {
        return Err(up(format!("repomd.xml names {:?}", d.href)));
    }
    let body = client.get(&url, &[])?.bytes(&url, MAX_METADATA)?;
    if matches(&d.checksum.0, &d.checksum.1, &body) != Some(true) {
        return Err(up(format!("{url}: does not match its {} checksum in repomd.xml (SHA-256 and SHA-512 are accepted)", d.checksum.0)));
    }
    let limit = d.open_size.unwrap_or(MAX_METADATA).min(MAX_METADATA);
    let mut out = Vec::new();
    let reader: Box<dyn Read + '_> = if d.href.ends_with(".gz") {
        Box::new(flate2::read::MultiGzDecoder::new(&body[..]))
    } else if d.href.ends_with(".zst") {
        Box::new(ruzstd::decoding::StreamingDecoder::new(&body[..]).map_err(|e| up(format!("{url}: {e}")))?)
    } else if d.href.ends_with(".xz") {
        let mut plain = Vec::new();
        lzma_rs::xz_decompress(&mut &body[..], &mut plain).map_err(|e| up(format!("{url}: {e:?}")))?;
        Box::new(std::io::Cursor::new(plain))
    } else if d.href.ends_with(".xml") {
        Box::new(&body[..])
    } else {
        return Err(up(format!("{url}: compression is not supported")));
    };
    reader.take(limit + 1).read_to_end(&mut out).map_err(|e| up(format!("{url}: {e}")))?;
    if out.len() as u64 > limit {
        return Err(up(format!("{url}: unpacks to more than its declared size")));
    }
    if let Some((algo, sum)) = &d.open_checksum {
        if matches(algo, sum, &out) == Some(false) {
            return Err(up(format!("{url}: unpacked content does not match its checksum")));
        }
    }
    String::from_utf8(out).map_err(|_| up(format!("{url}: not UTF-8")))
}

struct Upstream {
    base: String,
    filelists: String,
    /// pkgid -> byte range in `filelists`
    ranges: HashMap<String, (usize, usize)>,
    revision: u64,
}

struct Pool {
    pkgs: Vec<(usize, Package)>,
    provides: HashMap<String, Vec<usize>>,
}

impl Pool {
    fn providers(&self, cap: &str) -> &[usize] {
        self.provides.get(cap).map(Vec::as_slice).unwrap_or(&[])
    }
}

struct Closure<'a> {
    pool: &'a Pool,
    selected: BTreeSet<usize>,
    queue: VecDeque<usize>,
    conditionals: Vec<Rich>,
    warnings: Vec<String>,
}

impl Closure<'_> {
    fn satisfied(&self, cap: &str) -> bool {
        self.pool.providers(cap).iter().any(|i| self.selected.contains(i))
    }

    fn select(&mut self, i: usize) {
        if self.selected.insert(i) {
            self.queue.push_back(i);
        }
    }

    /// Make `cap` available: nothing if a selected package provides it,
    /// otherwise its best provider.
    fn need(&mut self, cap: &str, who: &str) -> bool {
        if cap.starts_with("rpmlib(") || self.satisfied(cap) {
            return true;
        }
        let provs = self.pool.providers(cap);
        // The package of that name, else the shortest name, as dnf leans.
        let best = provs
            .iter()
            .copied()
            .min_by_key(|&i| {
                let p = &self.pool.pkgs[i].1;
                (p.name != cap, p.name.len(), p.name.clone())
            });
        match best {
            Some(i) => {
                self.select(i);
                true
            }
            None => {
                self.warnings.push(format!("{who} requires {cap}, which no mirrored repository provides"));
                false
            }
        }
    }

    fn can(&self, r: &Rich) -> bool {
        match r {
            Rich::Cap(c) => c.starts_with("rpmlib(") || !self.pool.providers(c).is_empty(),
            Rich::And(v) => v.iter().all(|x| self.can(x)),
            Rich::Or(v) => v.iter().any(|x| self.can(x)),
            Rich::If(t, c, e) | Rich::Unless(t, c, e) => self.can(t) || self.can(c) || e.as_ref().is_some_and(|x| self.can(x)),
        }
    }

    fn holds(&self, r: &Rich) -> bool {
        match r {
            Rich::Cap(c) => self.satisfied(c),
            Rich::And(v) => v.iter().all(|x| self.holds(x)),
            Rich::Or(v) => v.iter().any(|x| self.holds(x)),
            Rich::If(t, c, e) => if self.holds(c) { self.holds(t) } else { e.as_ref().is_none_or(|x| self.holds(x)) },
            Rich::Unless(t, c, e) => if !self.holds(c) { self.holds(t) } else { e.as_ref().is_none_or(|x| self.holds(x)) },
        }
    }

    /// Satisfy a rich dependency as far as the current selection decides it.
    fn need_rich(&mut self, r: &Rich, who: &str) {
        match r {
            Rich::Cap(c) => {
                self.need(c, who);
            }
            Rich::And(v) => v.iter().for_each(|x| self.need_rich(x, who)),
            Rich::Or(v) => {
                if !v.iter().any(|x| self.holds(x)) {
                    match v.iter().find(|x| self.can(x)) {
                        Some(x) => self.need_rich(&x.clone(), who),
                        None => self.warnings.push(format!("{who} requires one of a set that no mirrored repository provides")),
                    }
                }
            }
            Rich::If(t, c, e) => {
                if self.holds(c) {
                    self.need_rich(t, who)
                } else if let Some(e) = e {
                    self.need_rich(e, who)
                }
                self.conditionals.push(r.clone());
            }
            Rich::Unless(t, c, e) => {
                if !self.holds(c) {
                    self.need_rich(t, who)
                } else if let Some(e) = e {
                    self.need_rich(e, who)
                }
            }
        }
    }

    fn run(&mut self) {
        loop {
            while let Some(i) = self.queue.pop_front() {
                let p = &self.pool.pkgs[i].1;
                let who = format!("{}-{}.{}", p.name, p.evr, p.arch);
                for req in p.requires.clone() {
                    if req.starts_with('(') {
                        match parse_rich(&req) {
                            Some(r) => self.need_rich(&r, &who),
                            None => self.warnings.push(format!("{who}: dependency {req:?} is not understood; ignored")),
                        }
                    } else {
                        self.need(&req, &who);
                    }
                }
            }
            // Conditions may have become true since: `(a if b)` once b is in.
            let pending = std::mem::take(&mut self.conditionals);
            for r in &pending {
                self.need_rich(r, "a conditional dependency");
            }
            if self.queue.is_empty() {
                break;
            }
        }
    }
}

fn gz(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::GzBuilder::new().mtime(0).write(Vec::new(), flate2::Compression::default());
    e.write_all(data).expect("in memory");
    e.finish().expect("in memory")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Mirror the configured packages and what they require into a new snapshot of `repo`.
///
/// Every metadata file is checked against `repomd.xml`, which is checked
/// against the configured keyring when one is given, and every package
/// against the SHA-256 in `primary.xml`. Package signatures are left for the
/// client: dnf checks them with the distribution key, as for any repository.
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &RpmConfig) -> Result<RpmReport, RpmError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "rpm" {
        return Err(RpmError::Config(format!("repo {repo} has format {}, not rpm", repo_row.format)));
    }
    if cfg.upstreams.is_empty() || cfg.packages.is_empty() || cfg.arches.is_empty() {
        return Err(RpmError::Config("name at least one upstream, package and architecture".into()));
    }
    let mut warnings = Vec::new();
    let mut ups = Vec::new();
    let mut pkgs: Vec<(usize, Package)> = Vec::new();
    for (n, u) in cfg.upstreams.iter().enumerate() {
        let base = u.trim_end_matches('/').to_string();
        if host_of(&base).is_none() {
            return Err(RpmError::Config(format!("{u:?} is not an http(s) URL")));
        }
        let url = format!("{base}/repodata/repomd.xml");
        let raw = client.get(&url, &[])?.bytes(&url, MAX_REPOMD)?;
        match &cfg.keyring {
            Some(k) => {
                let sig_url = format!("{url}.asc");
                let sig = client.get(&sig_url, &[])?.bytes(&sig_url, 1 << 20)?;
                verify_detached(k, &raw, &sig)?;
            }
            None => warnings.push(format!("{base}: repomd.xml is trusted on TLS alone (no keyring given)")),
        }
        let md = parse_repomd(&String::from_utf8_lossy(&raw)).map_err(up)?;
        let find = |kind: &str| md.data.iter().find(|d| d.kind == kind).cloned();
        let primary = find("primary").ok_or_else(|| up(format!("{base}: repomd.xml lists no primary metadata")))?;
        let parsed = parse_primary(&metadata(client, &base, &primary)?).map_err(up)?;
        let filelists = match find("filelists") {
            Some(d) => metadata(client, &base, &d)?,
            None => String::new(),
        };
        pkgs.extend(parsed.into_iter().map(|p| (n, p)));
        ups.push(Upstream { base, filelists, ranges: HashMap::new(), revision: md.revision.unwrap_or(0) });
    }

    // Keep the newest of each name.arch among the wanted architectures.
    pkgs.retain(|(_, p)| cfg.arches.iter().any(|a| a == &p.arch));
    let mut newest: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (i, (_, p)) in pkgs.iter().enumerate() {
        let key = (p.name.clone(), p.arch.clone());
        match newest.get(&key) {
            Some(&j) if pkgs[j].1.evr >= p.evr => {}
            _ => {
                newest.insert(key, i);
            }
        }
    }
    let keep: BTreeSet<usize> = newest.values().copied().collect();
    let pkgs: Vec<(usize, Package)> = pkgs.into_iter().enumerate().filter(|(i, _)| keep.contains(i)).map(|(_, p)| p).collect();

    // Capabilities: provides, plus files (primary lists the common ones,
    // filelists the rest, which is read only for paths something requires).
    let mut provides: HashMap<String, Vec<usize>> = HashMap::new();
    let by_id: HashMap<(usize, String), usize> = pkgs.iter().enumerate().map(|(i, (u, p))| ((*u, p.checksum.1.clone()), i)).collect();
    let wanted_files: BTreeSet<String> = pkgs.iter().flat_map(|(_, p)| p.requires.iter()).filter(|r| r.starts_with('/')).cloned().collect();
    for (i, (_, p)) in pkgs.iter().enumerate() {
        for c in p.provides.iter().chain(&p.files) {
            provides.entry(c.clone()).or_default().push(i);
        }
    }
    for (n, u) in ups.iter_mut().enumerate() {
        let text = std::mem::take(&mut u.filelists);
        parse_filelists(&text, |id, range, files| {
            u.ranges.insert(id.to_string(), range);
            if let Some(&i) = by_id.get(&(n, id.to_string())) {
                for f in files.into_iter().filter(|f| wanted_files.contains(f)) {
                    let v = provides.entry(f).or_default();
                    if !v.contains(&i) {
                        v.push(i);
                    }
                }
            }
        })
        .map_err(up)?;
        u.filelists = text;
    }
    let pool = Pool { pkgs, provides };

    // Closure.
    let mut c = Closure { pool: &pool, selected: BTreeSet::new(), queue: VecDeque::new(), conditionals: vec![], warnings: vec![] };
    for name in &cfg.packages {
        let found: Vec<usize> = (0..pool.pkgs.len()).filter(|&i| &pool.pkgs[i].1.name == name).collect();
        if found.is_empty() {
            return Err(up(format!("package {name} is in none of the repositories")));
        }
        found.into_iter().for_each(|i| c.select(i));
    }
    c.run();
    warnings.extend(std::mem::take(&mut c.warnings));
    let selected = c.selected;

    // Download, and lay out a repository of the subset.
    let previous: BTreeMap<String, Digest> = match st.head(repo)? {
        Some(h) => st.meta().read(|t| t.entries_at(repo_row.id, h.seq))?.into_iter().map(|e| (e.path, e.digest)).collect(),
        None => BTreeMap::new(),
    };
    let mut report = RpmReport { packages: selected.len(), ..Default::default() };
    let mut files: BTreeMap<String, Digest> = BTreeMap::new();
    let (mut primary, mut filelists, mut other) = (String::new(), String::new(), String::new());
    for &i in &selected {
        let (u, p) = &pool.pkgs[i];
        if !valid_href(&p.href) {
            return Err(up(format!("{}: package location {:?} is not acceptable", p.name, p.href)));
        }
        if p.checksum.0 != "sha256" {
            return Err(up(format!("{}: only SHA-256 package checksums are accepted, not {}", p.name, p.checksum.0)));
        }
        let digest = Digest::from_hex(&p.checksum.1.to_ascii_lowercase()).map_err(|_| up(format!("{}: bad checksum", p.name)))?;
        if let Some(d) = files.get(&p.href) {
            if *d != digest {
                return Err(up(format!("{}: two repositories hold different files at {}", p.name, p.href)));
            }
            continue;
        }
        if st.has_blob(&digest)? {
            report.reused += 1;
        } else {
            let url = format!("{}/{}", ups[*u].base, p.href);
            let mut body = client.get(&url, &[])?.into_reader().take(MAX_RPM + 1);
            let (_, size) = st.ingest(&mut body, mer_blob::Expect { digest: Some(digest), size: Some(p.size).filter(|s| *s > 0) }).map_err(|e| match e {
                StorageError::Store(mer_blob::StoreError::DigestMismatch { .. } | mer_blob::StoreError::SizeMismatch { .. }) => {
                    up(format!("{}: the download does not match primary.xml", p.href))
                }
                other => other.into(),
            })?;
            report.downloaded += 1;
            report.downloaded_bytes += size;
        }
        files.insert(p.href.clone(), digest);
        primary.push_str(&p.raw);
        primary.push('\n');
        if let Some(&(s, e)) = ups[*u].ranges.get(&p.checksum.1) {
            filelists.push_str(ups[*u].filelists[s..e].trim());
            filelists.push('\n');
        }
        other.push_str(&format!(
            "<package pkgid=\"{}\" name=\"{}\" arch=\"{}\"><version epoch=\"{}\" ver=\"{}\" rel=\"{}\"/></package>\n",
            xml_escape(&p.checksum.1),
            xml_escape(&p.name),
            xml_escape(&p.arch),
            p.evr.epoch,
            xml_escape(&p.evr.version),
            xml_escape(&p.evr.release)
        ));
    }
    let n = files.len();
    let docs = [
        ("primary", format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata xmlns=\"http://linux.duke.edu/metadata/common\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\" packages=\"{n}\">\n{primary}</metadata>\n")),
        ("filelists", format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<filelists xmlns=\"http://linux.duke.edu/metadata/filelists\" packages=\"{n}\">\n{filelists}</filelists>\n")),
        ("other", format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<otherdata xmlns=\"http://linux.duke.edu/metadata/other\" packages=\"{n}\">\n{other}</otherdata>\n")),
    ];
    // Deterministic, so an unchanged selection makes no new snapshot.
    let revision = ups.iter().map(|u| u.revision).max().unwrap_or(0);
    let mut repomd = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<repomd xmlns=\"http://linux.duke.edu/metadata/repo\" xmlns:rpm=\"http://linux.duke.edu/metadata/rpm\">\n  <revision>{revision}</revision>\n");
    for (kind, xml) in &docs {
        let packed = gz(xml.as_bytes());
        let sum = sha256_hex(&packed);
        let href = format!("repodata/{sum}-{kind}.xml.gz");
        repomd.push_str(&format!(
            "  <data type=\"{kind}\">\n    <checksum type=\"sha256\">{sum}</checksum>\n    <open-checksum type=\"sha256\">{}</open-checksum>\n    <location href=\"{href}\"/>\n    <timestamp>{revision}</timestamp>\n    <size>{}</size>\n    <open-size>{}</open-size>\n  </data>\n",
            sha256_hex(xml.as_bytes()),
            packed.len(),
            xml.len()
        ));
        let (d, _) = st.ingest_bytes(&packed)?;
        files.insert(href, d);
    }
    repomd.push_str("</repomd>\n");
    let (d, _) = st.ingest_bytes(repomd.as_bytes())?;
    files.insert("repodata/repomd.xml".into(), d);

    let mut writer = st.begin(repo)?;
    for (path, digest) in &files {
        writer.put(path, EntryRole::Artifact, digest)?;
    }
    for gone in previous.keys().filter(|p| !files.contains_key(*p)) {
        writer.remove(gone)?;
    }
    report.snapshot = writer.seal(None)?.map(|s| s.seq);
    warnings.sort();
    warnings.dedup();
    report.warnings = warnings;
    Ok(report)
}
