//! Cargo registries (the sparse index protocol, as crates.io serves it).
//!
//! The collector resolves the wanted crates the way Cargo would: the highest
//! version that satisfies each requirement, normal and build dependencies,
//! and optional dependencies only where a feature turns them on. Every
//! `.crate` file is downloaded against the checksum in the index, and the
//! index files written list only what was mirrored.
//!
//! Which features a build inside the air gap will ask for is not known here.
//! So the crates named on the command line are mirrored with all their
//! features on (unless told otherwise), and dependencies for every target
//! platform are followed. That mirrors more than one build needs, not less.
#[cfg(test)]
mod tests;

use mer_blob::Expect;
use mer_core::{Digest, EntryRole};
use mer_fetch::{host_of, FetchError, HttpClient};
use mer_storage::{Storage, StorageError};
use semver::{Version, VersionReq};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Read;

const MAX_INDEX_FILE: u64 = 256 << 20;
const MAX_CRATE: u64 = 2 << 30;
const MAX_VERSIONS: usize = 200_000;

#[derive(Debug, thiserror::Error)]
pub enum CargoError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("upstream registry: {0}")]
    Registry(String),
    #[error("{0}")]
    Config(String),
    #[error("no version of crate {krate} satisfies {req:?}")]
    Unsatisfiable { krate: String, req: String },
}

impl From<mer_meta::MetaError> for CargoError {
    fn from(e: mer_meta::MetaError) -> Self {
        CargoError::Storage(e.into())
    }
}

#[derive(Clone, Debug)]
pub struct CargoConfig {
    /// The sparse index, for example `https://index.crates.io`.
    pub index_url: String,
    /// Crates to mirror: `serde` (newest stable), `tokio@1.38` (as in
    /// Cargo.toml: 1.38 or a compatible later version), `rand@=0.8.5`.
    pub crates: Vec<String>,
    /// Exact (crate, version) pairs, usually read from Cargo.lock files, so
    /// that `cargo build --locked` works inside the air gap. A lock file is
    /// complete, so nothing further is followed from these.
    pub pins: Vec<(String, String)>,
    /// Mirror the named crates with only their default features, plus
    /// `features`, instead of with every feature.
    pub default_features_only: bool,
    /// Extra features of named crates, as `crate/feature`.
    pub features: Vec<String>,
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

/// Crate names: ASCII letters, digits, `-` and `_`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// Where the index keeps the file of a crate: `1/a`, `2/ab`, `3/a/abc`, `ab/cd/abcdef`.
pub fn index_path(name: &str) -> String {
    let n = name.to_ascii_lowercase();
    match n.len() {
        1 => format!("1/{n}"),
        2 => format!("2/{n}"),
        3 => format!("3/{}/{n}", &n[..1]),
        _ => format!("{}/{}/{n}", &n[..2], &n[2..4]),
    }
}

/// The registry packages a Cargo.lock names, as (crate, version).
pub fn parse_lockfile(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let (mut name, mut version, mut registry) = (None, None, false);
    let mut flush = |name: &mut Option<String>, version: &mut Option<String>, registry: &mut bool| {
        if let (Some(n), Some(v), true) = (name.take(), version.take(), *registry) {
            out.push((n, v));
        }
        *registry = false;
    };
    let value = |line: &str, key: &str| -> Option<String> {
        let rest = line.strip_prefix(key)?.trim_start().strip_prefix('=')?.trim();
        Some(rest.strip_prefix('"')?.strip_suffix('"')?.to_string())
    };
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            flush(&mut name, &mut version, &mut registry);
        } else if let Some(v) = value(line, "name") {
            name = Some(v);
        } else if let Some(v) = value(line, "version") {
            version = Some(v);
        } else if let Some(v) = value(line, "source") {
            registry = v.starts_with("registry+") || v.starts_with("sparse+");
        }
    }
    flush(&mut name, &mut version, &mut registry);
    out
}

/// A dependency as an index line states it.
#[derive(Clone, Debug)]
struct Dep {
    /// The name the depending crate uses for it (differs when renamed).
    alias: String,
    /// The crate it is.
    krate: String,
    req: String,
    features: Vec<String>,
    default_features: bool,
    optional: bool,
}

/// One version of a crate, as its line in the index says.
#[derive(Clone, Debug)]
struct Line {
    raw: String,
    name: String,
    vers: Version,
    yanked: bool,
    cksum: Digest,
    /// Normal and build dependencies in this registry.
    deps: Vec<Dep>,
    features: BTreeMap<String, Vec<String>>,
}

