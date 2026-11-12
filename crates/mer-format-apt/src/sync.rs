//! Collector-side sync of one apt suite into one repo snapshot.
use crate::release::{parse_date, render_release};
use crate::{parse_release, parse_stanzas, verify_inrelease, AptError, Index, ReleaseFile, SiteKey, Stanza};
use flate2::{read::MultiGzDecoder, write::GzEncoder, Compression, GzBuilder};
use mer_blob::Expect;
use mer_core::{validate_path, Digest, EntryRole};
use mer_fetch::Upstream;
use mer_storage::Storage;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::PathBuf;

const MAX_INRELEASE: u64 = 8 << 20;
const MAX_INDEX_UNPACKED: u64 = 2 << 30;

#[derive(Clone, Debug)]
pub struct AptConfig {
    /// Suite or codename, as it appears under `dists/` (for example `bookworm`).
    pub suite: String,
    pub components: Vec<String>,
    pub architectures: Vec<String>,
    /// Packages to mirror. Their dependencies are added automatically.
    pub packages: Vec<String>,
    pub recommends: bool,
    /// Public keys of the upstream archive (a `gpgv` keyring file).
    pub keyring: PathBuf,
    /// Accept an upstream Release whose Valid-Until has passed.
    pub ignore_valid_until: bool,
    /// Origin/Label written into this site's Release.
    pub origin: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub upstream_packages: usize,
    pub selected: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    /// Packages already in the store (not downloaded again).
    pub reused: usize,
    pub total_bytes: u64,
    /// Hard dependencies the upstream index cannot satisfy.
    pub unresolved: Vec<(String, String)>,
    /// Sealed snapshot, or `None` when nothing changed.
    pub snapshot: Option<u64>,
}

fn meta(msg: impl Into<String>) -> AptError {
    AptError::Metadata(msg.into())
}

fn simple_name(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.') && !s.starts_with('.')
}

fn gzip(data: &[u8]) -> Vec<u8> {
    // No timestamp and no file name in the header, so equal input gives equal output.
    let mut enc: GzEncoder<Vec<u8>> = GzBuilder::new().mtime(0).write(Vec::new(), Compression::new(6));
    enc.write_all(data).expect("writing to memory");
    enc.finish().expect("writing to memory")
}

fn gunzip(data: &[u8]) -> Result<Vec<u8>, AptError> {
    let mut out = Vec::new();
    MultiGzDecoder::new(data)
        .take(MAX_INDEX_UNPACKED + 1)
        .read_to_end(&mut out)
        .map_err(|e| meta(format!("index does not decompress: {e}")))?;
    if out.len() as u64 > MAX_INDEX_UNPACKED {
        return Err(meta("index is too large when decompressed"));
    }
    Ok(out)
}

/// One package file to mirror.
struct Wanted {
    filename: String,
    size: u64,
    sha256: Digest,
}

fn wanted(stanza: &Stanza) -> Result<Wanted, AptError> {
    let name = stanza.get("Package").unwrap_or("?");
    let field = |f: &str| stanza.get(f).ok_or_else(|| meta(format!("package {name} has no {f}")));
    let filename = field("Filename")?.to_string();
    if validate_path(&filename).is_err() || !filename.starts_with("pool/") {
        return Err(meta(format!("package {name} has an unacceptable Filename {filename:?}")));
    }
    Ok(Wanted {
        filename,
        size: field("Size")?.parse().map_err(|_| meta(format!("package {name}: bad Size")))?,
        sha256: Digest::from_hex(field("SHA256")?).map_err(|_| meta(format!("package {name}: bad SHA256")))?,
    })
}

