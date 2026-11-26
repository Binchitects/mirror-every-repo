//! Collector-side sync of a Composer repository (packagist.org) into one snapshot.
use crate::version::{normalize, php_version_compare, stability, Constraint};
use mer_core::{Digest, EntryRole};
use mer_fetch::{FetchError, HttpClient};
use mer_storage::{Storage, StorageError};
use serde_json::{json, Map, Value};
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;

const MAX_METADATA: u64 = 128 << 20;
const MAX_DIST: u64 = 2 << 30;

/// Stands for the vault's own address in the metadata it serves; replaced
/// when served, with the address the client used.
pub const BASE_PLACEHOLDER: &str = "mer://base/";

#[derive(Debug, thiserror::Error)]
pub enum ComposerError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("upstream: {0}")]
    Upstream(String),
    #[error("{0}")]
    Config(String),
}

impl From<mer_meta::MetaError> for ComposerError {
    fn from(e: mer_meta::MetaError) -> Self {
        ComposerError::Storage(e.into())
    }
}

#[derive(Clone, Debug)]
pub struct ComposerConfig {
    /// The repository, for example `https://repo.packagist.org`.
    pub upstream: String,
    /// What to mirror: `monolog/monolog` (newest stable),
    /// `symfony/console:^7.1`, `psr/log@3.0.2` (exactly).
    pub packages: Vec<String>,
    /// `composer.lock` files whose exact packages (dev ones included) are mirrored.
    pub lockfiles: Vec<PathBuf>,
    /// The PHP inside the air gap, for example `8.3.14`: versions that need
    /// another PHP are left out. `None`: every PHP.
    pub php: Option<String>,
    /// Let packages named without a constraint resolve to an unstable version.
    pub pre: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ComposerReport {
    pub packages: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub reused: usize,
    pub warnings: Vec<String>,
    pub snapshot: Option<u64>,
}

fn up(msg: impl Into<String>) -> ComposerError {
    ComposerError::Upstream(msg.into())
}

/// `vendor/name`, lower case, as packagist names packages.
pub fn valid_name(n: &str) -> bool {
    let Some((v, p)) = n.split_once('/') else { return false };
    let part = |s: &str| !s.is_empty() && s.len() <= 100 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')) && !s.starts_with(['.', '-', '_']);
    part(v) && part(p)
}

/// Requirements on the environment rather than on packages.
fn is_platform(name: &str) -> bool {
    !name.contains('/') || name.starts_with("ext-") || name.starts_with("lib-") || matches!(name, "php" | "php-64bit" | "php-ipv6" | "php-zts" | "php-debug" | "hhvm" | "composer" | "composer-plugin-api" | "composer-runtime-api")
}

/// Expand the `composer/2.0` minified form: each entry changes the one
/// before it, and the string `__unset` removes a key.
pub fn expand(doc: &Value, name: &str) -> Vec<Map<String, Value>> {
    let Some(list) = doc["packages"][name].as_array() else { return Vec::new() };
    let minified = doc["minified"].as_str() == Some("composer/2.0");
    let mut out = Vec::new();
    let mut prev: Map<String, Value> = Map::new();
    for e in list {
        let Some(obj) = e.as_object() else { continue };
        let mut cur = if minified { prev.clone() } else { Map::new() };
        for (k, v) in obj {
            if v.as_str() == Some("__unset") {
                cur.remove(k);
            } else {
                cur.insert(k.clone(), v.clone());
            }
        }
        prev = cur.clone();
        out.push(cur);
    }
    out
}

/// `vendor/name`, `vendor/name:^2.0`, `vendor/name@1.2.3`.
fn parse_spec(s: &str) -> Result<(String, Constraint, bool), String> {
    let (name, c) = match (s.split_once(':'), s.split_once('@')) {
        (Some((n, c)), _) => (n.trim(), c.trim().to_string()),
        (None, Some((n, v))) => (n.trim(), format!("=={}", v.trim())),
        (None, None) => (s.trim(), "*".to_string()),
    };
    let name = name.to_ascii_lowercase();
    if !valid_name(&name) {
        return Err(format!("{s:?} is not a package name (vendor/name)"));
    }
    let explicit = c != "*";
    let parsed = Constraint::parse(&c).ok_or_else(|| format!("{s:?}: {c:?} is not a constraint"))?;
    Ok((name, parsed, explicit))
}

/// Whether a constraint asks for unstable versions itself (`@beta`, `^2.0-RC1`).
fn asks_unstable(c: &str) -> bool {
    let l = c.to_ascii_lowercase();
    l.contains('@') || ["alpha", "beta", "rc", "-dev", "dev-"].iter().any(|w| l.contains(w))
}

struct Fetcher<'a> {
    client: &'a HttpClient,
    base: String,
    cache: HashMap<String, Option<Vec<Map<String, Value>>>>,
}

