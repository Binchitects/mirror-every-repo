//! Collector-side sync of a Helm chart repository into one repo snapshot.
use mer_core::{Digest, EntryRole};
use mer_fetch::{host_of, FetchError, HttpClient};
use mer_storage::{Storage, StorageError};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::io::Read;
use yaml_rust2::{Yaml, YamlLoader};

const MAX_INDEX: u64 = 512 << 20;
const MAX_CHART: u64 = 1 << 30;

#[derive(Debug, thiserror::Error)]
pub enum HelmError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("upstream repository: {0}")]
    Upstream(String),
    #[error("{0}")]
    Config(String),
}

impl From<mer_meta::MetaError> for HelmError {
    fn from(e: mer_meta::MetaError) -> Self {
        HelmError::Storage(e.into())
    }
}

#[derive(Clone, Debug)]
pub struct HelmConfig {
    /// Repository URL, the one `helm repo add` takes: `https://charts.jetstack.io`.
    pub repo: String,
    /// What to mirror: `cert-manager` (newest stable), `cert-manager@v1.16.1` (exact).
    pub charts: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HelmReport {
    pub charts: usize,
    pub versions: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub reused: usize,
    /// Charts the index states no digest for.
    pub unverified: usize,
    pub warnings: Vec<String>,
    pub snapshot: Option<u64>,
}

fn up(msg: impl Into<String>) -> HelmError {
    HelmError::Upstream(msg.into())
}

/// Chart names: letters, digits, `-`, `_` and `.`, starting with a letter or digit.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.len() <= 128 && b[0].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

fn valid_version(v: &str) -> bool {
    !v.is_empty() && v.len() <= 128 && v.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'+')) && !v.starts_with('.')
}

/// Helm versions are semver, sometimes written with a leading `v`.
fn semver_of(v: &str) -> Option<semver::Version> {
    semver::Version::parse(v.strip_prefix('v').unwrap_or(v)).ok()
}

/// YAML to JSON as Helm reads it. Numbers that are not booleans are kept as
/// their text: `version: 1.0` is the string "1.0" to Helm, not a float.
fn to_json(y: &Yaml) -> Value {
    match y {
        Yaml::String(s) | Yaml::Real(s) => Value::String(s.clone()),
        Yaml::Integer(i) => Value::String(i.to_string()),
        Yaml::Boolean(b) => Value::Bool(*b),
        Yaml::Array(a) => Value::Array(a.iter().map(to_json).collect()),
        Yaml::Hash(h) => Value::Object(
            h.iter()
                .filter_map(|(k, v)| {
                    let key = match k {
                        Yaml::String(s) | Yaml::Real(s) => s.clone(),
                        Yaml::Integer(i) => i.to_string(),
                        Yaml::Boolean(b) => b.to_string(),
                        _ => return None,
                    };
                    Some((key, to_json(v)))
                })
                .collect::<Map<String, Value>>(),
        ),
        _ => Value::Null,
    }
}

fn split_spec(spec: &str) -> Option<(&str, Option<&str>)> {
    let spec = spec.trim();
    let (name, version) = match spec.split_once('@') {
        Some((n, "latest")) => (n, None),
        Some((n, v)) => (n, Some(v)),
        None => (spec, None),
    };
    (valid_name(name) && version.is_none_or(valid_version)).then_some((name, version))
}

