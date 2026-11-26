//! Collector-side sync of a Cargo sparse index into one repo snapshot.
use crate::lock::{parse_lockfile, Locked};
use mer_core::{Digest, EntryRole};
use mer_fetch::{host_of, FetchError, HttpClient};
use mer_storage::{Storage, StorageError};
use semver::{Version, VersionReq};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Read;

/// Stands in for "this server's address" in the stored `config.json`. The
/// vault replaces it with its own URL when it serves the file.
pub const BASE_PLACEHOLDER: &str = "mer://base/";

const MAX_CONFIG: u64 = 1 << 20;
const MAX_INDEX_FILE: u64 = 64 << 20;
const MAX_CRATE: u64 = 2 << 30;
const MAX_VERSIONS: usize = 50_000;

#[derive(Debug, thiserror::Error)]
pub enum CargoError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("upstream registry: {0}")]
    Registry(String),
    #[error("{0}")]
    Config(String),
    #[error("no version of {krate} matches {req:?}")]
    Unsatisfiable { krate: String, req: String },
}

impl From<mer_meta::MetaError> for CargoError {
    fn from(e: mer_meta::MetaError) -> Self {
        CargoError::Storage(e.into())
    }
}

#[derive(Clone, Debug)]
pub struct CargoConfig {
    /// Sparse index URL, for example `https://index.crates.io/`.
    pub index: String,
    /// What to mirror: `serde`, `serde@1.0`, `tokio@1:full,macros`, `rand@=0.8.5`.
    pub crates: Vec<String>,
    /// Contents of `Cargo.lock` files whose registry packages are mirrored
    /// at exactly the versions they name.
    pub lockfiles: Vec<String>,
    /// The Rust toolchain inside the air gap, for example `1.80`. Versions
    /// that declare a newer `rust-version` are passed over, as Cargo's
    /// MSRV-aware resolver does.
    pub rust_version: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CargoReport {
    pub crates: usize,
    pub versions: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub reused: usize,
    pub warnings: Vec<String>,
    pub snapshot: Option<u64>,
}

fn reg(msg: impl Into<String>) -> CargoError {
    CargoError::Registry(msg.into())
}

/// Crate names: ASCII letters, digits, `-` and `_`, starting with a letter or digit.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.len() <= 64 && b[0].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

/// Directory part of a crate's index path, keeping the case of `name`.
fn prefix_of(name: &str) -> String {
    match name.len() {
        1 => "1".into(),
        2 => "2".into(),
        3 => format!("3/{}", &name[..1]),
        _ => format!("{}/{}", &name[..2], &name[2..4]),
    }
}

/// Where the index keeps a crate: `1/a`, `2/ab`, `3/a/abc`, `se/rd/serde`. Lower case.
pub fn index_path(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    format!("{}/{lower}", prefix_of(&lower))
}

/// The download URL from the `dl` value of the registry's `config.json`.
fn download_url(dl: &str, name: &str, version: &str, cksum: &str) -> String {
    const MARKERS: [&str; 5] = ["{crate}", "{version}", "{prefix}", "{lowerprefix}", "{sha256-checksum}"];
    if !MARKERS.iter().any(|m| dl.contains(m)) {
        return format!("{}/{name}/{version}/download", dl.trim_end_matches('/'));
    }
    dl.replace("{crate}", name)
        .replace("{version}", version)
        .replace("{lowerprefix}", &prefix_of(&name.to_ascii_lowercase()))
        .replace("{prefix}", &prefix_of(name))
        .replace("{sha256-checksum}", cksum)
}

/// `1.80` -> 1.80.0
fn loose_version(text: &str) -> Option<Version> {
    let mut parts = text.trim().split('.').map(|p| p.parse::<u64>());
    let major = parts.next()?.ok()?;
    let minor = parts.next().unwrap_or(Ok(0)).ok()?;
    let patch = parts.next().unwrap_or(Ok(0)).ok()?;
    parts.next().is_none().then(|| Version::new(major, minor, patch))
}

#[derive(Clone, Debug)]
struct Dep {
    /// The name the dependent uses for it (differs from `package` when renamed).
    key: String,
    package: String,
    req: VersionReq,
    req_text: String,
    features: Vec<String>,
    optional: bool,
    default_features: bool,
    dev: bool,
    registry: Option<String>,
}

/// One line of an index file: one published version.
#[derive(Clone, Debug)]
struct Line {
    name: String,
    vers: Version,
    vers_text: String,
    cksum: Digest,
    yanked: bool,
    rust_version: Option<Version>,
    deps: Vec<Dep>,
    features: BTreeMap<String, Vec<String>>,
    raw: String,
}

fn parse_line(raw: &str) -> Result<Line, String> {
    let v: Value = serde_json::from_str(raw).map_err(|_| "not JSON".to_string())?;
    let name = v["name"].as_str().filter(|n| valid_name(n)).ok_or("no valid name")?.to_string();
    let vers_text = v["vers"].as_str().ok_or("no version")?.to_string();
    let vers = Version::parse(&vers_text).map_err(|_| format!("version {vers_text:?} is not semver"))?;
    let cksum = v["cksum"].as_str().and_then(|c| Digest::from_hex(c).ok()).ok_or("no SHA-256 checksum")?;
    let mut features = BTreeMap::new();
    for key in ["features", "features2"] {
        if let Some(map) = v[key].as_object() {
            for (f, items) in map {
                let items = items.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string);
                features.entry(f.clone()).or_insert_with(Vec::new).extend(items);
            }
        }
    }
    let mut deps = Vec::new();
    for d in v["deps"].as_array().into_iter().flatten() {
        let key = d["name"].as_str().ok_or("dependency without a name")?.to_string();
        let package = d["package"].as_str().unwrap_or(&key).to_string();
        let req_text = d["req"].as_str().unwrap_or("*").to_string();
        let req = VersionReq::parse(&req_text).map_err(|_| format!("dependency {key}: requirement {req_text:?} is not understood"))?;
        deps.push(Dep {
            key,
            package,
            req,
            req_text,
            features: d["features"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect(),
            optional: d["optional"].as_bool().unwrap_or(false),
            default_features: d["default_features"].as_bool().unwrap_or(true),
            dev: d["kind"].as_str() == Some("dev"),
            registry: d["registry"].as_str().map(str::to_string),
        });
    }
    Ok(Line {
        name,
        vers,
        vers_text,
        cksum,
        yanked: v["yanked"].as_bool().unwrap_or(false),
        rust_version: v["rust_version"].as_str().and_then(loose_version),
        deps,
        features,
        raw: raw.to_string(),
    })
}

/// `name`, `name@req`, `name@req:feat1,feat2`, `name:feat` -> (name, req text, features).
fn split_spec(spec: &str) -> Option<(String, String, Vec<String>)> {
    let spec = spec.trim();
    let (head, feats) = match spec.split_once(':') {
        Some((h, f)) => (h, f.split(',').map(str::trim).filter(|f| !f.is_empty()).map(str::to_string).collect()),
        None => (spec, Vec::new()),
    };
    let (name, req) = match head.split_once('@') {
        Some((n, r)) if r.trim() != "latest" => (n.trim(), r.trim()),
        Some((n, _)) => (n.trim(), "*"),
        None => (head.trim(), "*"),
    };
    (valid_name(name) && VersionReq::parse(req).is_ok()).then(|| (name.to_string(), req.to_string(), feats))
}

struct Registry<'a> {
    client: &'a HttpClient,
    index: String,
    /// Lower-case name -> the versions the index lists (None: not in the index).
    files: BTreeMap<String, Option<Vec<Line>>>,
    warnings: Vec<String>,
}

