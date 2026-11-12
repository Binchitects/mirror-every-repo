//! Collector-side sync of a NuGet V3 feed into one repo snapshot.
use crate::{Framework, Range, Version};
use base64::Engine;
use mer_core::{Digest, EntryRole};
use mer_fetch::{host_of, FetchError, HttpClient};
use mer_storage::{Storage, StorageError};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha512};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Read;

/// Stands in for "this server's address" in the stored service index. The
/// vault replaces it with its own URL when it serves the file.
pub const BASE_PLACEHOLDER: &str = "mer://base/";

const MAX_JSON: u64 = 256 << 20;
const MAX_PACKAGE: u64 = 8 << 30;
const MAX_VERSIONS: usize = 50_000;

#[derive(Debug, thiserror::Error)]
pub enum NugetError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("upstream feed: {0}")]
    Feed(String),
    #[error("{0}")]
    Config(String),
    #[error("no version of {package} satisfies {range:?}")]
    Unsatisfiable { package: String, range: String },
}

impl From<mer_meta::MetaError> for NugetError {
    fn from(e: mer_meta::MetaError) -> Self {
        NugetError::Storage(e.into())
    }
}

#[derive(Clone, Debug)]
pub struct NugetConfig {
    /// Service index of the upstream feed, for example
    /// `https://api.nuget.org/v3/index.json`.
    pub service_index: String,
    /// What to mirror: `Serilog` (latest stable), `Newtonsoft.Json@13.0.3`
    /// (13.0.3 or the next higher version that exists), `Foo@[1.0,2.0)`.
    pub packages: Vec<String>,
    /// Target frameworks of the projects inside the air gap: `net8.0`, `netstandard2.0`, `net48`.
    pub frameworks: Vec<String>,
    /// Let a package given without a version resolve to a pre-release.
    pub prerelease: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NugetReport {
    pub packages: usize,
    pub versions: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub reused: usize,
    /// Packages for which the feed offered no hash to check against.
    pub unverified: usize,
    pub warnings: Vec<String>,
    pub snapshot: Option<u64>,
}

fn feed(msg: impl Into<String>) -> NugetError {
    NugetError::Feed(msg.into())
}

/// Package ids: letters, digits, `.`, `_`, `-`.
pub fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 100
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && !id.contains("..")
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Want {
    /// Newest listed stable version (a package named without a version).
    Latest,
    /// NuGet's rule for ranges: the lowest version that satisfies it.
    Range(String),
}

fn split_spec(spec: &str) -> Option<(&str, Want)> {
    let spec = spec.trim();
    let (id, want) = match spec.split_once('@') {
        Some((id, range)) => (id, Want::Range(range.trim().to_string())),
        None => (spec, Want::Latest),
    };
    valid_id(id).then_some((id, want))
}

struct Hashing<R> {
    inner: R,
    sha512: Sha512,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.sha512.update(&buf[..n]);
        Ok(n)
    }
}

struct Feed<'a> {
    client: &'a HttpClient,
    flat: String,
    registration: Option<String>,
    versions: BTreeMap<String, Option<Vec<(Version, String)>>>,
    docs: BTreeMap<String, Value>,
    warnings: Vec<String>,
}