/// Mirror the configured charts into a new snapshot of `repo`.
///
/// What is trusted is the TLS connection to the repository and the SHA-256
/// its index states for each chart; every download is checked against it.
/// Provenance files (`.prov`) are not mirrored or verified.
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &HelmConfig) -> Result<HelmReport, HelmError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "helm" {
        return Err(HelmError::Config(format!("repo {repo} has format {}, not helm", repo_row.format)));
    }
    let base = cfg.repo.trim_end_matches('/').to_string();
    if host_of(&base).is_none() {
        return Err(HelmError::Config(format!("{:?} is not an http(s) URL", cfg.repo)));
    }
    if cfg.charts.is_empty() {
        return Err(HelmError::Config("name at least one chart".into()));
    }
    let mut specs = Vec::new();
    for spec in &cfg.charts {
        specs.push(split_spec(spec).ok_or_else(|| HelmError::Config(format!("chart spec {spec:?} is not understood (name or name@version)")))?);
    }

    // 1. The upstream index.
    let url = format!("{base}/index.yaml");
    let body = client.get(&url, &[])?.bytes(&url, MAX_INDEX)?;
    let text = String::from_utf8(body).map_err(|_| up("index.yaml is not UTF-8"))?;
    let docs = YamlLoader::load_from_str(&text).map_err(|e| up(format!("index.yaml: {e}")))?;
    let index = docs.first().map(to_json).unwrap_or(Value::Null);
    let entries = index["entries"].as_object().ok_or_else(|| up("index.yaml has no entries"))?;

    // 2. Pick versions.
    let mut report = HelmReport::default();
    let mut chosen: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    for (name, version) in specs {
        let list = entries.get(name).and_then(Value::as_array).ok_or_else(|| up(format!("chart {name} is not in the repository")))?;
        let entry = match version {
            Some(want) => list.iter().find(|e| {
                let have = e["version"].as_str().unwrap_or("");
                have == want || have.strip_prefix('v') == Some(want) || want.strip_prefix('v') == Some(have)
            }),
            // Newest stable, as `helm install` without --version picks it.
            None => list
                .iter()
                .filter(|e| e["deprecated"] != json!(true) || list.len() == 1)
                .filter_map(|e| Some((semver_of(e["version"].as_str()?)?, e)))
                .filter(|(v, _)| v.pre.is_empty())
                .max_by(|a, b| a.0.cmp(&b.0))
                .map(|(_, e)| e),
        };
        let entry = entry.ok_or_else(|| up(format!("chart {name} has no version {}", version.unwrap_or("that is stable"))))?;
        let v = entry["version"].as_str().filter(|v| valid_version(v)).ok_or_else(|| up(format!("chart {name}: version is not acceptable")))?;
        if entry["name"].as_str().is_some_and(|n| n != name) {
            return Err(up(format!("chart {name}: the index entry names another chart")));
        }
        chosen.entry(name.to_string()).or_default().insert(v.to_string(), entry.clone());
    }

    // 3. Download and write an index of the subset.
    let previous: BTreeMap<String, Digest> = match st.head(repo)? {
        Some(h) => st.meta().read(|t| t.entries_at(repo_row.id, h.seq))?.into_iter().map(|e| (e.path, e.digest)).collect(),
        None => BTreeMap::new(),
    };
    let mut files: BTreeMap<String, Digest> = BTreeMap::new();
    let mut out_entries = Map::new();
    for (name, versions) in &chosen {
        let mut list = Vec::new();
        for (v, entry) in versions.iter().rev() {
            let path = format!("charts/{name}-{v}.tgz");
            let expected = entry["digest"].as_str().and_then(|d| Digest::from_hex(d.strip_prefix("sha256:").unwrap_or(d)).ok());
            let reusable = match expected {
                Some(d) => st.has_blob(&d)?.then_some(d),
                None => match previous.get(&path) {
                    Some(d) if st.has_blob(d)? => Some(*d),
                    _ => None,
                },
            };
            let digest = match reusable {
                Some(d) => {
                    report.reused += 1;
                    d
                }
                None => {
                    let src = entry["urls"].as_array().and_then(|u| u.first()).and_then(Value::as_str).ok_or_else(|| up(format!("{name} {v}: no download URL")))?;
                    let src = if host_of(src).is_some() { src.to_string() } else { format!("{base}/{}", src.trim_start_matches('/')) };
                    let mut body = client.get(&src, &[])?.into_reader().take(MAX_CHART + 1);
                    let (d, size) = st.ingest(&mut body, mer_blob::Expect { digest: expected, size: None }).map_err(|e| match e {
                        StorageError::Store(mer_blob::StoreError::DigestMismatch { .. }) => up(format!("{name} {v}: the download does not match the digest in the index")),
                        other => other.into(),
                    })?;
                    if expected.is_none() {
                        report.unverified += 1;
                        report.warnings.push(format!("{name} {v}: the index states no digest; mirrored as downloaded"));
                    }
                    report.downloaded += 1;
                    report.downloaded_bytes += size;
                    d
                }
            };
            files.insert(path.clone(), digest);
            let mut entry = entry.clone();
            entry["urls"] = json!([path]);
            entry["digest"] = json!(digest.to_string());
            list.push(entry);
            report.versions += 1;
        }
        out_entries.insert(name.clone(), Value::Array(list));
    }
    report.charts = chosen.len();
    // JSON is YAML, and Helm reads this index as YAML.
    let index = json!({ "apiVersion": "v1", "entries": out_entries });
    let (d, _) = st.ingest_bytes(serde_json::to_string_pretty(&index).expect("JSON").as_bytes())?;
    files.insert("index.yaml".into(), d);

    // 4. One snapshot.
    let mut writer = st.begin(repo)?;
    for (path, digest) in &files {
        writer.put(path, EntryRole::Artifact, digest)?;
    }
    for gone in previous.keys().filter(|p| !files.contains_key(*p)) {
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
    fn names_specs_and_yaml() {
        assert!(valid_name("cert-manager") && valid_name("kube-prometheus-stack") && valid_name("a.b_c"));
        assert!(!valid_name("") && !valid_name("../x") && !valid_name("a/b") && !valid_name("-x"));
        assert_eq!(split_spec("cert-manager@v1.16.1"), Some(("cert-manager", Some("v1.16.1"))));
        assert_eq!(split_spec("nginx"), Some(("nginx", None)));
        assert_eq!(split_spec("nginx@latest"), Some(("nginx", None)));
        assert_eq!(split_spec("nginx@../1"), None);
        let y = YamlLoader::load_from_str("a: 1.0\nb: 3\nc: true\nd: [x, 2]\n").unwrap();
        assert_eq!(to_json(&y[0]), json!({ "a": "1.0", "b": "3", "c": true, "d": ["x", "2"] }));
    }
}
