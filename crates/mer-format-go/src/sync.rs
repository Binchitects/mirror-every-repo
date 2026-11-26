//! Collector-side sync of Go modules from a module proxy into one repo snapshot.
use crate::hash::{gomod_hash, zip_hash, MAX_ZIP};
use crate::modfile::{escape, parse_gomod, parse_gosum, valid_path, GoMod};
use crate::version;
use mer_core::{Digest, EntryRole};
use mer_fetch::{host_of, FetchError, HttpClient};
use mer_storage::{Storage, StorageError};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const MAX_MOD: u64 = 16 << 20;
const MAX_SMALL: u64 = 16 << 20;
const MAX_VERSIONS: usize = 100_000;

#[derive(Debug, thiserror::Error)]
pub enum GoError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("upstream: {0}")]
    Upstream(String),
    #[error("{0}")]
    Config(String),
    #[error("{} {}: {} does not match the checksum ({}); expected {}, got {}", .0.module, .0.version, .0.what, .0.source_, .0.want, .0.got)]
    Checksum(Box<Mismatch>),
}

#[derive(Debug)]
pub struct Mismatch {
    pub module: String,
    pub version: String,
    pub what: &'static str,
    /// `go.sum` or `checksum database`.
    pub source_: &'static str,
    pub want: String,
    pub got: String,
}

impl From<mer_meta::MetaError> for GoError {
    fn from(e: mer_meta::MetaError) -> Self {
        GoError::Storage(e.into())
    }
}

#[derive(Clone, Debug)]
pub struct GoConfig {
    /// Module proxy, for example `https://proxy.golang.org`.
    pub proxy: String,
    /// Checksum database, for example `https://sum.golang.org`. `None`:
    /// downloads are checked only against hashes from `gosums`.
    pub sumdb: Option<String>,
    /// What to mirror: `rsc.io/quote@v1.5.2`, `github.com/spf13/cobra` (newest).
    pub modules: Vec<String>,
    /// Contents of `go.mod` files: their requirements are mirrored as roots.
    pub gomods: Vec<String>,
    /// Contents of `go.sum` files: exactly what they list is mirrored, and
    /// checked against the hashes they state.
    pub gosums: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoReport {
    /// Distinct modules with at least one zip.
    pub modules: usize,
    pub zips: usize,
    pub gomods: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub reused: usize,
    /// Downloads no checksum was available for.
    pub unverified: usize,
    pub warnings: Vec<String>,
    pub snapshot: Option<u64>,
}

fn up(msg: impl Into<String>) -> GoError {
    GoError::Upstream(msg.into())
}

type Mv = (String, String);

fn mod_path(m: &str, v: &str, ext: &str) -> String {
    format!("{}/@v/{}.{ext}", escape(m), escape(v))
}

/// Newest version per module.
fn select(versions: impl IntoIterator<Item = Mv>) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (m, v) in versions {
        match out.get(&m) {
            Some(have) if version::compare(have, &v).is_ge() => {}
            _ => {
                out.insert(m, v);
            }
        }
    }
    out
}

struct Fetcher<'a> {
    st: &'a Storage,
    client: &'a HttpClient,
    proxy: String,
    sumdb: Option<String>,
    previous: &'a BTreeMap<String, Digest>,
    /// From go.sum files: (module, version, is go.mod) -> h1.
    known: BTreeMap<(String, String, bool), String>,
    /// From the checksum database: (module, version) -> (zip h1, go.mod h1).
    looked_up: BTreeMap<Mv, (Option<String>, Option<String>)>,
    mods: BTreeMap<Mv, (GoMod, Digest)>,
    /// Versions the proxy does not have.
    missing: BTreeSet<Mv>,
    entries: BTreeMap<String, Digest>,
    report: GoReport,
}