fn parse_line(raw: &str, warnings: &mut Vec<String>) -> Option<Line> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let name = v["name"].as_str().filter(|n| valid_name(n))?.to_string();
    let vers = Version::parse(v["vers"].as_str()?).ok()?;
    let cksum = Digest::from_hex(v["cksum"].as_str()?).ok()?;
    let strings = |v: &Value| -> Vec<String> { v.as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect()).unwrap_or_default() };
    let mut deps = Vec::new();
    for d in v["deps"].as_array().map(Vec::as_slice).unwrap_or_default() {
        if d["kind"].as_str() == Some("dev") {
            continue; // only needed to test the crate itself
        }
        let Some(alias) = d["name"].as_str() else { continue };
        // A renamed dependency keeps its real name in "package".
        let Some(krate) = d["package"].as_str().or(Some(alias)).filter(|n| valid_name(n)) else { continue };
        if d["registry"].as_str().is_some() {
            warnings.push(format!("{name} {vers}: dependency {krate} lives in another registry and is not followed"));
            continue;
        }
        deps.push(Dep {
            alias: alias.to_string(),
            krate: krate.to_string(),
            req: d["req"].as_str().unwrap_or("*").to_string(),
            features: strings(&d["features"]),
            default_features: d["default_features"].as_bool().unwrap_or(true),
            optional: d["optional"].as_bool().unwrap_or(false),
        });
    }
    // Newer feature syntax lives in "features2"; together they are one table.
    let mut features: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for table in ["features", "features2"] {
        for (k, list) in v[table].as_object().into_iter().flatten() {
            features.entry(k.clone()).or_default().extend(strings(list));
        }
    }
    Some(Line { raw: raw.to_string(), name, vers, yanked: v["yanked"].as_bool().unwrap_or(false), cksum, deps, features })
}

/// A crate version being mirrored, and what is switched on in it.
struct Node {
    line: Line,
    features: BTreeSet<String>,
    /// Aliases of the optional dependencies that are switched on.
    optional_on: BTreeSet<String>,
    /// `dep?/feature`: (alias, feature) to pass on if that dependency is on.
    weak: Vec<(String, String)>,
    /// A locked version: mirrored as is, nothing followed.
    pinned_only: bool,
}

enum Want {
    Req(String),
    Exact(String),
}

/// Something to switch on, waiting to be worked through.
struct Ask {
    krate: String,
    want: Want,
    features: Vec<String>,
    default_features: bool,
    /// Every feature the crate has (for crates named on the command line).
    all_features: bool,
    root: bool,
}

