//! Container images (OCI / Docker registries), pull side.
//!
//! An image is a tree of content-addressed objects: a tag names a manifest
//! (or an index of per-platform manifests), which names a config and layers,
//! all by SHA-256. The collector resolves each wanted tag once, then fetches
//! everything below it by digest and checks each object against its digest.
//! Layers map directly onto the shared blob store, so a base layer used by
//! ten images is stored and shipped once.
#[cfg(test)]
mod tests;

use mer_blob::Expect;
use mer_core::{Digest, EntryRole};
use mer_fetch::{host_of, FetchError, HttpClient, HttpResponse};
use mer_storage::{Storage, StorageError};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;

const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";
const MAX_MANIFEST: u64 = 4 << 20;
const MAX_TOKEN: u64 = 64 << 10;
const MAX_LAYER: u64 = 64 << 30;

#[derive(Debug, thiserror::Error)]
pub enum OciError {
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
}

impl From<mer_meta::MetaError> for OciError {
    fn from(e: mer_meta::MetaError) -> Self {
        OciError::Storage(e.into())
    }
}

#[derive(Clone, Debug)]
pub struct OciConfig {
    /// For example `https://registry-1.docker.io`.
    pub registry: String,
    /// Images to mirror: `library/alpine:3.20`, `library/nginx@sha256:...`.
    pub images: Vec<String>,
    /// Platforms to keep from multi-platform images: `linux/amd64`, `linux/arm64/v8`.
    pub platforms: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OciReport {
    /// (image reference, digest it resolved to)
    pub images: Vec<(String, String)>,
    pub manifests: usize,
    pub blobs: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub reused: usize,
    pub warnings: Vec<String>,
    pub snapshot: Option<u64>,
}

fn reg(msg: impl Into<String>) -> OciError {
    OciError::Registry(msg.into())
}

/// Repository names: lower-case path components separated by `/`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name.split('/').all(|part| {
            let b = part.as_bytes();
            !b.is_empty()
                && b[0].is_ascii_alphanumeric()
                && b[b.len() - 1].is_ascii_alphanumeric()
                && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-'))
        })
}

