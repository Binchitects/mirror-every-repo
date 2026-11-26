//! Collector-side sync of Alpine repositories into one repo snapshot.
use crate::index::{check_package, read_index, rsa_key_from_pem, valid_key_name, valid_name, Entry, Keys};
use crate::resolve::{resolve, Pool};
use mer_core::{Digest, EntryRole};
use mer_fetch::{host_of, FetchError, HttpClient};
use mer_storage::{Storage, StorageError};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::PathBuf;

const MAX_INDEX: u64 = 256 << 20;
const MAX_APK: u64 = 4 << 30;

#[derive(Debug, thiserror::Error)]
pub enum ApkError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("upstream repository: {0}")]
    Upstream(String),
    #[error("{0}")]
    Config(String),
}

impl From<mer_meta::MetaError> for ApkError {
    fn from(e: mer_meta::MetaError) -> Self {
        ApkError::Storage(e.into())
    }
}

#[derive(Clone, Debug)]
pub struct ApkConfig {
    /// Repository URLs as in `/etc/apk/repositories`, for example
    /// `https://dl-cdn.alpinelinux.org/alpine/v3.24/main`. Several are
    /// mirrored together (community needs main); each keeps its own path.
    pub upstreams: Vec<String>,
    /// Package names to mirror, with what they need.
    pub packages: Vec<String>,
    /// Architectures, for example `x86_64` and `aarch64`.
    pub arches: Vec<String>,
    /// A directory of trusted keys (`*.rsa.pub`), as `/etc/apk/keys`. Without
    /// it, indexes are trusted on TLS alone.
    pub keys: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApkReport {
    pub packages: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub reused: usize,
    pub warnings: Vec<String>,
    pub snapshot: Option<u64>,
}

fn up(msg: impl Into<String>) -> ApkError {
    ApkError::Upstream(msg.into())
}

/// The keys of a directory, by file name, as apk finds them.
pub fn load_keys(dir: &std::path::Path) -> Result<Keys, ApkError> {
    let mut keys = Keys::new();
    let rd = std::fs::read_dir(dir).map_err(|e| ApkError::Config(format!("keys directory {}: {e}", dir.display())))?;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !valid_key_name(&name) || !entry.path().is_file() {
            continue;
        }
        let pem = std::fs::read_to_string(entry.path()).map_err(|e| ApkError::Config(format!("{}: {e}", entry.path().display())))?;
        let key = rsa_key_from_pem(&pem).map_err(|e| ApkError::Config(format!("{}: {e}", entry.path().display())))?;
        keys.insert(name, key);
    }
    if keys.is_empty() {
        return Err(ApkError::Config(format!("no RSA public keys in {}", dir.display())));
    }
    Ok(keys)
}

/// The path of an upstream repository in the snapshot: its last URL
/// segment (`main`, `community`, `testing`).
fn repo_dir(url: &str) -> Option<String> {
    let last = url.trim_end_matches('/').rsplit('/').next()?;
    valid_name(last).then(|| last.to_string())
}

/// Mirror the configured packages and what they need into a new snapshot of `repo`.
///
/// Each repository's `APKINDEX.tar.gz` is checked against the trusted keys
/// and kept unchanged, so clients check it with the keys they already have.
/// Every package is checked against it: its size, the SHA-1 of its control
/// section, and the SHA-256 of its contents that the control section states.
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &ApkConfig) -> Result<ApkReport, ApkError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "apk" {
        return Err(ApkError::Config(format!("repo {repo} has format {}, not apk", repo_row.format)));
    }
    if cfg.upstreams.is_empty() || cfg.packages.is_empty() || cfg.arches.is_empty() {
        return Err(ApkError::Config("name at least one upstream, package and architecture".into()));
    }
    for a in &cfg.arches {
        if !valid_name(a) {
            return Err(ApkError::Config(format!("{a:?} is not an architecture")));
        }
    }
    let keys = cfg.keys.as_deref().map(load_keys).transpose()?;
    let mut warnings = Vec::new();
    let mut dirs: Vec<(String, String)> = Vec::new();
    for u in &cfg.upstreams {
        let base = u.trim_end_matches('/').to_string();
        if host_of(&base).is_none() {
            return Err(ApkError::Config(format!("{u:?} is not an http(s) URL")));
        }
        let dir = repo_dir(&base).ok_or_else(|| ApkError::Config(format!("{u:?} does not end in a repository name such as main")))?;
        if dirs.iter().any(|(d, _)| *d == dir) {
            return Err(ApkError::Config(format!("two upstreams end in {dir}; each needs a name of its own")));
        }
        dirs.push((dir, base));
    }
    if keys.is_none() {
        warnings.push("indexes are trusted on TLS alone (no --keys given)".to_string());
    }

    let previous: BTreeMap<String, Digest> = match st.head(repo)? {
        Some(h) => st.meta().read(|t| t.entries_at(repo_row.id, h.seq))?.into_iter().map(|e| (e.path, e.digest)).collect(),
        None => BTreeMap::new(),
    };
    let mut report = ApkReport::default();
    let mut files: BTreeMap<String, Digest> = BTreeMap::new();
    for arch in &cfg.arches {
        let mut pkgs: Vec<(usize, Entry)> = Vec::new();
        for (n, (dir, base)) in dirs.iter().enumerate() {
            let url = format!("{base}/{arch}/APKINDEX.tar.gz");
            let raw = client.get_bytes(&url, &[], MAX_INDEX)?;
            let index = read_index(&raw, keys.as_ref()).map_err(|e| up(format!("{url}: {e}")))?;
            if index.skipped > 0 {
                warnings.push(format!("{url}: {} entries are not usable packages and were left out", index.skipped));
            }
            let (d, _) = st.ingest_bytes(&raw)?;
            files.insert(format!("{dir}/{arch}/APKINDEX.tar.gz"), d);
            pkgs.extend(index.entries.into_iter().map(|e| (n, e)));
        }
        let pool = Pool::new(&pkgs);
        let sel = resolve(&pool, &cfg.packages).map_err(|e| up(format!("{arch}: {e}")))?;
        warnings.extend(sel.warnings.iter().map(|w| format!("{arch}: {w}")));
        report.packages += sel.selected.len();
        for &i in &sel.selected {
            let (n, e) = &pkgs[i];
            let (dir, base) = &dirs[*n];
            let path = format!("{dir}/{arch}/{}", e.file_name());
            // The same file as last time, checked again against this index.
            if let Some(d) = previous.get(&path) {
                let mut body = Vec::new();
                if st.open_blob(d, None).is_ok_and(|mut r| r.read_to_end(&mut body).is_ok()) && check_package(&body, e).is_ok() {
                    files.insert(path, *d);
                    report.reused += 1;
                    continue;
                }
            }
            let url = format!("{base}/{arch}/{}", e.file_name());
            let body = client.get_bytes(&url, &[], MAX_APK.min(e.size))?;
            check_package(&body, e).map_err(|why| up(format!("{url} {why}")))?;
            let (d, size) = st.ingest_bytes(&body)?;
            report.downloaded += 1;
            report.downloaded_bytes += size;
            files.insert(path, d);
        }
    }

    let mut writer = st.begin(repo)?;
    for (path, digest) in &files {
        writer.put(path, EntryRole::Artifact, digest)?;
    }
    let keep: BTreeSet<&String> = files.keys().collect();
    for gone in previous.keys().filter(|p| !keep.contains(p)) {
        writer.remove(gone)?;
    }
    report.snapshot = writer.seal(None)?.map(|s| s.seq);
    warnings.sort();
    warnings.dedup();
    report.warnings = warnings;
    Ok(report)
}
