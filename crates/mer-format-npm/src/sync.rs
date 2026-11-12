//! Collector-side sync of an npm registry into one repo snapshot.
use crate::{Range, Version};
use base64::Engine;
use mer_core::{Digest, EntryRole};
use mer_fetch::{host_of, FetchError, HttpClient};
use mer_storage::{Storage, StorageError};
use serde_json::{json, Map, Value};
use sha1::Sha1;
use sha2::{Digest as _, Sha512};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Read;

/// What stands in for "this server's address" inside stored package
/// documents. The vault replaces it with its own URL when it serves them.
pub const BASE_PLACEHOLDER: &str = "mer://base/";

const ABBREVIATED: &str = "application/vnd.npm.install-v1+json";
const MAX_PACKUMENT: u64 = 512 << 20;
const MAX_TARBALL: u64 = 4 << 30;
const MAX_VERSIONS: usize = 50_000;

#[derive(Debug, thiserror::Error)]
pub enum NpmError {
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
    #[error("no version of {package} satisfies {range:?}")]
    Unsatisfiable { package: String, range: String },
}

impl From<mer_meta::MetaError> for NpmError {
    fn from(e: mer_meta::MetaError) -> Self {
        NpmError::Storage(e.into())
    }
}

#[derive(Clone, Debug)]
pub struct NpmConfig {
    /// For example `https://registry.npmjs.org`.
    pub registry: String,
    /// What to mirror: `express`, `lodash@^4.17`, `@types/node@20`, `typescript@latest`.
    pub packages: Vec<String>,
    /// Platform of the machines inside the air gap, in Node's words.
    pub os: String,
    pub cpu: String,
    pub libc: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NpmReport {
    pub packages: usize,
    pub versions: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub reused: usize,
    pub warnings: Vec<String>,
    pub snapshot: Option<u64>,
}

fn reg(msg: impl Into<String>) -> NpmError {
    NpmError::Registry(msg.into())
}

/// npm package names: an optional `@scope/`, then URL-safe characters.
pub fn valid_name(name: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty()
            && !p.starts_with(['.', '_'])
            && p.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'))
    };
    if name.len() > 214 {
        return false;
    }
    match name.strip_prefix('@') {
        Some(rest) => rest.split_once('/').is_some_and(|(scope, pkg)| part(scope) && part(pkg)),
        None => part(name),
    }
}

/// `name`, `name@range`, `@scope/name@range` -> (name, range text).
fn split_spec(spec: &str) -> Option<(&str, &str)> {
    let spec = spec.trim();
    let at = spec.char_indices().skip(1).find(|(_, c)| *c == '@').map(|(i, _)| i);
    let (name, range) = match at {
        Some(i) => (&spec[..i], &spec[i + 1..]),
        None => (spec, "latest"),
    };
    valid_name(name).then_some((name, range))
}

/// Does a package's `os` / `cpu` / `libc` list allow this target?
fn platform_allows(list: Option<&Value>, target: &str) -> bool {
    let Some(items) = list.and_then(Value::as_array) else { return true };
    let items: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
    if items.is_empty() {
        return true;
    }
    if items.iter().any(|i| i.strip_prefix('!') == Some(target)) {
        return false;
    }
    let positives: Vec<&str> = items.iter().copied().filter(|i| !i.starts_with('!')).collect();
    positives.is_empty() || positives.contains(&target)
}

/// Hashes a tarball while it streams into the store.
struct Hashing<R> {
    inner: R,
    sha512: Sha512,
    sha1: Sha1,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.sha512.update(&buf[..n]);
        self.sha1.update(&buf[..n]);
        Ok(n)
    }
}

/// What the registry says a tarball must hash to.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Integrity {
    Sha512(Vec<u8>),
    Sha1(Vec<u8>),
}

fn integrity_of(dist: &Value) -> Option<Integrity> {
    let b64 = base64::engine::general_purpose::STANDARD;
    if let Some(sri) = dist.get("integrity").and_then(Value::as_str) {
        for item in sri.split_whitespace() {
            if let Some(h) = item.strip_prefix("sha512-") {
                return b64.decode(h.split('?').next()?).ok().filter(|b| b.len() == 64).map(Integrity::Sha512);
            }
        }
        for item in sri.split_whitespace() {
            if let Some(h) = item.strip_prefix("sha1-") {
                return b64.decode(h).ok().filter(|b| b.len() == 20).map(Integrity::Sha1);
            }
        }
    }
    // Old packages only carry a hex SHA-1.
    let hex = dist.get("shasum").and_then(Value::as_str)?;
    hex::decode(hex).ok().filter(|b| b.len() == 20).map(Integrity::Sha1)
}