impl Feed<'_> {
    fn json(&mut self, url: &str) -> Result<Option<&Value>, NugetError> {
        if !self.docs.contains_key(url) {
            let body = match self.client.get(url, &[("Accept", "application/json")]) {
                Ok(r) => r.bytes(url, MAX_JSON)?,
                Err(FetchError::NotFound(_)) => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            let doc: Value = serde_json::from_slice(&body).map_err(|_| feed(format!("{url}: not JSON")))?;
            self.docs.insert(url.to_string(), doc);
        }
        Ok(self.docs.get(url))
    }

    /// All versions of a package, as (parsed, spelling in the feed).
    fn versions(&mut self, id: &str) -> Result<Option<&Vec<(Version, String)>>, NugetError> {
        let lower = id.to_ascii_lowercase();
        if !self.versions.contains_key(&lower) {
            let url = format!("{}{lower}/index.json", self.flat);
            let list = self.json(&url)?.map(|doc| {
                let mut out: Vec<(Version, String)> = doc["versions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .filter_map(|s| Version::parse(s).map(|v| (v, s.to_string())))
                    .collect();
                out.sort_by(|a, b| a.0.cmp(&b.0));
                out
            });
            self.versions.insert(lower.clone(), list);
        }
        Ok(self.versions[&lower].as_ref())
    }

    /// The feed's metadata for one version: dependency groups, listing state
    /// and where its catalog record is.
    fn entry(&mut self, id: &str, version: &Version) -> Result<Option<Value>, NugetError> {
        let Some(base) = self.registration.clone() else { return Ok(None) };
        let url = format!("{base}{}/index.json", id.to_ascii_lowercase());
        let Some(index) = self.json(&url)?.cloned() else { return Ok(None) };
        for page in index["items"].as_array().into_iter().flatten() {
            let within = |key: &str, ord: std::cmp::Ordering| page[key].as_str().and_then(Version::parse).map_or(true, |b| version.cmp(&b) != ord);
            if !(within("lower", std::cmp::Ordering::Less) && within("upper", std::cmp::Ordering::Greater)) {
                continue;
            }
            // Large packages keep their pages in separate documents.
            let items = match page["items"].as_array() {
                Some(items) => items.clone(),
                None => {
                    let Some(page_url) = page["@id"].as_str() else { continue };
                    let page_url = page_url.split('#').next().unwrap_or(page_url).to_string();
                    match self.json(&page_url)? {
                        Some(doc) => doc["items"].as_array().cloned().unwrap_or_default(),
                        None => continue,
                    }
                }
            };
            for leaf in items {
                let entry = &leaf["catalogEntry"];
                if entry["version"].as_str().and_then(Version::parse).is_some_and(|v| v == *version) {
                    return Ok(Some(entry.clone()));
                }
            }
        }
        Ok(None)
    }

    /// Pick the version of `id` that `want` stands for.
    fn resolve(&mut self, id: &str, want: &Want, prerelease: bool) -> Result<Result<(Version, String), String>, NugetError> {
        let Some(all) = self.versions(id)?.cloned() else {
            return Ok(Err("does not exist in the feed".into()));
        };
        match want {
            Want::Range(text) => {
                let Some(range) = Range::parse(text) else {
                    return Ok(Err(format!("{text:?} is not a version range")));
                };
                Ok(all.iter().filter(|(v, _)| range.satisfied_by(v)).min_by(|a, b| a.0.cmp(&b.0)).cloned().ok_or_else(|| format!("no version satisfies {text:?}")))
            }
            Want::Latest => {
                // Newest first; skip pre-releases and versions the publisher unlisted.
                for (v, text) in all.iter().rev().take(200) {
                    if v.is_prerelease() && !prerelease {
                        continue;
                    }
                    let listed = self.entry(id, v)?.map_or(true, |e| e["listed"].as_bool() != Some(false));
                    if listed {
                        return Ok(Ok((v.clone(), text.clone())));
                    }
                }
                Ok(Err("has no listed stable version".into()))
            }
        }
    }
}

/// One package version chosen for the mirror.
struct Chosen {
    /// The id as the feed spells it (`Newtonsoft.Json`).
    id: String,
    version: Version,
    catalog: Option<String>,
}

/// Mirror the configured packages and their dependencies into a new snapshot
/// of `repo`.
///
/// What is trusted is the TLS connection to the feed and the SHA-512 its
/// catalog states for each package; every download is checked against that
/// hash when the feed offers one. Package signatures are not verified.
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &NugetConfig) -> Result<NugetReport, NugetError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "nuget" {
        return Err(NugetError::Config(format!("repo {repo} has format {}, not nuget", repo_row.format)));
    }
    if host_of(&cfg.service_index).is_none() {
        return Err(NugetError::Config(format!("{:?} is not an http(s) URL", cfg.service_index)));
    }
    if cfg.packages.is_empty() || cfg.frameworks.is_empty() {
        return Err(NugetError::Config("at least one package and one target framework are needed".into()));
    }
    let mut targets = Vec::new();
    for f in &cfg.frameworks {
        let parsed = Framework::parse(f);
        if matches!(parsed, Framework::Any | Framework::Other) {
            return Err(NugetError::Config(format!("target framework {f:?} is not understood (use net8.0, netstandard2.0, net48, ...)")));
        }
        targets.push((f.clone(), parsed));
    }

    // 1. The service index says where the feed keeps things.
    let index = client.get(&cfg.service_index, &[("Accept", "application/json")])?.bytes(&cfg.service_index, MAX_JSON)?;
    let index: Value = serde_json::from_slice(&index).map_err(|_| feed("service index is not JSON"))?;
    let resource = |prefix: &str| {
        index["resources"].as_array().into_iter().flatten().find_map(|r| {
            let t = r["@type"].as_str()?;
            (t == prefix).then(|| r["@id"].as_str()).flatten().map(|u| if u.ends_with('/') { u.to_string() } else { format!("{u}/") })
        })
    };
    let flat = resource("PackageBaseAddress/3.0.0").ok_or_else(|| feed("the feed has no package download resource (PackageBaseAddress/3.0.0)"))?;
    let registration = ["RegistrationsBaseUrl/3.6.0", "RegistrationsBaseUrl/3.4.0", "RegistrationsBaseUrl/3.0.0-rc", "RegistrationsBaseUrl"].iter().find_map(|t| resource(t));
    let mut f = Feed { client, flat, registration, versions: BTreeMap::new(), docs: BTreeMap::new(), warnings: Vec::new() };
    if f.registration.is_none() {
        f.warnings.push("the feed has no metadata resource; dependencies cannot be followed and downloads cannot be verified".into());
    }

    // 2. Roots, then dependencies. Every distinct (package, range) is resolved
    //    the way a restore would resolve it on its own. A restore finally
    //    picks, per package, one of those versions, so the mirror holds a
    //    superset of what any project built from these roots needs.
    let mut queue: VecDeque<(String, Want, bool)> = VecDeque::new();
    let mut seen: BTreeSet<(String, Want)> = BTreeSet::new();
    let mut selected: BTreeMap<String, BTreeMap<String, Chosen>> = BTreeMap::new();
    for spec in &cfg.packages {
        let (id, want) = split_spec(spec).ok_or_else(|| NugetError::Config(format!("package spec {spec:?} is not understood")))?;
        queue.push_back((id.to_string(), want, true));
    }
    let mut total = 0usize;
    while let Some((id, want, root)) = queue.pop_front() {
        if !seen.insert((id.to_ascii_lowercase(), want.clone())) {
            continue;
        }
        if !valid_id(&id) {
            f.warnings.push(format!("dependency id {id:?} is not acceptable; skipped"));
            continue;
        }
        let (version, _) = match f.resolve(&id, &want, cfg.prerelease)? {
            Ok(found) => found,
            Err(why) => {
                if root {
                    let range = match &want {
                        Want::Latest => "latest".to_string(),
                        Want::Range(r) => r.clone(),
                    };
                    return Err(NugetError::Unsatisfiable { package: id, range });
                }
                f.warnings.push(format!("{id}: {why}; not mirrored"));
                continue;
            }
        };
        let lower = id.to_ascii_lowercase();
        let normalized = version.normalized();
        if selected.get(&lower).is_some_and(|v| v.contains_key(&normalized)) {
            continue;
        }
        total += 1;
        if total > MAX_VERSIONS {
            return Err(feed("dependency tree is implausibly large"));
        }
        let entry = f.entry(&id, &version)?;
        let display = entry.as_ref().and_then(|e| e["id"].as_str()).filter(|d| d.eq_ignore_ascii_case(&id)).unwrap_or(&id).to_string();
        let catalog = entry.as_ref().and_then(|e| e["@id"].as_str()).map(str::to_string);

        // Dependencies of the group each target framework would use.
        if let Some(entry) = &entry {
            let groups: Vec<&Value> = entry["dependencyGroups"].as_array().into_iter().flatten().collect();
            let frameworks: Vec<Framework> = groups.iter().map(|g| Framework::parse(g["targetFramework"].as_str().unwrap_or(""))).collect();
            for (name, target) in &targets {
                match target.nearest(&frameworks) {
                    Some(i) => {
                        for dep in groups[i]["dependencies"].as_array().into_iter().flatten() {
                            let Some(dep_id) = dep["id"].as_str() else { continue };
                            // No range means "any version", and NuGet then takes the lowest.
                            let range = dep["range"].as_str().map(str::trim).filter(|r| !r.is_empty()).unwrap_or("0.0.0");
                            queue.push_back((dep_id.to_string(), Want::Range(range.to_string()), false));
                        }
                    }
                    None if !groups.is_empty() => {
                        f.warnings.push(format!("{display} {normalized} has no dependency group for {name}; it may not be usable from such projects"));
                    }
                    None => {}
                }
            }
        }
        selected.entry(lower).or_default().insert(normalized, Chosen { id: display, version, catalog });
    }

    // 3. Download and lay out a static feed.
    let previous: BTreeMap<String, Digest> = match st.head(repo)? {
        Some(h) => st.meta().read(|t| t.entries_at(repo_row.id, h.seq))?.into_iter().map(|e| (e.path, e.digest)).collect(),
        None => BTreeMap::new(),
    };
    let mut report = NugetReport { packages: selected.len(), ..Default::default() };
    let mut entries: BTreeMap<String, Digest> = BTreeMap::new();
    for (lower, versions) in &selected {
        let mut listed: Vec<&Chosen> = versions.values().collect();
        listed.sort_by(|a, b| a.version.cmp(&b.version));
        for chosen in &listed {
            let ver = chosen.version.normalized();
            let path = format!("flat/{lower}/{ver}/{lower}.{ver}.nupkg");
            // A published package version never changes, so a copy from the
            // previous snapshot is reused without asking the feed again.
            let digest = match previous.get(&path) {
                Some(d) if st.has_blob(d)? => {
                    report.reused += 1;
                    *d
                }
                _ => {
                    let expected = match &chosen.catalog {
                        Some(url) => f.json(url)?.and_then(|leaf| {
                            let algo = leaf["packageHashAlgorithm"].as_str().unwrap_or("");
                            let hash = leaf["packageHash"].as_str()?;
                            algo.eq_ignore_ascii_case("SHA512").then(|| base64::engine::general_purpose::STANDARD.decode(hash).ok()).flatten().filter(|h| h.len() == 64)
                        }),
                        None => None,
                    };
                    let url = format!("{}{lower}/{ver}/{lower}.{ver}.nupkg", f.flat);
                    let body = client.get(&url, &[])?.into_reader().take(MAX_PACKAGE + 1);
                    let mut hashing = Hashing { inner: body, sha512: Sha512::new() };
                    let (digest, size) = st.ingest(&mut hashing, mer_blob::Expect::default())?;
                    match expected {
                        Some(want) if hashing.sha512.finalize().as_slice() != want.as_slice() => {
                            return Err(feed(format!("{} {ver}: the package does not match the hash in the feed catalog", chosen.id)));
                        }
                        Some(_) => {}
                        None => {
                            report.unverified += 1;
                            f.warnings.push(format!("{} {ver}: the feed offers no hash for it; mirrored as downloaded", chosen.id));
                        }
                    }
                    report.downloaded += 1;
                    report.downloaded_bytes += size;
                    digest
                }
            };
            entries.insert(path, digest);
            report.versions += 1;
        }
        let list: Vec<String> = listed.iter().map(|c| c.version.normalized()).collect();
        let (d, _) = st.ingest_bytes(json!({ "versions": list }).to_string().as_bytes())?;
        entries.insert(format!("flat/{lower}/index.json"), d);
    }
    let service_index = json!({
        "version": "3.0.0",
        "resources": [
            { "@id": format!("{BASE_PLACEHOLDER}flat/"), "@type": "PackageBaseAddress/3.0.0", "comment": "Packages mirrored on this vault" },
        ],
    });
    let (d, _) = st.ingest_bytes(service_index.to_string().as_bytes())?;
    entries.insert("v3/index.json".into(), d);

    // 4. One snapshot.
    let mut writer = st.begin(repo)?;
    for (path, digest) in &entries {
        writer.put(path, EntryRole::Artifact, digest)?;
    }
    for gone in previous.keys().filter(|p| !entries.contains_key(*p)) {
        writer.remove(gone)?;
    }
    report.snapshot = writer.seal(None)?.map(|s| s.seq);
    f.warnings.sort();
    f.warnings.dedup();
    report.warnings = f.warnings;
    Ok(report)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn ids_and_specs() {
        for ok in ["Newtonsoft.Json", "Serilog.Sinks.Console", "a", "System.Text.Json", "My_Pkg-2"] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in ["", ".x", "x.", "a..b", "a/b", "../x", "a b", "a@b", &"x".repeat(101)] {
            assert!(!valid_id(bad), "{bad:?}");
        }
        assert_eq!(split_spec("Serilog"), Some(("Serilog", Want::Latest)));
        assert_eq!(split_spec("Newtonsoft.Json@13.0.3"), Some(("Newtonsoft.Json", Want::Range("13.0.3".into()))));
        assert_eq!(split_spec("Foo@[1.0, 2.0)"), Some(("Foo", Want::Range("[1.0, 2.0)".into()))));
        assert_eq!(split_spec("../evil@1.0"), None);
    }
}