impl Fetcher<'_> {
    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.proxy)
    }

    /// The hash Go would check this file against, and where it comes from.
    fn expected(&mut self, m: &str, v: &str, gomod: bool) -> Result<Option<(String, &'static str)>, GoError> {
        if let Some(h) = self.known.get(&(m.to_string(), v.to_string(), gomod)) {
            return Ok(Some((h.clone(), "go.sum")));
        }
        let Some(db) = self.sumdb.clone() else { return Ok(None) };
        let key = (m.to_string(), v.to_string());
        if !self.looked_up.contains_key(&key) {
            let url = format!("{db}/lookup/{}@{}", escape(m), escape(v));
            let body = match self.client.get_bytes(&url, &[], MAX_SMALL) {
                Ok(b) => b,
                Err(FetchError::NotFound(_)) => {
                    return Err(up(format!("{m} {v}: the checksum database does not know this version (for private modules, pass their go.sum or turn the checksum database off)")));
                }
                Err(e) => return Err(e.into()),
            };
            let text = String::from_utf8_lossy(&body);
            let (mut zip, mut gm) = (None, None);
            for line in text.lines() {
                let f: Vec<&str> = line.split_whitespace().collect();
                if let [path, ver, hash] = f[..] {
                    if path != m || !hash.starts_with("h1:") {
                        continue;
                    }
                    if ver == v {
                        zip = Some(hash.to_string());
                    } else if ver.strip_suffix("/go.mod") == Some(v) {
                        gm = Some(hash.to_string());
                    }
                }
            }
            self.looked_up.insert(key.clone(), (zip, gm));
        }
        let (zip, gm) = &self.looked_up[&key];
        let found = if gomod { gm } else { zip };
        match found {
            Some(h) => Ok(Some((h.clone(), "checksum database"))),
            None => Err(up(format!("{m} {v}: the checksum database answer has no hash for it"))),
        }
    }

    fn check(&mut self, m: &str, v: &str, gomod: bool, got: &str) -> Result<(), GoError> {
        match self.expected(m, v, gomod)? {
            Some((want, source_)) if want != got => Err(GoError::Checksum(Box::new(Mismatch {
                module: m.into(),
                version: v.into(),
                what: if gomod { "go.mod" } else { "the module zip" },
                source_,
                want,
                got: got.into(),
            }))),
            Some(_) => Ok(()),
            None => {
                self.report.unverified += 1;
                self.report.warnings.push(format!("{m} {v}: no checksum to check {} against; mirrored as downloaded", if gomod { "go.mod" } else { "the zip" }));
                Ok(())
            }
        }
    }

    /// Reuse a file from the previous snapshot, if its blob is still there.
    fn reuse(&mut self, path: &str) -> Result<Option<Digest>, GoError> {
        match self.previous.get(path) {
            Some(d) if self.st.has_blob(d)? => {
                self.report.reused += 1;
                self.entries.insert(path.to_string(), *d);
                Ok(Some(*d))
            }
            _ => Ok(None),
        }
    }

    fn gomod(&mut self, m: &str, v: &str) -> Result<Option<&GoMod>, GoError> {
        let key = (m.to_string(), v.to_string());
        if self.missing.contains(&key) {
            return Ok(None);
        }
        if !self.mods.contains_key(&key) {
            if self.mods.len() >= MAX_VERSIONS {
                return Err(up("module graph is implausibly large"));
            }
            let path = mod_path(m, v, "mod");
            let (body, digest) = match self.reuse(&path)? {
                Some(d) => (self.st.read_blob(&d)?, d),
                None => {
                    let url = self.url(&path);
                    let body = match self.client.get_bytes(&url, &[], MAX_MOD) {
                        Ok(b) => b,
                        Err(FetchError::NotFound(_)) => {
                            self.missing.insert(key);
                            return Ok(None);
                        }
                        Err(e) => return Err(e.into()),
                    };
                    self.check(m, v, true, &gomod_hash(&body))?;
                    let (d, size) = self.st.ingest_bytes(&body)?;
                    self.report.downloaded += 1;
                    self.report.downloaded_bytes += size;
                    self.entries.insert(path, d);
                    (body, d)
                }
            };
            let parsed = parse_gomod(&String::from_utf8_lossy(&body));
            self.mods.insert(key.clone(), (parsed, digest));
        }
        Ok(self.mods.get(&key).map(|(g, _)| g))
    }

    /// The zip and `.info` of one version.
    fn zip(&mut self, m: &str, v: &str) -> Result<Vec<u8>, GoError> {
        let info_path = mod_path(m, v, "info");
        let zip_path = mod_path(m, v, "zip");
        if self.reuse(&zip_path)?.is_none() {
            let url = self.url(&zip_path);
            let body = self.client.get_bytes(&url, &[], MAX_ZIP)?;
            let got = zip_hash(&body, &format!("{m}@{v}")).map_err(|e| up(format!("{m} {v}: {e}")))?;
            self.check(m, v, false, &got)?;
            let (d, size) = self.st.ingest_bytes(&body)?;
            self.report.downloaded += 1;
            self.report.downloaded_bytes += size;
            self.entries.insert(zip_path, d);
        }
        let info = match self.reuse(&info_path)? {
            Some(d) => self.st.read_blob(&d)?,
            None => {
                let url = self.url(&info_path);
                let body = self.client.get_bytes(&url, &[], MAX_SMALL)?;
                let doc: Value = serde_json::from_slice(&body).map_err(|_| up(format!("{m} {v}: .info is not JSON")))?;
                if doc["Version"].as_str() != Some(v) {
                    return Err(up(format!("{m} {v}: .info names another version")));
                }
                let (d, _) = self.st.ingest_bytes(&body)?;
                self.entries.insert(info_path, d);
                body
            }
        };
        Ok(info)
    }

    /// `module` alone: the newest release, passing over retracted ones, as `go get module@latest` does.
    fn latest(&mut self, m: &str) -> Result<String, GoError> {
        let url = self.url(&format!("{}/@v/list", escape(m)));
        let listed: Vec<String> = match self.client.get(&url, &[]) {
            Ok(r) => String::from_utf8_lossy(&r.bytes(&url, MAX_SMALL)?).lines().map(str::trim).filter(|v| version::is_canonical(v)).map(str::to_string).collect(),
            Err(FetchError::NotFound(_)) => return Err(up(format!("{m}: the proxy does not know this module"))),
            Err(e) => return Err(e.into()),
        };
        let mut all = listed.clone();
        all.sort_by(|a, b| version::compare(b, a));
        let mut releases: Vec<&String> = all.iter().filter(|v| !version::is_prerelease(v)).collect();
        if releases.is_empty() {
            releases = all.iter().collect();
        }
        // Retractions come from the go.mod of the version @latest would pick without them.
        if let Some(newest) = releases.first().map(|v| (*v).clone()) {
            let retracts = self.gomod(m, &newest)?.cloned().unwrap_or_default();
            if let Some(v) = releases.iter().find(|v| !retracts.retracted(v)) {
                return Ok((*v).clone());
            }
        }
        // No tagged versions: the proxy can still name a pseudo-version.
        let url = self.url(&format!("{}/@latest", escape(m)));
        let info: Value = serde_json::from_slice(&self.client.get_bytes(&url, &[], MAX_SMALL)?).map_err(|_| up(format!("{m}: @latest is not JSON")))?;
        info["Version"].as_str().filter(|v| version::is_canonical(v)).map(str::to_string).ok_or_else(|| up(format!("{m}: no version to mirror")))
    }

    /// Every (module, version) reachable from `roots` through go.mod requirements.
    fn reach(&mut self, roots: &[Mv]) -> Result<BTreeSet<Mv>, GoError> {
        let mut seen = BTreeSet::new();
        let mut queue: VecDeque<Mv> = roots.iter().cloned().collect();
        while let Some((m, v)) = queue.pop_front() {
            if !seen.insert((m.clone(), v.clone())) {
                continue;
            }
            match self.gomod(&m, &v)? {
                Some(g) => queue.extend(g.requires.iter().filter(|(rm, rv)| valid_path(rm) && version::is_canonical(rv)).cloned()),
                None => self.report.warnings.push(format!("{m} {v}: the proxy does not have it; left out")),
            }
        }
        Ok(seen)
    }
}