pub fn valid_tag(tag: &str) -> bool {
    let b = tag.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// `sha256:<64 hex>` -> digest. Other algorithms are not accepted.
pub fn parse_digest(text: &str) -> Option<Digest> {
    let hex = text.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then(|| Digest::from_hex(hex).ok()).flatten()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Reference {
    Tag(String),
    Digest(Digest),
}

/// `name:tag`, `name@sha256:...`, or `name` (tag `latest`).
fn parse_image(spec: &str) -> Option<(String, Reference)> {
    let spec = spec.trim();
    if let Some((name, digest)) = spec.split_once('@') {
        return (valid_name(name)).then(|| parse_digest(digest).map(|d| (name.to_string(), Reference::Digest(d)))).flatten();
    }
    // A colon after the last slash separates the tag.
    let (name, tag) = match spec.rsplit_once(':') {
        Some((n, t)) if !t.contains('/') => (n, t),
        _ => (spec, "latest"),
    };
    (valid_name(name) && valid_tag(tag)).then(|| (name.to_string(), Reference::Tag(tag.to_string())))
}

/// `os/arch[/variant]` of an index entry.
fn platform_of(entry: &Value) -> String {
    let p = &entry["platform"];
    let mut out = format!("{}/{}", p["os"].as_str().unwrap_or("unknown"), p["architecture"].as_str().unwrap_or("unknown"));
    if let Some(v) = p["variant"].as_str() {
        out.push('/');
        out.push_str(v);
    }
    out
}

fn platform_wanted(wanted: &[String], have: &str) -> bool {
    // "linux/arm64" also accepts "linux/arm64/v8".
    wanted.iter().any(|w| w == have || have.strip_prefix(w.as_str()).is_some_and(|rest| rest.starts_with('/')))
}

struct Puller<'a> {
    client: &'a HttpClient,
    registry: String,
    /// Bearer tokens per repository name.
    tokens: BTreeMap<String, String>,
}

impl Puller<'_> {
    /// Parse `Bearer realm="...",service="...",scope="..."`.
    fn challenge(header: &str) -> Option<BTreeMap<String, String>> {
        let rest = header.trim().strip_prefix("Bearer ").or_else(|| header.trim().strip_prefix("bearer "))?;
        // Split on commas that are not inside quotes (a scope may list
        // several actions separated by commas).
        let mut parts = vec![String::new()];
        let mut quoted = false;
        for c in rest.chars() {
            match c {
                '"' => quoted = !quoted,
                ',' if !quoted => parts.push(String::new()),
                _ => {}
            }
            if c != ',' || quoted {
                parts.last_mut().expect("never empty").push(c);
            }
        }
        let mut out = BTreeMap::new();
        for part in &parts {
            let (k, v) = part.trim().split_once('=')?;
            out.insert(k.trim().to_ascii_lowercase(), v.trim().trim_matches('"').to_string());
        }
        out.contains_key("realm").then_some(out)
    }

    /// Anonymous pull token, as Docker Hub and most registries hand out.
    fn token_for(&mut self, name: &str, www_authenticate: &str) -> Result<String, OciError> {
        let ch = Self::challenge(www_authenticate).ok_or_else(|| reg("the registry asks for credentials this mirror cannot provide"))?;
        let mut url = ch["realm"].clone();
        let mut sep = if url.contains('?') { '&' } else { '?' };
        if let Some(service) = ch.get("service") {
            url.push_str(&format!("{sep}service={}", service.replace(' ', "%20")));
            sep = '&';
        }
        // Ask only for what is needed: pulling this one repository.
        url.push_str(&format!("{sep}scope=repository:{name}:pull"));
        let body = self.client.get(&url, &[])?.bytes(&url, MAX_TOKEN)?;
        let doc: Value = serde_json::from_slice(&body).map_err(|_| reg("token service did not answer with JSON"))?;
        let token = doc["token"].as_str().or(doc["access_token"].as_str()).ok_or_else(|| reg("token service gave no token"))?;
        if token.is_empty() || token.bytes().any(|b| b <= 0x20 || b == 0x7f) {
            return Err(reg("token service gave an unusable token"));
        }
        self.tokens.insert(name.to_string(), token.to_string());
        Ok(token.to_string())
    }

    fn get(&mut self, name: &str, path: &str, accept: &str) -> Result<HttpResponse, OciError> {
        let url = format!("{}/v2/{name}/{path}", self.registry);
        for attempt in 0..2 {
            let bearer = self.tokens.get(name).map(|t| format!("Bearer {t}"));
            let mut headers = vec![("Accept", accept)];
            if let Some(b) = &bearer {
                headers.push(("Authorization", b));
            }
            let resp = self.client.get_any(&url, &headers)?;
            match resp.status {
                200..=299 => return Ok(resp),
                401 if attempt == 0 => {
                    let challenge = resp.header("WWW-Authenticate").unwrap_or("").to_string();
                    self.token_for(name, &challenge)?;
                }
                404 => return Err(FetchError::NotFound(url).into()),
                status => return Err(FetchError::Http { path: url, status }.into()),
            }
        }
        Err(reg(format!("{url}: still unauthorized after getting a token")))
    }

    /// Fetch a manifest and check it against the digest it must have.
    fn manifest(&mut self, name: &str, reference: &Reference) -> Result<(Digest, Vec<u8>), OciError> {
        let r = match reference {
            Reference::Tag(t) => t.clone(),
            Reference::Digest(d) => format!("sha256:{d}"),
        };
        let resp = self.get(name, &format!("manifests/{r}"), ACCEPT)?;
        let claimed = resp.header("Docker-Content-Digest").map(str::to_string);
        let body = resp.bytes(&format!("{name}:{r}"), MAX_MANIFEST)?;
        let digest = Digest::of(&body);
        if let Reference::Digest(want) = reference {
            if digest != *want {
                return Err(reg(format!("{name}@sha256:{want}: the registry returned different content")));
            }
        }
        if let Some(c) = claimed {
            if parse_digest(&c) != Some(digest) {
                return Err(reg(format!("{name}:{r}: manifest does not match the digest the registry announced")));
            }
        }
        Ok((digest, body))
    }
}