impl Registry<'_> {
    fn lines(&mut self, name: &str) -> Result<Option<&Vec<Line>>, CargoError> {
        let lower = name.to_ascii_lowercase();
        if !self.files.contains_key(&lower) {
            let url = format!("{}{}", self.index, index_path(&lower));
            let parsed = match self.client.get(&url, &[]) {
                Ok(r) => {
                    let body = r.bytes(&url, MAX_INDEX_FILE)?;
                    let text = String::from_utf8(body).map_err(|_| reg(format!("{url}: not UTF-8")))?;
                    let mut lines = Vec::new();
                    for raw in text.lines().filter(|l| !l.trim().is_empty()) {
                        match parse_line(raw) {
                            Ok(line) if line.name.eq_ignore_ascii_case(name) => lines.push(line),
                            Ok(line) => return Err(reg(format!("{url}: lists crate {:?}, not {name}", line.name))),
                            Err(why) => self.warnings.push(format!("{name}: an index line is skipped ({why})")),
                        }
                    }
                    Some(lines)
                }
                Err(FetchError::NotFound(_)) => None,
                Err(e) => return Err(e.into()),
            };
            self.files.insert(lower.clone(), parsed);
        }
        Ok(self.files.get(&lower).and_then(Option::as_ref))
    }
}

