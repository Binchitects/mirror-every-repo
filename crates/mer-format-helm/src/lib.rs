//! Helm chart repositories (the classic `index.yaml` kind).
//!
//! Charts kept in OCI registries need nothing from this crate: they are
//! mirrored with the container image format and pulled with `helm pull oci://`.
//!
//! A packaged chart already contains the charts it depends on, so there is no
//! dependency walk here: the collector picks the wanted versions, downloads
//! each archive against the digest in the upstream index, and writes an
//! `index.yaml` that lists only what was mirrored.
#[cfg(test)]
mod tests;

use mer_blob::Expect;
use mer_core::{Digest, EntryRole};
use mer_fetch::{host_of, FetchError, HttpClient};
use mer_format_npm::{Range, Version};
use mer_storage::{Storage, StorageError};
use serde_yaml::{Mapping, Value};
use std::collections::BTreeMap;
use std::io::Read;

const MAX_INDEX: u64 = 512 << 20;
const MAX_CHART: u64 = 1 << 30;

#[derive(Debug, thiserror::Error)]
pub enum HelmError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("upstream chart repository: {0}")]
    Repo(String),
    #[error("{0}")]
    Config(String),
    #[error("no version of chart {chart} satisfies {range:?}")]
    Unsatisfiable { chart: String, range: String },
}

impl From<mer_meta::MetaError> for HelmError {
    fn from(e: mer_meta::MetaError) -> Self {
        HelmError::Storage(e.into())
    }
}

#[derive(Clone, Debug)]
pub struct HelmConfig {
    /// Base URL of the chart repository (where `index.yaml` lives).
    pub repo_url: String,
    /// Charts to mirror: `ingress-nginx` (newest stable), `ingress-nginx@4.15.1`,
    /// `ingress-nginx@^4.14`. The same chart may be named more than once.
    pub charts: Vec<String>,
    /// Let a chart named without a version resolve to a pre-release.
    pub prerelease: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HelmReport {
    /// (chart, version) mirrored.
    pub charts: Vec<(String, String)>,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub reused: usize,
    pub warnings: Vec<String>,
    pub snapshot: Option<u64>,
}

fn repo_err(msg: impl Into<String>) -> HelmError {
    HelmError::Repo(msg.into())
}

/// Chart names become file names and URL parts.
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_' | b'.'))
        && b[0].is_ascii_alphanumeric()
        && !name.contains("..")
}

/// Resolve a chart URL from the index against the repository base.
fn resolve(base: &str, url: &str) -> Option<String> {
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(url.to_string());
    }
    if url.contains("://") || url.split('/').any(|s| s == "..") {
        return None;
    }
    if let Some(abs) = url.strip_prefix('/') {
        let scheme_end = base.find("://")? + 3;
        let host_end = base[scheme_end..].find('/').map_or(base.len(), |i| scheme_end + i);
        return Some(format!("{}/{abs}", &base[..host_end]));
    }
    Some(format!("{}/{url}", base.trim_end_matches('/')))
}

