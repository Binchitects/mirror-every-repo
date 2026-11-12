//! Go modules (the GOPROXY protocol, as proxy.golang.org serves it).
//!
//! For each module asked for, the collector builds the same module graph the
//! `go` command would build for a project that requires it, picks the same
//! versions (minimal version selection), and mirrors:
//!
//! * the `go.mod` and `.info` of every module version the graph is read from,
//! * the source archive of every selected version.
//!
//! Each file is checked against the Go checksum database before it is kept,
//! so clients inside the air gap can run with `GOSUMDB=off`: the check they
//! cannot make was made here.
mod gomod;
mod sum;
#[cfg(test)]
mod tests;

pub use gomod::{parse_gomod, GoMod};
pub use sum::{h1_gomod, h1_zip};

use mer_core::{Digest, EntryRole};
use mer_fetch::{host_of, FetchError, HttpClient};
use mer_storage::{Storage, StorageError};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const MAX_SMALL: u64 = 16 << 20;
const MAX_ZIP: u64 = 600 << 20;
const MAX_NODES: usize = 200_000;

#[derive(Debug, thiserror::Error)]
pub enum GoError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("upstream proxy: {0}")]
    Proxy(String),
    #[error("{0}")]
    Config(String),
    #[error("module {module}: {reason}")]
    NotFound { module: String, reason: String },
    #[error("{what} does not match the Go checksum database: it says {expected}, the proxy sent {actual}")]
    Checksum { what: String, expected: String, actual: String },
}

impl From<mer_meta::MetaError> for GoError {
    fn from(e: mer_meta::MetaError) -> Self {
        GoError::Storage(e.into())
    }
}

#[derive(Clone, Debug)]
pub struct GoConfig {
    /// The module proxy, for example `https://proxy.golang.org`.
    pub proxy_url: String,
    /// Modules to mirror: `github.com/gin-gonic/gin` (latest release) or
    /// `github.com/gin-gonic/gin@v1.10.0`.
    pub modules: Vec<String>,
    /// Contents of go.mod files of projects inside the air gap: everything
    /// they require is mirrored at the versions they name.
    pub gomods: Vec<String>,
    /// The checksum database, for example `https://sum.golang.org`.
    /// `None`: files are kept as the proxy sent them.
    pub sumdb_url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoReport {
    /// Distinct modules with at least one source archive.
    pub modules: usize,
    /// Source archives mirrored.
    pub archives: usize,
    /// Module versions whose go.mod is mirrored (archives included).
    pub gomods: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub reused: usize,
    /// Files compared with the checksum database in this run.
    pub verified: usize,
    pub warnings: Vec<String>,
    pub snapshot: Option<u64>,
}

type ModVer = (String, String);

/// The proxy protocol writes an upper-case letter as `!` and its lower-case form.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('!');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

pub fn valid_module(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 256
        && path.split('/').all(|seg| {
            !seg.is_empty() && !seg.starts_with('.') && !seg.ends_with('.') && seg.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
        })
}

/// A version as Go orders it: semantic, with `+incompatible` and the like ignored.
fn version_key(v: &str) -> Option<semver::Version> {
    let mut parsed = semver::Version::parse(v.strip_prefix('v')?).ok()?;
    parsed.build = semver::BuildMetadata::EMPTY;
    Some(parsed)
}

pub fn valid_version(v: &str) -> bool {
    v.len() <= 128 && version_key(v).is_some() && v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
}

struct Sync<'a> {
    st: &'a Storage,
    client: &'a HttpClient,
    proxy: String,
    sumdb: Option<String>,
    /// Files of the newest snapshot: what is there is not fetched again.
    previous: BTreeMap<String, Digest>,
    entries: BTreeMap<String, Digest>,
    /// go.mod files read so far. None: the proxy does not have it.
    mods: BTreeMap<ModVer, Option<GoMod>>,
    sums: BTreeMap<ModVer, (String, String)>,
    report: GoReport,
}