/// Mirror the configured images into a new snapshot of `repo`.
///
/// A tag is resolved over TLS and that mapping is trusted as given. From
/// there on nothing is trusted: every manifest, config and layer must hash
/// to the digest that refers to it. Image signatures are not verified.
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &OciConfig) -> Result<OciReport, OciError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "oci" {
        return Err(OciError::Config(format!("repo {repo} has format {}, not oci", repo_row.format)));
    }
    if host_of(&cfg.registry).is_none() {
        return Err(OciError::Config(format!("{:?} is not an http(s) URL", cfg.registry)));
    }
    if cfg.images.is_empty() || cfg.platforms.is_empty() {
        return Err(OciError::Config("at least one image and one platform are needed".into()));
    }
    let mut p = Puller { client, registry: cfg.registry.trim_end_matches('/').to_string(), tokens: BTreeMap::new() };
    let mut report = OciReport::default();
    let mut entries: BTreeMap<String, Digest> = BTreeMap::new();

    for spec in &cfg.images {
        let (name, reference) = parse_image(spec).ok_or_else(|| OciError::Config(format!("image reference {spec:?} is not understood")))?;
        let (top, body) = p.manifest(&name, &reference)?;
        report.images.push((spec.clone(), format!("sha256:{top}")));
        if let Reference::Tag(tag) = &reference {
            let (d, _) = st.ingest_bytes(format!("sha256:{top}\n").as_bytes())?;
            entries.insert(format!("tags/{name}/{tag}"), d);
        }

        // The top object is either one image or an index of per-platform images.
        let mut images: Vec<(Digest, Vec<u8>)> = Vec::new();
        let doc: Value = serde_json::from_slice(&body).map_err(|_| reg(format!("{spec}: manifest is not JSON")))?;
        st.ingest_bytes(&body)?;
        entries.insert(format!("manifests/{name}/sha256/{top}"), top);
        match doc.get("manifests").and_then(Value::as_array) {
            Some(list) => {
                let mut kept: Vec<Digest> = Vec::new();
                for entry in list {
                    let platform = platform_of(entry);
                    if !platform_wanted(&cfg.platforms, &platform) {
                        continue;
                    }
                    let d = entry["digest"].as_str().and_then(parse_digest).ok_or_else(|| reg(format!("{spec}: index entry without a sha256 digest")))?;
                    let (got, bytes) = p.manifest(&name, &Reference::Digest(d))?;
                    st.ingest_bytes(&bytes)?;
                    entries.insert(format!("manifests/{name}/sha256/{got}"), got);
                    images.push((got, bytes));
                    kept.push(got);
                }
                // Build attestations (provenance, SBOM) are separate manifests
                // that point at the image they describe. Container engines
                // fetch them together with the image, so they travel with it.
                for entry in list {
                    let refers_to = entry.pointer("/annotations/vnd.docker.reference.digest").and_then(Value::as_str).and_then(parse_digest);
                    if !refers_to.is_some_and(|d| kept.contains(&d)) {
                        continue;
                    }
                    let d = entry["digest"].as_str().and_then(parse_digest).ok_or_else(|| reg(format!("{spec}: index entry without a sha256 digest")))?;
                    let (got, bytes) = p.manifest(&name, &Reference::Digest(d))?;
                    st.ingest_bytes(&bytes)?;
                    entries.insert(format!("manifests/{name}/sha256/{got}"), got);
                    images.push((got, bytes));
                }
                if kept.is_empty() {
                    let offered: Vec<String> = list.iter().map(platform_of).filter(|p| !p.starts_with("unknown")).collect();
                    return Err(reg(format!("{spec}: no image for {} (the registry offers {})", cfg.platforms.join(", "), offered.join(", "))));
                }
            }
            None => images.push((top, body)),
        }

        // Config and layers of each image, by digest.
        for (image_digest, bytes) in &images {
            let m: Value = serde_json::from_slice(bytes).map_err(|_| reg(format!("{name}@sha256:{image_digest}: manifest is not JSON")))?;
            if m["schemaVersion"].as_u64() != Some(2) || !m["config"].is_object() {
                return Err(reg(format!("{name}@sha256:{image_digest}: only schema version 2 image manifests are supported")));
            }
            report.manifests += 1;
            let mut wanted = vec![&m["config"]];
            wanted.extend(m["layers"].as_array().into_iter().flatten());
            for desc in wanted {
                let d = desc["digest"].as_str().and_then(parse_digest).ok_or_else(|| reg(format!("{name}: blob without a sha256 digest")))?;
                let size = desc["size"].as_u64();
                let path = format!("blobs/sha256/{d}");
                if entries.contains_key(&path) {
                    continue;
                }
                if desc["urls"].as_array().is_some_and(|u| !u.is_empty()) {
                    report.warnings.push(format!("{name}: layer sha256:{d} is distributed from another location and was not mirrored"));
                    continue;
                }
                if st.has_blob(&d)? {
                    report.reused += 1;
                } else {
                    let body = p.get(&name, &format!("blobs/sha256:{d}"), "application/octet-stream")?.into_reader();
                    let mut limited = body.take(size.unwrap_or(MAX_LAYER).min(MAX_LAYER) + 1);
                    let (_, got) = st.ingest(&mut limited, Expect { digest: Some(d), size })?;
                    report.downloaded += 1;
                    report.downloaded_bytes += got;
                }
                report.blobs += 1;
                entries.insert(path, d);
            }
        }
    }

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