/// Mirror the configured crates and what they depend on into a new snapshot.
///
/// What is trusted is the TLS connection to the index and the SHA-256 it
/// states for each crate file, which is also what Cargo itself checks.
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &CargoConfig) -> Result<CargoReport, CargoError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "cargo" {
        return Err(CargoError::Config(format!("repo {repo} has format {}, not cargo", repo_row.format)));
    }
    if host_of(&cfg.index_url).is_none() {
        return Err(CargoError::Config(format!("{:?} is not an http(s) URL", cfg.index_url)));
    }
    if cfg.crates.is_empty() && cfg.pins.is_empty() {
        return Err(CargoError::Config("at least one crate or lock file is needed".into()));
    }
    let base = cfg.index_url.trim_end_matches('/').to_string();
    let config_url = format!("{base}/config.json");
    let config: Value = serde_json::from_slice(&client.get(&config_url, &[])?.bytes(&config_url, 1 << 20)?)
        .map_err(|e| CargoError::Registry(format!("config.json does not parse: {e}")))?;
    let dl = config["dl"].as_str().ok_or_else(|| CargoError::Registry("config.json names no download URL".into()))?.to_string();

    let mut report = CargoReport::default();
    let mut extra: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in &cfg.features {
        let (krate, feature) = f.split_once('/').ok_or_else(|| CargoError::Config(format!("feature {f:?} must be written crate/feature")))?;
        extra.entry(krate.trim().to_ascii_lowercase()).or_default().push(feature.trim().to_string());
    }
    let mut queue: VecDeque<Ask> = VecDeque::new();
    for spec in &cfg.crates {
        let (name, req) = match spec.trim().split_once('@') {
            Some((n, r)) => (n.trim(), r.trim()),
            None => (spec.trim(), "*"),
        };
        if !valid_name(name) {
            return Err(CargoError::Config(format!("crate name {name:?} is not acceptable")));
        }
        VersionReq::parse(req).map_err(|e| CargoError::Config(format!("{spec}: {e}")))?;
        queue.push_back(Ask {
            krate: name.to_string(),
            want: Want::Req(req.to_string()),
            features: extra.remove(&name.to_ascii_lowercase()).unwrap_or_default(),
            default_features: true,
            all_features: !cfg.default_features_only,
            root: true,
        });
    }
    if let Some(unused) = extra.keys().next() {
        return Err(CargoError::Config(format!("a feature was given for {unused}, which is not one of the crates to mirror")));
    }
    for (name, version) in &cfg.pins {
        if !valid_name(name) || Version::parse(version).is_err() {
            return Err(CargoError::Config(format!("lock file entry {name} {version} is not acceptable")));
        }
        queue.push_back(Ask { krate: name.clone(), want: Want::Exact(version.clone()), features: Vec::new(), default_features: false, all_features: false, root: true });
    }

    // Index files read so far, by lower-case crate name. None: no such crate.
    let mut index: BTreeMap<String, Option<Vec<Line>>> = BTreeMap::new();
    // What each (crate, requirement) resolved to, so it is decided once.
    let mut resolved: BTreeMap<(String, String), Option<Version>> = BTreeMap::new();
    let mut chosen: BTreeMap<String, BTreeMap<Version, Node>> = BTreeMap::new();
    let mut versions = 0usize;
    while let Some(ask) = queue.pop_front() {
        let key = ask.krate.to_ascii_lowercase();
        let want_text = match &ask.want {
            Want::Req(r) => r.clone(),
            Want::Exact(v) => format!("={v}"),
        };
        if !index.contains_key(&key) {
            let url = format!("{base}/{}", index_path(&key));
            let lines = match client.get(&url, &[]) {
                Ok(resp) => {
                    let body = resp.bytes(&url, MAX_INDEX_FILE)?;
                    let text = String::from_utf8_lossy(&body);
                    Some(text.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| parse_line(l, &mut Vec::new())).collect())
                }
                Err(FetchError::NotFound(_)) => None,
                Err(e) => return Err(e.into()),
            };
            index.insert(key.clone(), lines);
        }
        let lines = index[&key].as_deref().unwrap_or_default();
        let first_time = !resolved.contains_key(&(key.clone(), want_text.clone()));
        let version = resolved.entry((key.clone(), want_text.clone())).or_insert_with(|| match &ask.want {
            Want::Exact(v) => Version::parse(v).ok().filter(|v| lines.iter().any(|l| &l.vers == v)),
            // Cargo's choice: the highest version that is not yanked.
            Want::Req(r) => VersionReq::parse(r).ok().and_then(|req| lines.iter().filter(|l| !l.yanked && req.matches(&l.vers)).map(|l| l.vers.clone()).max()),
        });
        let Some(version) = version.clone() else {
            if ask.root {
                return Err(CargoError::Unsatisfiable { krate: ask.krate, req: want_text });
            }
            if first_time {
                report.warnings.push(format!("no version of {} satisfies {want_text:?}; skipped", ask.krate));
            }
            continue;
        };
        let line = lines.iter().find(|l| l.vers == version).expect("resolved from these lines");
        let slot = chosen.entry(line.name.clone()).or_default();
        let pinned = matches!(ask.want, Want::Exact(_));
        if !slot.contains_key(&version) {
            versions += 1;
            if versions > MAX_VERSIONS {
                return Err(CargoError::Registry(format!("more than {MAX_VERSIONS} crate versions to mirror")));
            }
            // Read the line again, this time keeping what it has to say.
            let line = parse_line(&line.raw, &mut report.warnings).expect("parsed before");
            slot.insert(version.clone(), Node { line, features: BTreeSet::new(), optional_on: BTreeSet::new(), weak: Vec::new(), pinned_only: true });
        }
        let node = slot.get_mut(&version).expect("just ensured");
        if pinned {
            continue;
        }
        let depend = |d: &Dep, more: Vec<String>| Ask {
            krate: d.krate.clone(),
            want: Want::Req(d.req.clone()),
            features: d.features.iter().cloned().chain(more).collect(),
            default_features: d.default_features,
            all_features: false,
            root: false,
        };
        if node.pinned_only {
            // First visit as part of a resolve: what it always depends on.
            node.pinned_only = false;
            queue.extend(node.line.deps.iter().filter(|d| !d.optional).map(|d| depend(d, Vec::new())));
        }
        let mut todo: Vec<String> = ask.features.clone();
        if ask.default_features {
            todo.push("default".into());
        }
        if ask.all_features {
            todo.extend(node.line.features.keys().cloned());
            todo.extend(node.line.deps.iter().filter(|d| d.optional).map(|d| d.alias.clone()));
        }
        // Switch features on, following what each one switches on in turn.
        while let Some(feature) = todo.pop() {
            if !node.features.insert(feature.clone()) {
                continue;
            }
            let mut turn_on: Vec<(String, Option<String>)> = Vec::new(); // (dependency alias, feature to pass on)
            match node.line.features.get(&feature) {
                Some(items) => {
                    for item in items {
                        if let Some(dep) = item.strip_prefix("dep:") {
                            turn_on.push((dep.to_string(), None));
                        } else if let Some((dep, f)) = item.split_once("?/") {
                            if node.optional_on.contains(dep) || node.line.deps.iter().any(|d| d.alias == dep && !d.optional) {
                                turn_on.push((dep.to_string(), Some(f.to_string())));
                            } else {
                                node.weak.push((dep.to_string(), f.to_string()));
                            }
                        } else if let Some((dep, f)) = item.split_once('/') {
                            turn_on.push((dep.to_string(), Some(f.to_string())));
                        } else {
                            todo.push(item.clone());
                        }
                    }
                }
                // An optional dependency is a feature of the same name.
                None => turn_on.push((feature.clone(), None)),
            }
            for (alias, pass) in turn_on {
                let newly_on = node.line.deps.iter().any(|d| d.alias == alias && d.optional) && node.optional_on.insert(alias.clone());
                let mut more: Vec<String> = pass.into_iter().collect();
                if newly_on {
                    more.extend(node.weak.iter().filter(|(a, _)| *a == alias).map(|(_, f)| f.clone()));
                } else if more.is_empty() {
                    continue; // nothing new to say to this dependency
                }
                queue.extend(node.line.deps.iter().filter(|d| d.alias == alias).map(|d| depend(d, more.clone())));
            }
        }
    }

    let mut entries: BTreeMap<String, Digest> = BTreeMap::new();
    for (name, versions) in &chosen {
        let mut text = String::new();
        for line in versions.values().map(|n| &n.line) {
            if st.has_blob(&line.cksum)? {
                report.reused += 1;
            } else {
                let url = download_url(&dl, line);
                let mut body = client.get(&url, &[])?.into_reader().take(MAX_CRATE + 1);
                let (_, size) = st.ingest(&mut body, Expect { digest: Some(line.cksum), size: None })?;
                report.downloaded += 1;
                report.downloaded_bytes += size;
            }
            entries.insert(format!("crates/{name}/{name}-{}.crate", line.vers), line.cksum);
            text.push_str(&line.raw);
            text.push('\n');
        }
        let (d, _) = st.ingest_bytes(text.as_bytes())?;
        entries.insert(format!("index/{}", index_path(name)), d);
        report.versions += versions.len();
    }
    report.crates = chosen.len();

    let previous: Vec<String> = match st.head(repo)? {
        Some(h) => st.meta().read(|t| t.entries_at(repo_row.id, h.seq))?.into_iter().map(|e| e.path).collect(),
        None => Vec::new(),
    };
    let mut writer = st.begin(repo)?;
    for (path, digest) in &entries {
        writer.put(path, EntryRole::Artifact, digest)?;
    }
    for gone in previous.iter().filter(|p| !entries.contains_key(*p)) {
        writer.remove(gone)?;
    }
    report.snapshot = writer.seal(None)?.map(|s| s.seq);
    Ok(report)
}

/// The download URL template of the registry, filled in the way Cargo does.
fn download_url(dl: &str, line: &Line) -> String {
    let markers = ["{crate}", "{version}", "{prefix}", "{lowerprefix}", "{sha256-checksum}"];
    if !markers.iter().any(|m| dl.contains(m)) {
        return format!("{}/{}/{}/download", dl.trim_end_matches('/'), line.name, line.vers);
    }
    let prefix = |name: &str| index_path(name).rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
    // The prefix keeps the case of the name; index_path lower-cases, so build it from the name itself.
    let cased_prefix = match line.name.len() {
        1 => "1".to_string(),
        2 => "2".to_string(),
        3 => format!("3/{}", &line.name[..1]),
        _ => format!("{}/{}", &line.name[..2], &line.name[2..4]),
    };
    dl.replace("{crate}", &line.name)
        .replace("{version}", &line.vers.to_string())
        .replace("{prefix}", &cased_prefix)
        .replace("{lowerprefix}", &prefix(&line.name))
        .replace("{sha256-checksum}", &line.cksum.to_hex())
}