impl Sync<'_> {
    fn path(module: &str, version: &str, ext: &str) -> String {
        format!("{}/@v/{}.{ext}", escape(module), escape(version))
    }

    /// (archive checksum, go.mod checksum) the checksum database states.
    fn expected(&mut self, mv: &ModVer) -> Result<Option<(String, String)>, GoError> {
        let Some(sumdb) = &self.sumdb else { return Ok(None) };
        if let Some(s) = self.sums.get(mv) {
            return Ok(Some(s.clone()));
        }
        let url = format!("{sumdb}/lookup/{}@{}", escape(&mv.0), escape(&mv.1));
        let body = match self.client.get(&url, &[]) {
            Ok(r) => r.bytes(&url, MAX_SMALL)?,
            Err(FetchError::NotFound(_)) | Err(FetchError::Http { status: 410, .. }) => {
                return Err(GoError::NotFound { module: format!("{}@{}", mv.0, mv.1), reason: "the checksum database does not know it (for a private proxy, turn the check off)".into() })
            }
            Err(e) => return Err(e.into()),
        };
        let text = String::from_utf8_lossy(&body);
        let line = |suffix: &str| {
            let prefix = format!("{} {}{suffix} ", mv.0, mv.1);
            text.lines().find_map(|l| l.strip_prefix(&prefix)).map(|h| h.trim().to_string())
        };
        let sums = line("").zip(line("/go.mod")).ok_or_else(|| GoError::Proxy(format!("{url}: the answer does not have the two checksums")))?;
        self.sums.insert(mv.clone(), sums.clone());
        Ok(Some(sums))
    }

    /// Bring one small file (`.mod`, `.info`) into the snapshot and return it.
    fn small(&mut self, mv: &ModVer, ext: &str) -> Result<Option<Vec<u8>>, GoError> {
        let path = Self::path(&mv.0, &mv.1, ext);
        if let Some(d) = self.previous.get(&path).copied() {
            self.entries.insert(path, d);
            self.report.reused += 1;
            return Ok(Some(self.st.read_blob(&d)?));
        }
        let url = format!("{}/{path}", self.proxy);
        let body = match self.client.get(&url, &[]) {
            Ok(r) => r.bytes(&url, MAX_SMALL)?,
            Err(FetchError::NotFound(_)) | Err(FetchError::Http { status: 410, .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if ext == "mod" {
            if let Some((_, expected)) = self.expected(mv)? {
                let actual = h1_gomod(&body);
                if actual != expected {
                    return Err(GoError::Checksum { what: format!("go.mod of {}@{}", mv.0, mv.1), expected, actual });
                }
                self.report.verified += 1;
            }
        }
        let (d, size) = self.st.ingest_bytes(&body)?;
        self.report.downloaded += 1;
        self.report.downloaded_bytes += size;
        self.entries.insert(path, d);
        Ok(Some(body))
    }

    /// The go.mod of a module version (and its `.info`, which the go command asks for first).
    fn gomod(&mut self, mv: &ModVer) -> Result<Option<GoMod>, GoError> {
        if let Some(m) = self.mods.get(mv) {
            return Ok(m.clone());
        }
        let parsed = match self.small(mv, "mod")? {
            Some(body) => {
                self.small(mv, "info")?;
                Some(parse_gomod(&String::from_utf8_lossy(&body)))
            }
            None => None,
        };
        self.mods.insert(mv.clone(), parsed.clone());
        Ok(parsed)
    }

    fn archive(&mut self, mv: &ModVer) -> Result<(), GoError> {
        let path = Self::path(&mv.0, &mv.1, "zip");
        if let Some(d) = self.previous.get(&path).copied() {
            self.entries.insert(path, d);
            self.report.reused += 1;
            return Ok(());
        }
        let url = format!("{}/{path}", self.proxy);
        let body = self.client.get(&url, &[])?.bytes(&url, MAX_ZIP)?;
        if let Some((expected, _)) = self.expected(mv)? {
            let actual = h1_zip(&body).map_err(|e| GoError::Proxy(format!("{url}: {e}")))?;
            if actual != expected {
                return Err(GoError::Checksum { what: format!("the archive of {}@{}", mv.0, mv.1), expected, actual });
            }
            self.report.verified += 1;
        }
        let (d, size) = self.st.ingest_bytes(&body)?;
        self.report.downloaded += 1;
        self.report.downloaded_bytes += size;
        self.entries.insert(path, d);
        Ok(())
    }

    /// The module graph of a project whose go.mod requires `roots`, as the go
    /// command loads it: the go.mod of every requirement is read; what those
    /// require is in the graph too, but is read further only below a module
    /// from before Go 1.17, whose go.mod does not list everything it needs.
    fn graph(&mut self, roots: &[ModVer]) -> Result<BTreeSet<ModVer>, GoError> {
        let mut nodes: BTreeSet<ModVer> = roots.iter().cloned().collect();
        let mut queue: VecDeque<(ModVer, bool)> = roots.iter().map(|r| (r.clone(), false)).collect();
        let mut visited: BTreeSet<(ModVer, bool)> = BTreeSet::new();
        while let Some((mv, complete)) = queue.pop_front() {
            if !visited.insert((mv.clone(), complete)) {
                continue;
            }
            let Some(gomod) = self.gomod(&mv)? else {
                self.warn(format!("{}@{} is required but the proxy does not have it; skipped", mv.0, mv.1));
                continue;
            };
            for r in &gomod.requires {
                if !valid_module(&r.0) || !valid_version(&r.1) {
                    continue;
                }
                nodes.insert(r.clone());
                if complete || !gomod.lists_all_dependencies() {
                    queue.push_back((r.clone(), true));
                }
            }
            if nodes.len() > MAX_NODES {
                return Err(GoError::Proxy(format!("more than {MAX_NODES} module versions in the graph")));
            }
        }
        Ok(nodes)
    }

    fn warn(&mut self, w: String) {
        if !self.report.warnings.contains(&w) {
            self.report.warnings.push(w);
        }
    }
}

/// Minimal version selection: of every module, the highest version in the graph.
fn select(nodes: &BTreeSet<ModVer>) -> BTreeSet<ModVer> {
    let mut best: BTreeMap<&str, &str> = BTreeMap::new();
    for (m, v) in nodes {
        let slot = best.entry(m).or_insert(v);
        if version_key(v) > version_key(slot) {
            *slot = v;
        }
    }
    best.into_iter().map(|(m, v)| (m.to_string(), v.to_string())).collect()
}

/// Mirror the configured modules and what they require into a new snapshot.
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &GoConfig) -> Result<GoReport, GoError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "go" {
        return Err(GoError::Config(format!("repo {repo} has format {}, not go", repo_row.format)));
    }
    for url in std::iter::once(&cfg.proxy_url).chain(&cfg.sumdb_url) {
        if host_of(url).is_none() {
            return Err(GoError::Config(format!("{url:?} is not an http(s) URL")));
        }
    }
    if cfg.modules.is_empty() && cfg.gomods.is_empty() {
        return Err(GoError::Config("at least one module or go.mod file is needed".into()));
    }
    let previous = match st.head(repo)? {
        Some(h) => st.meta().read(|t| t.entries_at(repo_row.id, h.seq))?.into_iter().map(|e| (e.path, e.digest)).collect(),
        None => BTreeMap::new(),
    };
    let mut s = Sync {
        st,
        client,
        proxy: cfg.proxy_url.trim_end_matches('/').to_string(),
        sumdb: cfg.sumdb_url.as_ref().map(|u| u.trim_end_matches('/').to_string()),
        previous,
        entries: BTreeMap::new(),
        mods: BTreeMap::new(),
        sums: BTreeMap::new(),
        report: GoReport::default(),
    };

    // One requirement list per project to serve: a named module stands for a
    // project that requires it, which after `go get` lists the module and
    // everything the module's own go.mod lists.
    let mut projects: Vec<Vec<ModVer>> = Vec::new();
    for spec in &cfg.modules {
        let (module, version) = match spec.trim().split_once('@') {
            Some((m, v)) => (m.trim(), v.trim()),
            None => (spec.trim(), "latest"),
        };
        if !valid_module(module) {
            return Err(GoError::Config(format!("module path {module:?} is not acceptable")));
        }
        let version = if version == "latest" {
            let url = format!("{}/{}/@latest", s.proxy, escape(module));
            let body = match client.get(&url, &[]) {
                Ok(r) => r.bytes(&url, MAX_SMALL)?,
                Err(FetchError::NotFound(_)) | Err(FetchError::Http { status: 410, .. }) => {
                    return Err(GoError::NotFound { module: module.into(), reason: "the proxy does not know it".into() })
                }
                Err(e) => return Err(e.into()),
            };
            let info: serde_json::Value = serde_json::from_slice(&body).map_err(|e| GoError::Proxy(format!("{url}: {e}")))?;
            info["Version"].as_str().filter(|v| valid_version(v)).ok_or_else(|| GoError::Proxy(format!("{url}: no usable version")))?.to_string()
        } else if valid_version(version) {
            version.to_string()
        } else {
            return Err(GoError::Config(format!("{spec}: {version:?} is not a version (write v1.2.3, or leave it out for the latest)")));
        };
        let root = (module.to_string(), version);
        let gomod = s.gomod(&root)?.ok_or_else(|| GoError::NotFound { module: format!("{}@{}", root.0, root.1), reason: "the proxy does not have this version".into() })?;
        let mut list = vec![root];
        list.extend(gomod.requires.into_iter().filter(|r| valid_module(&r.0) && valid_version(&r.1)));
        projects.push(list);
    }
    for text in &cfg.gomods {
        let list: Vec<ModVer> = parse_gomod(text).requires.into_iter().filter(|r| valid_module(&r.0) && valid_version(&r.1)).collect();
        if list.is_empty() {
            return Err(GoError::Config("a go.mod file was given that requires nothing".into()));
        }
        projects.push(list);
    }
    if projects.len() > 1 {
        // And one project that uses all of them together.
        let all: BTreeSet<ModVer> = projects.iter().flatten().cloned().collect();
        projects.push(all.into_iter().collect());
    }

    let mut selected: BTreeSet<ModVer> = BTreeSet::new();
    for project in &projects {
        let nodes = s.graph(project)?;
        selected.extend(select(&nodes));
    }
    let mut archives: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for mv in &selected {
        // The go command reads .info and .mod before it asks for the archive.
        if s.gomod(mv)?.is_none() {
            s.warn(format!("{}@{} is required but the proxy does not have it; skipped", mv.0, mv.1));
            continue;
        }
        s.archive(mv)?;
        archives.entry(mv.0.clone()).or_default().push(mv.1.clone());
    }

    // Per module: the list of versions, and what "latest" is.
    for (module, versions) in &mut archives {
        versions.sort_by_key(|v| version_key(v));
        let list: String = versions.iter().map(|v| format!("{v}\n")).collect();
        let (d, _) = st.ingest_bytes(list.as_bytes())?;
        s.entries.insert(format!("{}/@v/list", escape(module)), d);
        let release = versions.iter().rev().find(|v| version_key(v).is_some_and(|k| k.pre.is_empty())).or(versions.last()).expect("not empty");
        if let Some(d) = s.entries.get(&Sync::path(module, release, "info")).copied() {
            s.entries.insert(format!("{}/@latest", escape(module)), d);
        }
        s.report.archives += versions.len();
    }
    s.report.modules = archives.len();
    s.report.gomods = s.mods.values().filter(|m| m.is_some()).count();

    let mut writer = st.begin(repo)?;
    for (path, digest) in &s.entries {
        writer.put(path, EntryRole::Artifact, digest)?;
    }
    for gone in s.previous.keys().filter(|p| !s.entries.contains_key(*p)) {
        writer.remove(gone)?;
    }
    s.report.snapshot = writer.seal(None)?.map(|x| x.seq);
    Ok(s.report)
}