fn tarball_file(dist: &Value) -> Option<(String, String)> {
    let url = dist.get("tarball").and_then(Value::as_str)?;
    let file = url.split(['?', '#']).next()?.rsplit('/').next()?;
    let ok = file.ends_with(".tgz")
        && file.len() <= 255
        && !file.starts_with('.')
        && file.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'));
    ok.then(|| (url.to_string(), file.to_string()))
}

struct Resolver<'a> {
    client: &'a HttpClient,
    cfg: &'a NpmConfig,
    packuments: BTreeMap<String, Value>,
    warnings: Vec<String>,
}

impl Resolver<'_> {
    fn packument(&mut self, name: &str) -> Result<Option<&Value>, NpmError> {
        if !self.packuments.contains_key(name) {
            let url = format!("{}/{}", self.cfg.registry.trim_end_matches('/'), name.replace('/', "%2f"));
            let body = match self.client.get(&url, &[("Accept", ABBREVIATED)]) {
                Ok(r) => r.bytes(&url, MAX_PACKUMENT)?,
                Err(FetchError::NotFound(_)) => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            let doc: Value = serde_json::from_slice(&body).map_err(|_| reg(format!("{url}: not JSON")))?;
            if !doc.get("versions").is_some_and(Value::is_object) {
                return Err(reg(format!("{url}: no version list")));
            }
            self.packuments.insert(name.to_string(), doc);
        }
        Ok(self.packuments.get(name))
    }

    /// Resolve a dependency spec to (real package name, version).
    fn resolve(&mut self, name: &str, spec: &str) -> Result<Result<(String, String), String>, NpmError> {
        // "npm:other@range": the dependency is installed under another name.
        if let Some(alias) = spec.strip_prefix("npm:") {
            return match split_spec(alias) {
                Some((real, range)) => {
                    let (real, range) = (real.to_string(), range.to_string());
                    self.resolve(&real, &range)
                }
                None => Ok(Err(format!("alias {spec:?} is not understood"))),
            };
        }
        let Some(doc) = self.packument(name)? else {
            return Ok(Err("does not exist in the registry".into()));
        };
        let versions = doc["versions"].as_object().expect("checked when fetched");
        let tags = doc.get("dist-tags").and_then(Value::as_object);
        // A dist-tag such as "latest" or "next" names one exact version.
        if let Some(tagged) = tags.and_then(|t| t.get(spec)).and_then(Value::as_str) {
            return Ok(if versions.contains_key(tagged) {
                Ok((name.to_string(), tagged.to_string()))
            } else {
                Err(format!("tag {spec:?} points at {tagged}, which the registry does not list"))
            });
        }
        let Some(range) = Range::parse(spec) else {
            return Ok(Err(format!("{spec:?} is not a registry version range (git, file and URL dependencies cannot be mirrored)")));
        };
        let parsed: Vec<(Version, &String)> = versions.keys().filter_map(|k| Version::parse(k).map(|v| (v, k))).collect();
        // Like npm: the version tagged "latest" wins if it fits, else the newest that fits.
        let latest = tags.and_then(|t| t.get("latest")).and_then(Value::as_str);
        if let Some(l) = latest.and_then(Version::parse).filter(|v| range.satisfied_by(v)) {
            if let Some((_, key)) = parsed.iter().find(|(v, _)| *v == l) {
                return Ok(Ok((name.to_string(), (*key).clone())));
            }
        }
        Ok(match parsed.iter().filter(|(v, _)| range.satisfied_by(v)).max_by(|a, b| a.0.cmp(&b.0)) {
            Some((_, key)) => Ok((name.to_string(), (*key).clone())),
            None => Err(format!("no version satisfies {spec:?}")),
        })
    }
}