/// Mirror the configured subset of an upstream suite into a new snapshot of `repo`.
///
/// Trust chain: the upstream `InRelease` signature is verified first; every
/// index must match a hash in that signed file; every package must match a
/// hash in a verified index. Anything that does not match aborts the sync and
/// leaves the repo at its previous snapshot.
pub fn sync(
    st: &Storage,
    repo: &str,
    up: &dyn Upstream,
    cfg: &AptConfig,
    site: &SiteKey,
    now_unix: i64,
) -> Result<SyncReport, AptError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "apt" {
        return Err(meta(format!("repo {repo} has format {}, not apt", repo_row.format)));
    }
    for name in std::iter::once(&cfg.suite).chain(&cfg.components).chain(&cfg.architectures) {
        if !simple_name(name) {
            return Err(meta(format!("unacceptable suite, component or architecture name {name:?}")));
        }
    }
    if cfg.components.is_empty() || cfg.architectures.is_empty() || cfg.packages.is_empty() {
        return Err(meta("at least one component, one architecture and one package are needed"));
    }
    let suite = &cfg.suite;

    // 1. The signed root of trust.
    let inrelease = up.read(&format!("dists/{suite}/InRelease"), MAX_INRELEASE)?;
    let release_text = verify_inrelease(&cfg.keyring, &inrelease)?;
    let release_text = String::from_utf8(release_text).map_err(|_| meta("Release is not UTF-8"))?;
    let upstream_release = parse_release(&release_text).map_err(meta)?;
    // A validly signed Release of a different suite must not be accepted here.
    if upstream_release.suite.as_deref() != Some(suite) && upstream_release.codename.as_deref() != Some(suite) {
        return Err(meta(format!(
            "asked for suite {suite}, upstream Release is for {:?} / {:?}",
            upstream_release.suite, upstream_release.codename
        )));
    }
    if let (Some(until), false) = (&upstream_release.valid_until, cfg.ignore_valid_until) {
        let t = parse_date(until).ok_or_else(|| meta(format!("cannot read Valid-Until {until:?}")))?;
        if t < now_unix {
            return Err(meta(format!(
                "upstream Release expired on {until}; the mirror may be stale or replayed"
            )));
        }
    }

    let mut report = SyncReport::default();
    // Entries of the new snapshot: path -> (role, digest).
    let mut entries: BTreeMap<String, (EntryRole, Digest)> = BTreeMap::new();
    let mut release_files: Vec<ReleaseFile> = Vec::new();
    let mut selected_names: BTreeSet<String> = BTreeSet::new();

    for arch in &cfg.architectures {
        // 2. Verified indices of every component, merged for dependency resolution.
        let mut stanzas: Vec<Stanza> = Vec::new();
        let mut component_of: BTreeMap<String, &str> = BTreeMap::new();
        for comp in &cfg.components {
            let rel = format!("{comp}/binary-{arch}/Packages.gz");
            let listed = upstream_release
                .file(&rel)
                .ok_or_else(|| meta(format!("upstream Release does not list {rel}")))?;
            let packed = up.read(&format!("dists/{suite}/{rel}"), listed.size)?;
            if packed.len() as u64 != listed.size || Digest::of(&packed) != listed.sha256 {
                return Err(meta(format!("{rel} does not match the signed Release")));
            }
            let text = String::from_utf8(gunzip(&packed)?).map_err(|_| meta(format!("{rel} is not UTF-8")))?;
            for s in parse_stanzas(&text).map_err(|e| meta(format!("{rel}: {e}")))? {
                if let Some(f) = s.get("Filename") {
                    component_of.insert(f.to_string(), comp.as_str());
                }
                stanzas.push(s);
            }
        }
        let index = Index::new(stanzas)?;
        report.upstream_packages += index.len();

        // 3. Wanted packages plus what they depend on.
        let closure = index.closure(&cfg.packages, cfg.recommends)?;
        for u in closure.unresolved {
            if !report.unresolved.contains(&u) {
                report.unresolved.push(u);
            }
        }

        // 4. Fetch what the store does not hold yet, against the signed hash.
        let mut by_component: BTreeMap<&str, Vec<&Stanza>> = BTreeMap::new();
        for name in &closure.selected {
            let stanza = index.get(name).expect("selected from this index");
            let w = wanted(stanza)?;
            if !entries.contains_key(&w.filename) {
                if st.has_blob(&w.sha256)? {
                    report.reused += 1;
                } else {
                    let mut body = up.open(&w.filename)?.take(w.size + 1);
                    st.ingest(&mut body, Expect::exact(w.sha256, w.size))?;
                    report.downloaded += 1;
                    report.downloaded_bytes += w.size;
                }
                report.total_bytes += w.size;
                entries.insert(w.filename.clone(), (EntryRole::Artifact, w.sha256));
            }
            let comp = component_of.get(&w.filename).copied().unwrap_or(cfg.components[0].as_str());
            by_component.entry(comp).or_default().push(stanza);
            selected_names.insert(name.clone());
        }

        // 5. Our own indices: the upstream entries of the selected packages, verbatim.
        for comp in &cfg.components {
            let text: String = by_component
                .get(comp.as_str())
                .map(|list| list.iter().map(|s| s.raw.as_str()).collect::<Vec<_>>().join("\n"))
                .unwrap_or_default();
            let dir = format!("{comp}/binary-{arch}");
            for (name, bytes) in [("Packages", text.clone().into_bytes()), ("Packages.gz", gzip(text.as_bytes()))] {
                let (digest, size) = st.ingest_bytes(&bytes)?;
                entries.insert(format!("dists/{suite}/{dir}/{name}"), (EntryRole::Artifact, digest));
                // by-hash copies keep clients consistent while the repo is being switched.
                entries.insert(format!("dists/{suite}/{dir}/by-hash/SHA256/{digest}"), (EntryRole::Artifact, digest));
                release_files.push(ReleaseFile { path: format!("{dir}/{name}"), size, sha256: digest });
            }
        }
    }
    report.selected = selected_names.len();

    // 6. Release, signed with the site key. If the content is identical to the
    //    current snapshot, the existing signatures are kept, so an unchanged
    //    upstream does not produce a new snapshot.
    let release = render_release(
        &cfg.origin,
        &upstream_release,
        suite,
        &cfg.architectures,
        &cfg.components,
        &release_files,
    );
    let (release_digest, _) = st.ingest_bytes(release.as_bytes())?;
    let release_path = format!("dists/{suite}/Release");
    let head = st.head(repo)?;
    let previous: BTreeMap<String, Digest> = match &head {
        Some(h) => st.meta().read(|t| t.entries_at(repo_row.id, h.seq))?.into_iter().map(|e| (e.path, e.digest)).collect(),
        None => BTreeMap::new(),
    };
    let signature_paths = [format!("dists/{suite}/InRelease"), format!("dists/{suite}/Release.gpg")];
    let reuse = previous.get(&release_path) == Some(&release_digest)
        && signature_paths.iter().all(|p| previous.contains_key(p));
    entries.insert(release_path, (EntryRole::Artifact, release_digest));
    if reuse {
        for p in signature_paths {
            let d = previous[&p];
            entries.insert(p, (EntryRole::Artifact, d));
        }
    } else {
        let [inrelease_path, detached_path] = signature_paths;
        let (d, _) = st.ingest_bytes(&site.clearsign(release.as_bytes())?)?;
        entries.insert(inrelease_path, (EntryRole::Artifact, d));
        let (d, _) = st.ingest_bytes(&site.detach_sign(release.as_bytes())?)?;
        entries.insert(detached_path, (EntryRole::Artifact, d));
    }
    let (d, _) = st.ingest_bytes(&site.public_armored()?)?;
    entries.insert("keys/site.asc".into(), (EntryRole::Artifact, d));
    // What upstream signed, kept as evidence of where this snapshot came from.
    // Upstream re-signs the same content periodically; that alone is not a change.
    let evidence_path = format!("evidence/{suite}/InRelease");
    let d = match previous.get(&evidence_path) {
        Some(old) if reuse => *old,
        _ => st.ingest_bytes(&inrelease)?.0,
    };
    entries.insert(evidence_path, (EntryRole::Evidence, d));

    // 7. One snapshot: indices, signatures and packages change together.
    let mut writer = st.begin(repo)?;
    for (path, (role, digest)) in &entries {
        writer.put(path, *role, digest)?;
    }
    for gone in previous.keys().filter(|p| !entries.contains_key(*p)) {
        writer.remove(gone)?;
    }
    report.snapshot = writer.seal(None)?.map(|s| s.seq);
    Ok(report)
}