/// Mirror the configured charts into a new snapshot of `repo`.
///
/// What is trusted is the TLS connection to the repository and the SHA-256
/// its index states for each chart. Provenance files (`.prov`) are not used.
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &HelmConfig) -> Result<HelmReport, HelmError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "helm" {
        return Err(HelmError::Config(format!("repo {repo} has format {}, not helm", repo_row.format)));
    }
    if host_of(&cfg.repo_url).is_none() {
        return Err(HelmError::Config(format!("{:?} is not an http(s) URL", cfg.repo_url)));
    }
    if cfg.charts.is_empty() {
        return Err(HelmError::Config("at least one chart is needed".into()));
    }
    let base = cfg.repo_url.trim_end_matches('/').to_string();
    let index_url = format!("{base}/index.yaml");
    let body = client.get(&index_url, &[])?.bytes(&index_url, MAX_INDEX)?;
    let index: Value = serde_yaml::from_slice(&body).map_err(|e| repo_err(format!("index.yaml does not parse: {e}")))?;
    let entries = index.get("entries").and_then(Value::as_mapping).ok_or_else(|| repo_err("index.yaml has no entries"))?;

    let mut report = HelmReport::default();
    // chart -> version text -> index entry
    let mut chosen: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    for spec in &cfg.charts {
        let (name, want) = match spec.trim().split_once('@') {
            Some((n, r)) => (n, Some(r.trim())),
            None => (spec.trim(), None),
        };
        if !valid_name(name) {
            return Err(HelmError::Config(format!("chart name {name:?} is not acceptable")));
        }
        let unsatisfiable = || HelmError::Unsatisfiable { chart: name.to_string(), range: want.unwrap_or("latest").to_string() };
        let versions: Vec<(Version, &Value)> = entries
            .get(name)
            .and_then(Value::as_sequence)
            .ok_or_else(unsatisfiable)?
            .iter()
            .filter_map(|e| e.get("version").and_then(Value::as_str).and_then(Version::parse).map(|v| (v, e)))
            .collect();
        let pick = match want {
            // Like `helm install` without --version: the newest stable release.
            None => versions.iter().filter(|(v, _)| cfg.prerelease || v.pre.is_empty()).max_by(|a, b| a.0.cmp(&b.0)),
            Some(text) => {
                // Helm joins constraints with a comma (">=1.2, <2"); npm with a space.
                let range = Range::parse(&text.replace(',', " ")).ok_or_else(|| HelmError::Config(format!("{text:?} is not a version or range")))?;
                versions.iter().filter(|(v, _)| range.satisfied_by(v)).max_by(|a, b| a.0.cmp(&b.0))
            }
        };
        let (version, entry) = pick.ok_or_else(unsatisfiable)?;
        chosen.entry(name.to_string()).or_default().insert(version.to_string(), (*entry).clone());
    }

    let mut out_entries = Mapping::new();
    let mut entries_out: BTreeMap<String, Digest> = BTreeMap::new();
    for (name, versions) in &chosen {
        let mut listed: Vec<(Version, Value)> = Vec::new();
        for (version, entry) in versions {
            let file = format!("{name}-{version}.tgz");
            let path = format!("charts/{file}");
            let url = entry
                .get("urls")
                .and_then(Value::as_sequence)
                .and_then(|u| u.first())
                .and_then(Value::as_str)
                .and_then(|u| resolve(&base, u))
                .ok_or_else(|| repo_err(format!("{name} {version}: the index gives no usable download URL")))?;
            let expected = entry.get("digest").and_then(Value::as_str).map(|d| d.strip_prefix("sha256:").unwrap_or(d)).and_then(|d| Digest::from_hex(d).ok());
            let digest = match expected {
                Some(d) if st.has_blob(&d)? => {
                    report.reused += 1;
                    d
                }
                _ => {
                    let mut body = client.get(&url, &[])?.into_reader().take(MAX_CHART + 1);
                    let (d, size) = st.ingest(&mut body, Expect { digest: expected, size: None })?;
                    if expected.is_none() {
                        report.warnings.push(format!("{name} {version}: the index states no digest; mirrored as downloaded"));
                    }
                    report.downloaded += 1;
                    report.downloaded_bytes += size;
                    d
                }
            };
            entries_out.insert(path.clone(), digest);
            // The index entry as upstream wrote it, pointing at our copy.
            let mut e = entry.as_mapping().cloned().unwrap_or_default();
            e.insert("urls".into(), Value::Sequence(vec![Value::String(path)]));
            e.insert("digest".into(), Value::String(digest.to_hex()));
            listed.push((Version::parse(version).expect("parsed above"), Value::Mapping(e)));
            report.charts.push((name.clone(), version.clone()));
        }
        listed.sort_by(|a, b| b.0.cmp(&a.0)); // newest first, as helm expects
        out_entries.insert(Value::String(name.clone()), Value::Sequence(listed.into_iter().map(|(_, e)| e).collect()));
    }
    let mut doc = Mapping::new();
    doc.insert("apiVersion".into(), "v1".into());
    doc.insert("entries".into(), Value::Mapping(out_entries));
    let text = serde_yaml::to_string(&Value::Mapping(doc)).map_err(|e| repo_err(e.to_string()))?;
    let (d, _) = st.ingest_bytes(text.as_bytes())?;
    entries_out.insert("index.yaml".into(), d);

    let previous: Vec<String> = match st.head(repo)? {
        Some(h) => st.meta().read(|t| t.entries_at(repo_row.id, h.seq))?.into_iter().map(|e| e.path).collect(),
        None => Vec::new(),
    };
    let mut writer = st.begin(repo)?;
    for (path, digest) in &entries_out {
        writer.put(path, EntryRole::Artifact, digest)?;
    }
    for gone in previous.iter().filter(|p| !entries_out.contains_key(*p)) {
        writer.remove(gone)?;
    }
    report.snapshot = writer.seal(None)?.map(|s| s.seq);
    Ok(report)
}