impl Fetcher<'_> {
    /// The versions of a package (`~dev`: its branches); `None` if the
    /// repository does not have it.
    fn versions(&mut self, file: &str) -> Result<Option<&Vec<Map<String, Value>>>, ComposerError> {
        if !self.cache.contains_key(file) {
            let url = format!("{}/p2/{file}.json", self.base);
            let entries = match self.client.get_bytes(&url, &[], MAX_METADATA) {
                Ok(body) => {
                    let doc: Value = serde_json::from_slice(&body).map_err(|e| up(format!("{url}: {e}")))?;
                    let name = file.trim_end_matches("~dev");
                    Some(expand(&doc, name))
                }
                Err(FetchError::NotFound(_) | FetchError::Http { status: 404 | 410, .. }) => None,
                Err(e) => return Err(e.into()),
            };
            self.cache.insert(file.to_string(), entries);
        }
        Ok(self.cache[file].as_ref())
    }
}

fn text<'a>(e: &'a Map<String, Value>, k: &str) -> &'a str {
    e.get(k).and_then(Value::as_str).unwrap_or("")
}

/// Whether an entry's `require.php` admits the PHP inside the air gap.
fn suits_php(e: &Map<String, Value>, php: Option<&str>) -> bool {
    let (Some(php), Some(req)) = (php, e.get("require").and_then(|r| r.get("php")).and_then(Value::as_str)) else { return true };
    Constraint::parse(req).is_none_or(|c| c.matches(php))
}

/// GitHub's API zipball address, as packagist states it, becomes the
/// address it redirects to, which has no API rate limit.
fn download_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("https://api.github.com/repos/") {
        let parts: Vec<&str> = rest.split('/').collect();
        if let [owner, repo, "zipball", reference] = parts.as_slice() {
            return format!("https://codeload.github.com/{owner}/{repo}/legacy.zip/{reference}");
        }
    }
    url.to_string()
}