type Key = (String, String); // (lower-case name, version text)

struct Node {
    line: Line,
}

struct Resolver<'a, 'b> {
    reg: &'b mut Registry<'a>,
    rust: Option<Version>,
    nodes: BTreeMap<Key, Node>,
    /// Enabled features per crate (lower-case name), shared by all its
    /// mirrored versions: Cargo may settle on any of them, and the features
    /// asked of the crate then all apply to that one version.
    features: BTreeMap<String, BTreeSet<String>>,
    queue: VecDeque<Key>,
}

impl Resolver<'_, '_> {
    /// Newest non-yanked version matching `req`, preferring ones the
    /// configured toolchain can build.
    fn pick(&mut self, name: &str, req: &VersionReq) -> Result<Result<Line, String>, CargoError> {
        let rust = self.rust.clone();
        let Some(lines) = self.reg.lines(name)? else { return Ok(Err("not in the index".into())) };
        let matching: Vec<&Line> = lines.iter().filter(|l| !l.yanked && req.matches(&l.vers)).collect();
        let fits = |l: &&&Line| match (&rust, &l.rust_version) {
            (Some(have), Some(need)) => need <= have,
            _ => true,
        };
        if let Some(best) = matching.iter().filter(fits).max_by(|a, b| a.vers.cmp(&b.vers)) {
            return Ok(Ok((*best).clone()));
        }
        let Some(best) = matching.iter().max_by(|a, b| a.vers.cmp(&b.vers)).map(|l| (*l).clone()) else {
            return Ok(Err(format!("no version matches {req}")));
        };
        let need = best.rust_version.as_ref().map(ToString::to_string).unwrap_or_default();
        self.reg.warnings.push(format!("{name} {}: needs Rust {need}, newer than the configured toolchain; mirrored anyway", best.vers_text));
        Ok(Ok(best))
    }

    fn activate(&mut self, name: &str, req: &VersionReq, req_text: &str, features: &[String], default: bool, root: bool) -> Result<(), CargoError> {
        if !valid_name(name) {
            self.reg.warnings.push(format!("dependency name {name:?} is not acceptable; skipped"));
            return Ok(());
        }
        let line = match self.pick(name, req)? {
            Ok(line) => line,
            Err(_) if root => {
                return Err(CargoError::Unsatisfiable { krate: name.to_string(), req: req_text.to_string() });
            }
            Err(why) => {
                self.reg.warnings.push(format!("{name} {req_text}: {why}; not mirrored"));
                return Ok(());
            }
        };
        let key = (line.name.to_ascii_lowercase(), line.vers_text.clone());
        let is_new = !self.nodes.contains_key(&key);
        if is_new && self.nodes.len() >= MAX_VERSIONS {
            return Err(reg("dependency tree is implausibly large"));
        }
        let has_default = line.features.contains_key("default");
        self.nodes.entry(key.clone()).or_insert_with(|| Node { line });
        let enabled = self.features.entry(key.0.clone()).or_default();
        let mut grew = false;
        for f in features.iter().cloned().chain((default && has_default).then(|| "default".to_string())) {
            grew |= enabled.insert(f);
        }
        if grew {
            // Every mirrored version of the crate gets the new features.
            let versions: Vec<Key> = self.nodes.keys().filter(|k| k.0 == key.0).cloned().collect();
            self.queue.extend(versions);
        } else if is_new {
            self.queue.push_back(key);
        }
        Ok(())
    }