/// `path@version` or `path` (newest release).
fn split_spec(spec: &str) -> Option<(String, Option<String>)> {
    let spec = spec.trim();
    let (path, v) = match spec.split_once('@') {
        Some((p, "latest")) => (p, None),
        Some((p, v)) => (p, Some(v.to_string())),
        None => (spec, None),
    };
    (valid_path(path) && v.as_deref().is_none_or(version::is_canonical)).then(|| (path.to_string(), v))
}

/// Mirror the configured modules and what they need into a new snapshot of `repo`.
///
/// Every downloaded `.zip` and `.mod` is checked against the `h1:` hash a
/// given `go.sum` states, or else the one the checksum database states
/// (fetched over TLS; its signed tree and inclusion proofs are not checked).
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &GoConfig) -> Result<GoReport, GoError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "go" {
        return Err(GoError::Config(format!("repo {repo} has format {}, not go", repo_row.format)));
    }
    for url in std::iter::once(&cfg.proxy).chain(cfg.sumdb.iter()) {
        if host_of(url).is_none() {
            return Err(GoError::Config(format!("{url:?} is not an http(s) URL")));
        }
    }
    if cfg.modules.is_empty() && cfg.gomods.is_empty() && cfg.gosums.is_empty() {
        return Err(GoError::Config("name at least one module, go.mod or go.sum".into()));
    }
    let mut specs = Vec::new();
    for spec in &cfg.modules {
        specs.push(split_spec(spec).ok_or_else(|| GoError::Config(format!("module spec {spec:?} is not understood (path or path@vX.Y.Z)")))?);
    }
    let mut known = BTreeMap::new();
    let mut exact_zips: BTreeSet<Mv> = BTreeSet::new();
    let mut exact_mods: BTreeSet<Mv> = BTreeSet::new();
    for text in &cfg.gosums {
        for (m, v, gomod, h) in parse_gosum(text).map_err(GoError::Config)? {
            if gomod {
                exact_mods.insert((m.clone(), v.clone()));
            } else {
                exact_zips.insert((m.clone(), v.clone()));
            }
            known.insert((m, v, gomod), h);
        }
    }

    let previous: BTreeMap<String, Digest> = match st.head(repo)? {
        Some(h) => st.meta().read(|t| t.entries_at(repo_row.id, h.seq))?.into_iter().map(|e| (e.path, e.digest)).collect(),
        None => BTreeMap::new(),
    };
    let mut f = Fetcher {
        st,
        client,
        proxy: cfg.proxy.trim_end_matches('/').to_string(),
        sumdb: cfg.sumdb.as_ref().map(|s| s.trim_end_matches('/').to_string()),
        previous: &previous,
        known,
        looked_up: BTreeMap::new(),
        mods: BTreeMap::new(),
        missing: BTreeSet::new(),
        entries: BTreeMap::new(),
        report: GoReport::default(),
    };

    // 1. Roots: each is resolved on its own, because what Go selects depends
    //    on the project that requires it.
    let mut roots: Vec<Vec<Mv>> = Vec::new();
    for (m, v) in specs {
        let v = match v {
            Some(v) => v,
            None => f.latest(&m)?,
        };
        roots.push(vec![(m, v)]);
    }
    for text in &cfg.gomods {
        let g = parse_gomod(text);
        if g.replaces > 0 {
            f.report.warnings.push(format!("{}: replace directives are not followed; mirror replacement modules by naming them", g.module.as_deref().unwrap_or("a go.mod")));
        }
        roots.push(g.requires.into_iter().filter(|(m, v)| valid_path(m) && version::is_canonical(v)).collect());
    }

    // 2. Zips for what a build would select. Go selects the newest version
    //    of each module in the requirement graph (minimal version selection).
    //    With graph pruning (go 1.17 and later) the graph is smaller and the
    //    choice can be older, so both selections are kept, as is every
    //    version a root names directly.
    // What the configuration names outright must exist: a typo must not
    // quietly produce a snapshot without it.
    for (m, v) in roots.iter().flatten().chain(&exact_zips).chain(&exact_mods) {
        if f.gomod(m, v)?.is_none() {
            return Err(up(format!("{m} {v}: the proxy does not have this version")));
        }
    }
    let mut zips = exact_zips;
    for root in &roots {
        let all = f.reach(root)?;
        let mut pruned: Vec<Mv> = root.clone();
        for (m, v) in root {
            let g = f.gomod(m, v)?.cloned();
            let modern = g.as_ref().and_then(|g| g.go.as_deref()).and_then(version::go_release).is_some_and(|r| r >= (1, 17));
            match g {
                Some(g) if modern => pruned.extend(g.requires.into_iter().filter(|(m, v)| valid_path(m) && version::is_canonical(v))),
                Some(_) => pruned.extend(f.reach(&[(m.clone(), v.clone())])?),
                None => {}
            }
        }
        zips.extend(select(all).into_iter().chain(select(pruned)).chain(root.iter().cloned()));
    }
    // 3. Zips, version lists, and @latest.
    let mut listed: BTreeMap<String, Vec<(String, Vec<u8>)>> = BTreeMap::new();
    for (m, v) in &zips {
        if f.gomod(m, v)?.is_none() {
            continue;
        }
        let info = f.zip(m, v)?;
        listed.entry(m.clone()).or_default().push((v.clone(), info));
    }
    f.report.modules = listed.len();
    f.report.zips = listed.values().map(Vec::len).sum();
    f.report.gomods = f.mods.len();
    for (m, mut versions) in listed {
        versions.sort_by(|a, b| version::compare(&a.0, &b.0));
        let list: String = versions.iter().map(|(v, _)| format!("{v}\n")).collect();
        let (d, _) = st.ingest_bytes(list.as_bytes())?;
        f.entries.insert(format!("{}/@v/list", escape(&m)), d);
        let newest = versions.iter().rev().find(|(v, _)| !version::is_prerelease(v)).or(versions.last()).expect("not empty");
        let (d, _) = st.ingest_bytes(&newest.1)?;
        f.entries.insert(format!("{}/@latest", escape(&m)), d);
    }

    // 4. One snapshot.
    let entries = std::mem::take(&mut f.entries);
    let mut report = f.report;
    let mut writer = st.begin(repo)?;
    for (path, digest) in &entries {
        writer.put(path, EntryRole::Artifact, digest)?;
    }
    for gone in previous.keys().filter(|p| !entries.contains_key(*p)) {
        writer.remove(gone)?;
    }
    report.snapshot = writer.seal(None)?.map(|s| s.seq);
    report.warnings.sort();
    report.warnings.dedup();
    Ok(report)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn specs() {
        assert_eq!(split_spec("rsc.io/quote@v1.5.2"), Some(("rsc.io/quote".into(), Some("v1.5.2".into()))));
        assert_eq!(split_spec("github.com/spf13/cobra"), Some(("github.com/spf13/cobra".into(), None)));
        assert_eq!(split_spec("x.io/y@latest"), Some(("x.io/y".into(), None)));
        assert_eq!(split_spec("x.io/y@v1"), None, "queries other than latest are not resolved");
        assert_eq!(split_spec("../x@v1.0.0"), None);
        assert_eq!(mod_path("github.com/BurntSushi/toml", "v1.4.0", "zip"), "github.com/!burnt!sushi/toml/@v/v1.4.0.zip");
    }
}