/// Mirror the configured packages, with what they require, into a new snapshot of `repo`.
///
/// Each constraint resolves to its newest matching version on its own (not
/// a solver), stable unless the constraint asks otherwise, among versions
/// for the configured PHP. Packagist states no checksum for most
/// downloads, so archives are trusted on TLS; the vault's metadata states
/// the SHA-1 of what it holds, which Composer checks.
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &ComposerConfig) -> Result<ComposerReport, ComposerError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "composer" {
        return Err(ComposerError::Config(format!("repo {repo} has format {}, not composer", repo_row.format)));
    }
    if cfg.packages.is_empty() && cfg.lockfiles.is_empty() {
        return Err(ComposerError::Config("name a package or a composer.lock".into()));
    }
    let php = cfg.php.as_deref().map(|p| normalize(p).map(|_| p).ok_or_else(|| ComposerError::Config(format!("{p:?} is not a PHP version")))).transpose()?;
    let mut f = Fetcher { client, base: cfg.upstream.trim_end_matches('/').to_string(), cache: HashMap::new() };
    let mut warnings = Vec::new();
    // name -> (metadata file it came from, entries chosen by version)
    let mut chosen: BTreeMap<String, BTreeMap<String, Map<String, Value>>> = BTreeMap::new();

    for path in &cfg.lockfiles {
        let lock: Value = serde_json::from_slice(&std::fs::read(path).map_err(|e| ComposerError::Config(format!("{}: {e}", path.display())))?)
            .map_err(|e| ComposerError::Config(format!("{}: {e}", path.display())))?;
        let mut n = 0;
        for section in ["packages", "packages-dev"] {
            for p in lock[section].as_array().into_iter().flatten() {
                let (name, version) = (p["name"].as_str().unwrap_or("").to_ascii_lowercase(), p["version"].as_str().unwrap_or(""));
                if !valid_name(&name) {
                    continue;
                }
                let file = if version.starts_with("dev-") { format!("{name}~dev") } else { name.clone() };
                let entry = f.versions(&file)?.and_then(|es| es.iter().find(|e| text(e, "version") == version).cloned());
                let entry = entry.ok_or_else(|| up(format!("{}: {name} {version} is not in the repository", path.display())))?;
                chosen.entry(name).or_default().insert(version.to_string(), entry);
                n += 1;
            }
        }
        if n == 0 {
            return Err(ComposerError::Config(format!("{} names no packages", path.display())));
        }
    }

    // (package, constraint, the constraint as written, who needs it, unstable allowed)
    let mut queue: VecDeque<(String, Constraint, String, String, bool)> = VecDeque::new();
    for s in &cfg.packages {
        let (name, c, explicit) = parse_spec(s).map_err(ComposerError::Config)?;
        let unstable = cfg.pre || (explicit && asks_unstable(s));
        queue.push_back((name, c, s.clone(), "asked for".into(), unstable));
    }
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    while let Some((name, c, written, who, unstable)) = queue.pop_front() {
        if !seen.insert((name.clone(), written.clone())) {
            continue;
        }
        let asked = who == "asked for";
        let Some(entries) = f.versions(&name)?.cloned() else {
            if asked {
                return Err(up(format!("{name} is not in the repository")));
            }
            warnings.push(format!("{who} requires {name}, which the repository does not have (a virtual package? name a package that provides it)"));
            continue;
        };
        let best = entries
            .iter()
            .filter(|e| {
                let v = text(e, "version_normalized");
                !v.is_empty() && !v.starts_with("dev-") && (unstable || stability(v) == "stable") && suits_php(e, php) && c.matches_normalized(v)
            })
            .max_by(|a, b| php_version_compare(text(a, "version_normalized"), text(b, "version_normalized")));
        let Some(best) = best else {
            let msg = if asked {
                format!("no version of {name} matches what was asked, for this PHP")
            } else {
                format!("{who} requires {name} {written}, but no version matches, for this PHP")
            };
            if asked {
                return Err(up(msg));
            }
            warnings.push(msg);
            continue;
        };
        let version = text(best, "version").to_string();
        let label = format!("{name} {version}");
        if let Some(req) = best.get("require").and_then(Value::as_object) {
            for (dep, dc) in req {
                let dep = dep.to_ascii_lowercase();
                if is_platform(&dep) {
                    continue;
                }
                let dc = dc.as_str().unwrap_or("*");
                match Constraint::parse(dc) {
                    Some(parsed) => queue.push_back((dep, parsed, dc.to_string(), label.clone(), asks_unstable(dc))),
                    None => warnings.push(format!("{label}: constraint {dc:?} on {dep} is not understood; ignored")),
                }
            }
        }
        chosen.entry(name).or_default().insert(version, best.clone());
    }

    let previous: BTreeMap<String, Digest> = match st.head(repo)? {
        Some(h) => st.meta().read(|t| t.entries_at(repo_row.id, h.seq))?.into_iter().map(|e| (e.path, e.digest)).collect(),
        None => BTreeMap::new(),
    };
    let mut report = ComposerReport::default();
    let mut files: BTreeMap<String, Digest> = BTreeMap::new();
    for (name, versions) in &chosen {
        let mut served = Vec::new();
        for (version, entry) in versions {
            let dist = entry.get("dist").cloned().unwrap_or(Value::Null);
            let (url, kind) = (dist["url"].as_str().unwrap_or(""), dist["type"].as_str().unwrap_or(""));
            if kind != "zip" || url.is_empty() {
                warnings.push(format!("{name} {version}: no zip archive to download ({kind:?}); left out"));
                continue;
            }
            let normalized = text(entry, "version_normalized");
            let reference: String = dist["reference"].as_str().unwrap_or("").chars().filter(char::is_ascii_alphanumeric).take(12).collect();
            let safe = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
            let file_version = if safe(normalized) { normalized.to_string() } else { version.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '_' }).collect() };
            let path = format!("dists/{name}/{file_version}{}{reference}.zip", if reference.is_empty() { "" } else { "-" });
            let (digest, sha1) = match previous.get(&path) {
                // The same archive as last time: the path names its commit.
                Some(d) => {
                    let body = st.read_blob(d)?;
                    report.reused += 1;
                    (*d, hex::encode(Sha1::digest(&body)))
                }
                None => {
                    let fetch = download_url(url);
                    let body = client.get_bytes(&fetch, &[], MAX_DIST)?;
                    if body.len() < 4 || &body[..2] != b"PK" {
                        return Err(up(format!("{fetch}: not a zip archive")));
                    }
                    let sha1 = hex::encode(Sha1::digest(&body));
                    if let Some(want) = dist["shasum"].as_str().filter(|s| !s.is_empty()) {
                        if !want.eq_ignore_ascii_case(&sha1) {
                            return Err(up(format!("{fetch} does not match the shasum the repository states")));
                        }
                    }
                    let (d, size) = st.ingest_bytes(&body)?;
                    report.downloaded += 1;
                    report.downloaded_bytes += size;
                    (d, sha1)
                }
            };
            files.insert(path.clone(), digest);
            let mut e = entry.clone();
            // Inside the air gap there is nothing to clone from; the archive is all there is.
            e.remove("source");
            e.remove("notification-url");
            e.insert("dist".into(), json!({ "type": "zip", "url": format!("{BASE_PLACEHOLDER}{path}"), "reference": dist["reference"], "shasum": sha1 }));
            served.push(Value::Object(e));
            report.packages += 1;
        }
        if served.is_empty() {
            continue;
        }
        let doc = json!({ "packages": { name: served } });
        let (d, _) = st.ingest_bytes(serde_json::to_string(&doc).expect("JSON").as_bytes())?;
        files.insert(format!("p2/{name}.json"), d);
    }
    let index = json!({ "packages": [], "metadata-url": format!("{BASE_PLACEHOLDER}p2/%package%.json") });
    let (d, _) = st.ingest_bytes(serde_json::to_string(&index).expect("JSON").as_bytes())?;
    files.insert("packages.json".into(), d);

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