    /// Turn on what the enabled features of one version ask for.
    fn process(&mut self, key: &Key) -> Result<(), CargoError> {
        let node = &self.nodes[key];
        let (fmap, deps) = (&node.line.features, node.line.deps.clone());
        let optional: BTreeSet<&str> = deps.iter().filter(|d| d.optional).map(|d| d.key.as_str()).collect();
        let mut active: BTreeSet<String> = BTreeSet::new();
        let mut extra: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut stack: Vec<String> = self.features.get(&key.0).into_iter().flatten().cloned().collect();
        while let Some(f) = stack.pop() {
            if !seen.insert(f.clone()) {
                continue;
            }
            let Some(items) = fmap.get(&f) else {
                // A feature not in the table: the implicit feature of an optional dependency.
                if optional.contains(f.as_str()) {
                    active.insert(f);
                }
                continue;
            };
            for item in items {
                if let Some(dep) = item.strip_prefix("dep:") {
                    active.insert(dep.to_string());
                } else if let Some((dep, feat)) = item.split_once('/') {
                    let (dep, weak) = match dep.strip_suffix('?') {
                        Some(d) => (d, true),
                        None => (dep, false),
                    };
                    if !weak && optional.contains(dep) {
                        active.insert(dep.to_string());
                    }
                    extra.entry(dep.to_string()).or_default().push(feat.to_string());
                } else if fmap.contains_key(item) {
                    stack.push(item.clone());
                } else if optional.contains(item.as_str()) {
                    active.insert(item.clone());
                }
            }
        }
        for dep in deps.iter().filter(|d| !d.dev) {
            if dep.optional && !active.contains(&dep.key) {
                continue;
            }
            if let Some(other) = &dep.registry {
                self.reg.warnings.push(format!("{} needs {} from another registry ({other}); not mirrored", key.0, dep.package));
                continue;
            }
            let mut feats = dep.features.clone();
            feats.extend(extra.get(&dep.key).into_iter().flatten().cloned());
            self.activate(&dep.package, &dep.req, &dep.req_text, &feats, dep.default_features, false)?;
        }
        Ok(())
    }
}

/// Does a lockfile `source` name the registry being mirrored?
fn same_registry(source: &str, index: &str) -> bool {
    const CRATES_IO: [&str; 2] = ["registry+https://github.com/rust-lang/crates.io-index", "sparse+https://index.crates.io/"];
    let norm = |s: &str| s.trim_end_matches('/').to_ascii_lowercase();
    if CRATES_IO.contains(&source) {
        return norm(index) == "https://index.crates.io";
    }
    let url = source.strip_prefix("sparse+").or_else(|| source.strip_prefix("registry+")).unwrap_or(source);
    norm(url) == norm(index)
}