/// Mirror the configured packages and everything they depend on into a new
/// snapshot of `repo`.
///
/// What is trusted is the TLS connection to the registry and the integrity
/// hash it states for each tarball; every download is checked against that
/// hash. Registry signatures are not verified.
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &NpmConfig) -> Result<NpmReport, NpmError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "npm" {
        return Err(NpmError::Config(format!("repo {repo} has format {}, not npm", repo_row.format)));
    }
    if host_of(&cfg.registry).is_none() {
        return Err(NpmError::Config(format!("{:?} is not an http(s) URL", cfg.registry)));
    }
    if cfg.packages.is_empty() {
        return Err(NpmError::Config("at least one package is needed".into()));
    }
    let mut r = Resolver { client, cfg, packuments: BTreeMap::new(), warnings: Vec::new() };

    // 1. Resolve roots and walk dependency ranges. Unlike Python, npm installs
    //    several versions of one package side by side, so every distinct
    //    (package, range) is resolved on its own.
    let mut queue: VecDeque<(String, String, bool)> = VecDeque::new(); // (name, spec, optional)
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut selected: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for spec in &cfg.packages {
        let (name, range) = split_spec(spec).ok_or_else(|| NpmError::Config(format!("package spec {spec:?} is not understood")))?;
        match r.resolve(name, range)? {
            Ok(_) => queue.push_back((name.to_string(), range.to_string(), false)),
            Err(_) => return Err(NpmError::Unsatisfiable { package: name.to_string(), range: range.to_string() }),
        }
    }
    let mut total = 0usize;
    while let Some((name, spec, optional)) = queue.pop_front() {
        if !seen.insert((name.clone(), spec.clone())) {
            continue;
        }
        if !valid_name(&name) {
            r.warnings.push(format!("dependency name {name:?} is not acceptable; skipped"));
            continue;
        }
        let (real, version) = match r.resolve(&name, &spec)? {
            Ok(found) => found,
            Err(why) => {
                if !optional {
                    r.warnings.push(format!("{name}@{spec}: {why}; not mirrored"));
                }
                continue;
            }
        };
        let object = r.packuments[&real]["versions"][&version].clone();
        // Packages built for another operating system or CPU are left out.
        let fits = platform_allows(object.get("os"), &cfg.os)
            && platform_allows(object.get("cpu"), &cfg.cpu)
            && platform_allows(object.get("libc"), &cfg.libc);
        if !fits {
            if !optional {
                r.warnings.push(format!("{real}@{version} does not support {}/{}; not mirrored", cfg.os, cfg.cpu));
            }
            continue;
        }
        if !selected.entry(real.clone()).or_default().insert(version.clone()) {
            continue;
        }
        total += 1;
        if total > MAX_VERSIONS {
            return Err(reg("dependency tree is implausibly large"));
        }
        let optional_names: BTreeSet<&String> = object.get("optionalDependencies").and_then(Value::as_object).map(|m| m.keys().collect()).unwrap_or_default();
        let optional_peers: BTreeSet<&String> = object
            .get("peerDependenciesMeta")
            .and_then(Value::as_object)
            .map(|m| m.iter().filter(|(_, v)| v.get("optional").and_then(Value::as_bool) == Some(true)).map(|(k, _)| k).collect())
            .unwrap_or_default();
        for field in ["dependencies", "optionalDependencies", "peerDependencies"] {
            let Some(deps) = object.get(field).and_then(Value::as_object) else { continue };
            for (dep, range) in deps {
                let Some(range) = range.as_str() else { continue };
                let opt = field == "optionalDependencies" || optional_names.contains(dep) || (field == "peerDependencies" && optional_peers.contains(dep));
                if field == "peerDependencies" && optional_peers.contains(dep) {
                    continue; // npm does not install optional peers on its own
                }
                queue.push_back((dep.clone(), range.to_string(), opt));
            }
        }
    }

    // 2. Download tarballs and write package documents.
    let previous: BTreeMap<String, Digest> = match st.head(repo)? {
        Some(h) => st.meta().read(|t| t.entries_at(repo_row.id, h.seq))?.into_iter().map(|e| (e.path, e.digest)).collect(),
        None => BTreeMap::new(),
    };
    let mut report = NpmReport { packages: selected.len(), ..Default::default() };
    let mut entries: BTreeMap<String, Digest> = BTreeMap::new();
    for (name, versions) in &selected {
        let doc = &r.packuments[name];
        let doc_path = format!("packuments/{name}.json");
        // What the previous snapshot recorded, to avoid downloading again.
        let old_doc: Option<Value> = previous.get(&doc_path).and_then(|d| st.read_blob(d).ok()).and_then(|b| serde_json::from_slice(&b).ok());
        let mut out_versions = Map::new();
        for version in versions {
            let mut object = doc["versions"][version].clone();
            let dist = object.get("dist").cloned().unwrap_or(Value::Null);
            let (Some((url, file)), Some(integrity)) = (tarball_file(&dist), integrity_of(&dist)) else {
                r.warnings.push(format!("{name}@{version}: the registry gives no usable tarball or hash; skipped"));
                continue;
            };
            let path = format!("tarballs/{name}/{file}");
            let unchanged = old_doc.as_ref().and_then(|o| o["versions"].get(version)).and_then(|o| o.get("dist")).is_some_and(|d| integrity_of(d).as_ref() == Some(&integrity));
            let digest = match previous.get(&path) {
                Some(d) if unchanged && st.has_blob(d)? => {
                    report.reused += 1;
                    *d
                }
                _ => {
                    let body = client.get(&url, &[])?.into_reader().take(MAX_TARBALL + 1);
                    let mut hashing = Hashing { inner: body, sha512: Sha512::new(), sha1: Sha1::new() };
                    let (digest, size) = st.ingest(&mut hashing, mer_blob::Expect::default())?;
                    let good = match &integrity {
                        Integrity::Sha512(want) => hashing.sha512.finalize().as_slice() == want.as_slice(),
                        Integrity::Sha1(want) => hashing.sha1.finalize().as_slice() == want.as_slice(),
                    };
                    if !good || size > MAX_TARBALL {
                        // The bytes are in the store but nothing refers to them;
                        // garbage collection removes them.
                        return Err(reg(format!("{name}@{version}: the tarball does not match the integrity hash of the registry")));
                    }
                    report.downloaded += 1;
                    report.downloaded_bytes += size;
                    digest
                }
            };
            entries.insert(path, digest);
            object["dist"]["tarball"] = json!(format!("{BASE_PLACEHOLDER}{name}/-/{file}"));
            out_versions.insert(version.clone(), object);
            report.versions += 1;
        }
        if out_versions.is_empty() {
            continue;
        }
        // "latest" must point at something the vault actually has.
        let upstream_latest = doc.pointer("/dist-tags/latest").and_then(Value::as_str).filter(|l| out_versions.contains_key(*l));
        let latest = upstream_latest.map(str::to_string).unwrap_or_else(|| {
            let mut have: Vec<(Version, &String)> = out_versions.keys().filter_map(|k| Version::parse(k).map(|v| (v, k))).collect();
            have.sort_by(|a, b| a.0.cmp(&b.0));
            let stable = have.iter().rev().find(|(v, _)| v.pre.is_empty()).or(have.last());
            stable.map(|(_, k)| (*k).clone()).unwrap_or_default()
        });
        let packument = json!({ "name": name, "dist-tags": { "latest": latest }, "versions": out_versions });
        let (d, _) = st.ingest_bytes(packument.to_string().as_bytes())?;
        entries.insert(doc_path, d);
    }

    // 3. One snapshot.
    let mut writer = st.begin(repo)?;
    for (path, digest) in &entries {
        writer.put(path, EntryRole::Artifact, digest)?;
    }
    for gone in previous.keys().filter(|p| !entries.contains_key(*p)) {
        writer.remove(gone)?;
    }
    report.snapshot = writer.seal(None)?.map(|s| s.seq);
    r.warnings.sort();
    r.warnings.dedup();
    report.warnings = r.warnings;
    Ok(report)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn names_and_specs() {
        for ok in ["express", "lodash.merge", "@types/node", "@babel/core", "a-b_c~d", "JSONStream"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", ".hidden", "_private", "@scope", "@/x", "@scope/", "a/b", "@a/b/c", "a b", "../x", "@scope/..", &"x".repeat(215)] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert_eq!(split_spec("express"), Some(("express", "latest")));
        assert_eq!(split_spec("lodash@^4.17"), Some(("lodash", "^4.17")));
        assert_eq!(split_spec("@types/node@20"), Some(("@types/node", "20")));
        assert_eq!(split_spec("@types/node"), Some(("@types/node", "latest")));
        assert_eq!(split_spec("../evil@1"), None);
    }

    #[test]
    fn platform_lists() {
        let l = |items: &[&str]| json!(items);
        assert!(platform_allows(None, "linux"));
        assert!(platform_allows(Some(&l(&[])), "linux"));
        assert!(platform_allows(Some(&l(&["linux", "darwin"])), "linux"));
        assert!(!platform_allows(Some(&l(&["win32"])), "linux"));
        assert!(platform_allows(Some(&l(&["!win32"])), "linux"));
        assert!(!platform_allows(Some(&l(&["!linux"])), "linux"));
        assert!(!platform_allows(Some(&l(&["darwin", "!linux"])), "linux"));
    }

    #[test]
    fn integrity_forms() {
        let b64 = base64::engine::general_purpose::STANDARD;
        let s512 = b64.encode([7u8; 64]);
        assert_eq!(integrity_of(&json!({ "integrity": format!("sha512-{s512}") })), Some(Integrity::Sha512(vec![7; 64])));
        assert_eq!(integrity_of(&json!({ "integrity": format!("sha1-{} sha512-{s512}", b64.encode([1u8; 20])) })), Some(Integrity::Sha512(vec![7; 64])));
        assert_eq!(integrity_of(&json!({ "shasum": "0101010101010101010101010101010101010101" })), Some(Integrity::Sha1(vec![1; 20])));
        assert_eq!(integrity_of(&json!({ "integrity": "sha512-short" })), None);
        assert_eq!(integrity_of(&json!({ "integrity": "md5-abc" })), None);
        assert_eq!(integrity_of(&json!({})), None);
        assert!(tarball_file(&json!({ "tarball": "https://r/x/-/x-1.0.0.tgz" })).is_some());
        for bad in ["https://r/x/-/x.zip", "https://r/x/-/..tgz", "https://r/x/-/a%2fb.tgz", "https://r/x/-/<s>.tgz"] {
            assert!(tarball_file(&json!({ "tarball": bad })).is_none(), "{bad}");
        }
    }
}