/// Mirror the configured crates and their dependencies into a new snapshot
/// of `repo`.
///
/// What is trusted is the TLS connection to the index and the SHA-256 it
/// states for each version; every `.crate` is checked against that hash.
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &CargoConfig) -> Result<CargoReport, CargoError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "cargo" {
        return Err(CargoError::Config(format!("repo {repo} has format {}, not cargo", repo_row.format)));
    }
    let index = cfg.index.strip_prefix("sparse+").unwrap_or(&cfg.index);
    if host_of(index).is_none() {
        return Err(CargoError::Config(format!("{:?} is not an http(s) URL of a sparse index", cfg.index)));
    }
    let index = if index.ends_with('/') { index.to_string() } else { format!("{index}/") };
    if cfg.crates.is_empty() && cfg.lockfiles.is_empty() {
        return Err(CargoError::Config("name at least one crate or lockfile".into()));
    }
    let rust = match &cfg.rust_version {
        Some(text) => Some(loose_version(text).ok_or_else(|| CargoError::Config(format!("Rust version {text:?} is not understood (use 1.80)")))?),
        None => None,
    };
    let mut roots = Vec::new();
    for spec in &cfg.crates {
        roots.push(split_spec(spec).ok_or_else(|| CargoError::Config(format!("crate spec {spec:?} is not understood")))?);
    }
    let mut locked: Vec<Locked> = Vec::new();
    for text in &cfg.lockfiles {
        locked.extend(parse_lockfile(text).map_err(CargoError::Config)?);
    }

    // 1. Where the registry keeps the crate files.
    let config_url = format!("{index}config.json");
    let config = client.get(&config_url, &[("Accept", "application/json")])?.bytes(&config_url, MAX_CONFIG)?;
    let config: Value = serde_json::from_slice(&config).map_err(|_| reg("config.json is not JSON"))?;
    let dl = config["dl"].as_str().filter(|d| host_of(d).is_some()).ok_or_else(|| reg("config.json has no http(s) download URL (dl)"))?.to_string();

    // 2. Resolve: roots as Cargo would, lockfile entries exactly as named.
    let mut registry = Registry { client, index, files: BTreeMap::new(), warnings: Vec::new() };
    let mut r = Resolver { reg: &mut registry, rust, nodes: BTreeMap::new(), features: BTreeMap::new(), queue: VecDeque::new() };
    for (name, req_text, feats) in &roots {
        let req = VersionReq::parse(req_text).expect("checked by split_spec");
        r.activate(name, &req, req_text, feats, true, true)?;
    }
    while let Some(key) = r.queue.pop_front() {
        r.process(&key)?;
    }
    let mut selected: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (name, vers) in r.nodes.keys() {
        selected.entry(name.clone()).or_default().insert(vers.clone());
    }
    for l in &locked {
        if !same_registry(&l.source, &registry.index) {
            registry.warnings.push(format!("{} {}: locked to another registry ({}); not mirrored", l.name, l.version, l.source));
            continue;
        }
        if !valid_name(&l.name) {
            return Err(CargoError::Config(format!("lockfile names crate {:?}, which is not a valid name", l.name)));
        }
        let lines = registry.lines(&l.name)?.ok_or_else(|| CargoError::Unsatisfiable { krate: l.name.clone(), req: format!("={}", l.version) })?;
        let line = lines.iter().find(|x| x.vers_text == l.version).ok_or_else(|| CargoError::Unsatisfiable { krate: l.name.clone(), req: format!("={}", l.version) })?;
        if let Some(sum) = &l.checksum {
            if !sum.eq_ignore_ascii_case(&line.cksum.to_string()) {
                return Err(reg(format!("{} {}: the lockfile checksum differs from the one in the index", l.name, l.version)));
            }
        }
        selected.entry(l.name.to_ascii_lowercase()).or_default().insert(l.version.clone());
    }

    // 3. Download and lay out a sparse index.
    let previous: BTreeMap<String, Digest> = match st.head(repo)? {
        Some(h) => st.meta().read(|t| t.entries_at(repo_row.id, h.seq))?.into_iter().map(|e| (e.path, e.digest)).collect(),
        None => BTreeMap::new(),
    };
    let mut report = CargoReport { crates: selected.len(), ..Default::default() };
    let mut entries: BTreeMap<String, Digest> = BTreeMap::new();
    for (lower, versions) in &selected {
        let lines: Vec<Line> = registry.lines(lower)?.cloned().unwrap_or_default();
        let mut kept = String::new();
        for line in lines.iter().filter(|l| versions.contains(&l.vers_text)) {
            let path = format!("crates/{0}/{0}-{1}.crate", line.name, line.vers_text);
            if mer_core::validate_path(&path).is_err() {
                return Err(reg(format!("{} {}: cannot be stored under {path:?}", line.name, line.vers_text)));
            }
            if st.has_blob(&line.cksum)? {
                report.reused += 1;
            } else {
                let url = download_url(&dl, &line.name, &line.vers_text, &line.cksum.to_string());
                let mut body = client.get(&url, &[])?.into_reader().take(MAX_CRATE + 1);
                // The store refuses the file unless it hashes to the index checksum.
                let (_, size) = st.ingest(&mut body, mer_blob::Expect { digest: Some(line.cksum), size: None }).map_err(|e| match e {
                    StorageError::Store(mer_blob::StoreError::DigestMismatch { .. }) => reg(format!("{} {}: the download does not match the checksum in the index ({e})", line.name, line.vers_text)),
                    other => other.into(),
                })?;
                report.downloaded += 1;
                report.downloaded_bytes += size;
            }
            entries.insert(path, line.cksum);
            kept.push_str(&line.raw);
            kept.push('\n');
            report.versions += 1;
        }
        let (d, _) = st.ingest_bytes(kept.as_bytes())?;
        entries.insert(index_path(lower), d);
    }
    let config = json!({ "dl": format!("{BASE_PLACEHOLDER}crates/{{crate}}/{{crate}}-{{version}}.crate") });
    let (d, _) = st.ingest_bytes(config.to_string().as_bytes())?;
    entries.insert("config.json".into(), d);

    // 4. One snapshot.
    let mut writer = st.begin(repo)?;
    for (path, digest) in &entries {
        writer.put(path, EntryRole::Artifact, digest)?;
    }
    for gone in previous.keys().filter(|p| !entries.contains_key(*p)) {
        writer.remove(gone)?;
    }
    report.snapshot = writer.seal(None)?.map(|s| s.seq);
    registry.warnings.sort();
    registry.warnings.dedup();
    report.warnings = registry.warnings;
    Ok(report)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn names_paths_and_specs() {
        for ok in ["serde", "a", "serde_json", "tokio-util", "Inflector", "0x"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "-x", "_x", "a/b", "a.b", "../x", "a b", &"x".repeat(65)] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert_eq!(index_path("a"), "1/a");
        assert_eq!(index_path("ab"), "2/ab");
        assert_eq!(index_path("Abc"), "3/a/abc");
        assert_eq!(index_path("Serde"), "se/rd/serde");
        assert_eq!(split_spec("serde"), Some(("serde".into(), "*".into(), vec![])));
        assert_eq!(split_spec("tokio@1:full,macros"), Some(("tokio".into(), "1".into(), vec!["full".into(), "macros".into()])));
        assert_eq!(split_spec("rand@=0.8.5"), Some(("rand".into(), "=0.8.5".into(), vec![])));
        assert_eq!(split_spec("x@latest"), Some(("x".into(), "*".into(), vec![])));
        assert_eq!(split_spec("../evil"), None);
        assert_eq!(split_spec("x@not a req"), None);
    }

    #[test]
    fn download_urls_follow_the_dl_template() {
        let c = "ab".repeat(32);
        assert_eq!(download_url("https://static.crates.io/crates", "serde", "1.0.0", &c), "https://static.crates.io/crates/serde/1.0.0/download");
        assert_eq!(
            download_url("https://h/{prefix}/{lowerprefix}/{crate}-{version}.crate?h={sha256-checksum}", "Abcd", "0.1.0", &c),
            format!("https://h/Ab/cd/ab/cd/Abcd-0.1.0.crate?h={c}")
        );
        assert_eq!(loose_version("1.80"), Some(Version::new(1, 80, 0)));
        assert_eq!(loose_version("1.80.1"), Some(Version::new(1, 80, 1)));
        assert_eq!(loose_version("x"), None);
    }

    #[test]
    fn lockfile_sources() {
        assert!(same_registry("registry+https://github.com/rust-lang/crates.io-index", "https://index.crates.io/"));
        assert!(same_registry("sparse+https://index.crates.io/", "https://index.crates.io"));
        assert!(!same_registry("sparse+https://index.crates.io/", "https://my.corp/index/"));
        assert!(same_registry("sparse+https://my.corp/index/", "https://my.corp/index"));
    }
}
