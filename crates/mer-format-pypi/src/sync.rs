//! Collector-side sync of a Python package index into one repo snapshot.
use crate::files::{parse_filename, safe_filename, FileKind, Targets};
use crate::{evaluate, parse_requirement, Env, SpecifierSet, Version};
use mer_blob::Expect;
use mer_core::{Digest, EntryRole};
use mer_fetch::{host_of, FetchError, HttpClient};
use mer_storage::{Storage, StorageError};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Read;

const JSON_ACCEPT: &str = "application/vnd.pypi.simple.v1+json";
const MAX_INDEX: u64 = 256 << 20;
const MAX_METADATA: u64 = 4 << 20;
const MAX_FILE: u64 = 16 << 30;

#[derive(Debug, thiserror::Error)]
pub enum PyError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("upstream index: {0}")]
    Index(String),
    #[error("{0}")]
    Config(String),
    #[error("no version of {project} satisfies {spec:?} for the configured Python targets")]
    Unsatisfiable { project: String, spec: String },
}

impl From<mer_meta::MetaError> for PyError {
    fn from(e: mer_meta::MetaError) -> Self {
        PyError::Storage(e.into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sdists {
    /// Wheels only.
    Never,
    /// Source archive only when a version has no installable wheel.
    IfNoWheel,
    Always,
}

#[derive(Clone, Debug)]
pub struct PyConfig {
    /// Base URL of the Simple API, for example `https://pypi.org/simple`.
    pub index_url: String,
    /// What to mirror, as pip requirement lines: `requests`, `django>=4.2,<5`.
    pub requirements: Vec<String>,
    /// CPython versions the air-gapped side uses, for example `3.11`.
    pub pythons: Vec<String>,
    pub arch: String,
    pub libcs: Vec<String>,
    pub sdists: Sdists,
    pub prereleases: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PyReport {
    /// (project, version) mirrored.
    pub projects: Vec<(String, String)>,
    pub files: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub reused: usize,
    pub warnings: Vec<String>,
    pub snapshot: Option<u64>,
}

/// One downloadable file of a project, as the upstream index describes it.
#[derive(Clone, Debug)]
struct UpFile {
    filename: String,
    url: String,
    sha256: Digest,
    size: Option<u64>,
    requires_python: Option<String>,
    /// PEP 658: metadata is available at `url + ".metadata"`.
    metadata: Option<Option<Digest>>,
    kind: FileKind,
}

/// The version picked for a project, and what to mirror of it.
#[derive(Clone, Debug)]
struct Chosen {
    version: Version,
    version_text: String,
    files: Vec<UpFile>,
}

struct Wanted {
    spec: SpecifierSet,
    extras: BTreeSet<String>,
    chosen: Option<Chosen>,
    /// Metadata of the chosen version: (file name it belongs to, bytes).
    metadata: Option<(String, Vec<u8>)>,
}

fn index_err(msg: impl Into<String>) -> PyError {
    PyError::Index(msg.into())
}

/// Resolve `href` against the URL of the page it appeared on.
fn resolve_url(page: &str, href: &str) -> Option<String> {
    let href = href.split('#').next()?;
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }
    let scheme_end = page.find("://")? + 3;
    let host_end = page[scheme_end..].find('/').map_or(page.len(), |i| scheme_end + i);
    let origin = &page[..host_end];
    let mut segments: Vec<&str> = if href.starts_with('/') {
        Vec::new()
    } else {
        let mut dir: Vec<&str> = page[host_end..].split('/').filter(|s| !s.is_empty()).collect();
        if !page.ends_with('/') {
            dir.pop();
        }
        dir
    };
    for seg in href.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    Some(format!("{origin}/{}", segments.join("/")))
}

fn parse_files(page_url: &str, doc: &Value, warnings: &mut Vec<String>) -> Result<Vec<UpFile>, PyError> {
    let list = doc.get("files").and_then(Value::as_array).ok_or_else(|| index_err(format!("{page_url}: no file list")))?;
    let mut out = Vec::new();
    for f in list {
        let Some(filename) = f.get("filename").and_then(Value::as_str) else { continue };
        if f.get("yanked").is_some_and(|y| y.as_bool() == Some(true) || y.is_string()) {
            continue; // withdrawn by its author
        }
        let Some(parsed) = parse_filename(filename) else { continue };
        let sha256 = f.pointer("/hashes/sha256").and_then(Value::as_str).and_then(|h| Digest::from_hex(h).ok());
        let (Some(sha256), Some(url)) = (sha256, f.get("url").and_then(Value::as_str).and_then(|u| resolve_url(page_url, u))) else {
            warnings.push(format!("{filename}: skipped, the index gives no SHA-256 or URL for it"));
            continue;
        };
        // "core-metadata" (PEP 714) or its older name: true, or {"sha256": ...}.
        let metadata = [f.get("core-metadata"), f.get("dist-info-metadata")].into_iter().flatten().find_map(|m| match m {
            Value::Bool(true) => Some(None),
            Value::Object(h) => Some(h.get("sha256").and_then(Value::as_str).and_then(|x| Digest::from_hex(x).ok())),
            _ => None,
        });
        out.push(UpFile {
            filename: filename.to_string(),
            url,
            sha256,
            size: f.get("size").and_then(Value::as_u64),
            requires_python: f.get("requires-python").and_then(Value::as_str).map(str::to_string),
            metadata,
            kind: parsed.kind,
        });
    }
    Ok(out)
}

struct Resolver<'a> {
    client: &'a HttpClient,
    cfg: &'a PyConfig,
    targets: Targets,
    /// Python versions used to test `Requires-Python` and markers. Each minor
    /// is tried at its first and at a late patch level; a dependency is kept
    /// if either says so, so the mirror errs towards including.
    pythons: Vec<String>,
    pages: BTreeMap<String, Vec<UpFile>>,
    warnings: Vec<String>,
}

impl Resolver<'_> {
    fn files_of(&mut self, project: &str) -> Result<&Vec<UpFile>, PyError> {
        if !self.pages.contains_key(project) {
            let url = format!("{}/{project}/", self.cfg.index_url.trim_end_matches('/'));
            let body = match self.client.get(&url, &[("Accept", JSON_ACCEPT)]) {
                Ok(r) => r.bytes(&url, MAX_INDEX)?,
                Err(FetchError::NotFound(_)) => return Err(index_err(format!("project {project} does not exist upstream"))),
                Err(e) => return Err(e.into()),
            };
            let doc: Value = serde_json::from_slice(&body)
                .map_err(|_| index_err(format!("{url}: not a JSON Simple API answer (PEP 691); is this index too old?")))?;
            let files = parse_files(&url, &doc, &mut self.warnings)?;
            self.pages.insert(project.to_string(), files);
        }
        Ok(&self.pages[project])
    }

    fn python_allows(&self, requires_python: &Option<String>) -> bool {
        let Some(text) = requires_python else { return true };
        // Unreadable Requires-Python values exist in the wild; pip ignores them too.
        let Some(spec) = SpecifierSet::parse(text) else { return true };
        self.pythons.iter().any(|p| Version::parse(p).is_some_and(|v| spec.matches(&v)))
    }

    /// Newest version that satisfies `spec` and has something installable.
    fn choose(&mut self, project: &str, spec: &SpecifierSet) -> Result<Option<Chosen>, PyError> {
        let files = self.files_of(project)?.clone();
        let mut by_version: BTreeMap<Version, Vec<UpFile>> = BTreeMap::new();
        let mut texts: BTreeMap<Version, String> = BTreeMap::new();
        for f in files {
            let Some(parsed) = parse_filename(&f.filename) else { continue };
            texts.entry(parsed.version.clone()).or_insert_with(|| version_text(&f.filename, &f.kind));
            by_version.entry(parsed.version).or_default().push(f);
        }
        let want_pre = self.cfg.prereleases || spec.mentions_prerelease();
        // Like pip: final releases first, pre-releases only if nothing else fits.
        for allow_pre in [want_pre, true] {
            for (version, files) in by_version.iter().rev() {
                if !spec.matches(version) || (version.is_prerelease() && !allow_pre) {
                    continue;
                }
                let usable: Vec<&UpFile> = files.iter().filter(|f| self.python_allows(&f.requires_python)).collect();
                let wheels: Vec<UpFile> = usable.iter().filter(|f| self.targets.wheel_ok(&f.kind)).map(|f| (*f).clone()).collect();
                let sdists: Vec<UpFile> = usable.iter().filter(|f| f.kind == FileKind::Sdist).map(|f| (*f).clone()).collect();
                let mut keep = wheels.clone();
                match self.cfg.sdists {
                    Sdists::Always => keep.extend(sdists),
                    Sdists::IfNoWheel if wheels.is_empty() => keep.extend(sdists),
                    _ => {}
                }
                if keep.is_empty() {
                    continue;
                }
                keep.sort_by(|a, b| a.filename.cmp(&b.filename));
                return Ok(Some(Chosen { version: version.clone(), version_text: texts[version].clone(), files: keep }));
            }
            if want_pre {
                break;
            }
        }
        Ok(None)
    }

    /// Core metadata of the chosen version, from the first wheel that offers it.
    fn metadata(&mut self, chosen: &Chosen) -> Result<Option<(String, Vec<u8>)>, PyError> {
        for f in &chosen.files {
            let Some(expected) = &f.metadata else { continue };
            let url = format!("{}.metadata", f.url);
            let body = self.client.get(&url, &[])?.bytes(&url, MAX_METADATA)?;
            if let Some(d) = expected {
                if Digest::of(&body) != *d {
                    return Err(index_err(format!("{url}: content does not match the hash in the index")));
                }
            }
            return Ok(Some((f.filename.clone(), body)));
        }
        Ok(None)
    }

    /// Requirements from core metadata that apply to the targets.
    fn dependencies(&mut self, project: &str, extras: &BTreeSet<String>, metadata: &[u8]) -> Vec<crate::Requirement> {
        let text = String::from_utf8_lossy(metadata);
        let mut out = Vec::new();
        for line in text.lines().take_while(|l| !l.trim().is_empty()) {
            let Some(value) = line.strip_prefix("Requires-Dist:") else { continue };
            let req = match parse_requirement(value) {
                Ok(r) => r,
                Err(e) => {
                    self.warnings.push(format!("{project}: ignored dependency, {e}"));
                    continue;
                }
            };
            if req.direct_url {
                self.warnings.push(format!("{project}: depends on {} by URL, which a mirror cannot follow", req.name));
                continue;
            }
            let applies = match &req.marker {
                None => true,
                Some(marker) => {
                    let mut envs = Vec::new();
                    for py in &self.pythons {
                        let base = Env::cpython_linux(py, &self.targets.arch);
                        envs.extend(extras.iter().map(|e| base.clone().with_extra(e)));
                        envs.push(base);
                    }
                    let mut any = false;
                    for env in &envs {
                        match evaluate(marker, env) {
                            Ok(v) => any |= v,
                            Err(e) => {
                                // Unknown marker: include rather than risk a hole in the mirror.
                                self.warnings.push(format!("{project}: cannot evaluate marker {marker:?} ({e}); dependency kept"));
                                any = true;
                                break;
                            }
                        }
                    }
                    any
                }
            };
            if applies {
                out.push(req);
            }
        }
        out
    }
}

/// The version as the file name spells it (kept for display).
fn version_text(filename: &str, kind: &FileKind) -> String {
    match kind {
        FileKind::Wheel { .. } => filename.split('-').nth(1).unwrap_or("").to_string(),
        FileKind::Sdist => {
            let stem = [".tar.gz", ".zip", ".tar.bz2", ".tar.xz"].iter().find_map(|e| filename.strip_suffix(e)).unwrap_or(filename);
            stem.rsplit_once('-').map_or("", |(_, v)| v).to_string()
        }
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// PEP 503 page for one project.
fn project_page(project: &str, files: &[(&UpFile, Option<Digest>)]) -> String {
    let mut out = format!(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta name=\"pypi:repository-version\" content=\"1.0\">\n<title>Links for {0}</title>\n</head>\n<body>\n<h1>Links for {0}</h1>\n",
        escape(project)
    );
    for (f, metadata) in files {
        out.push_str(&format!("<a href=\"../../packages/{}/{}#sha256={}\"", escape(project), escape(&f.filename), f.sha256));
        if let Some(rp) = &f.requires_python {
            out.push_str(&format!(" data-requires-python=\"{}\"", escape(rp)));
        }
        if let Some(d) = metadata {
            // Both spellings: newer and older pip each read one of them.
            out.push_str(&format!(" data-core-metadata=\"sha256={d}\" data-dist-info-metadata=\"sha256={d}\""));
        }
        out.push_str(&format!(">{}</a><br>\n", escape(&f.filename)));
    }
    out.push_str("</body>\n</html>\n");
    out
}

fn root_page(projects: &[&String]) -> String {
    let mut out = String::from(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta name=\"pypi:repository-version\" content=\"1.0\">\n<title>Simple index</title>\n</head>\n<body>\n",
    );
    for p in projects {
        out.push_str(&format!("<a href=\"{0}/\">{0}</a><br>\n", escape(p)));
    }
    out.push_str("</body>\n</html>\n");
    out
}

/// Mirror the configured requirements and their dependencies into a new
/// snapshot of `repo`.
///
/// Python indexes are not signed. What is trusted here is the TLS connection
/// to the index and the SHA-256 it states for each file; every download is
/// checked against that hash before it enters the store.
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &PyConfig) -> Result<PyReport, PyError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "pypi" {
        return Err(PyError::Config(format!("repo {repo} has format {}, not pypi", repo_row.format)));
    }
    if host_of(&cfg.index_url).is_none() {
        return Err(PyError::Config(format!("{:?} is not an http(s) URL", cfg.index_url)));
    }
    let mut minors = Vec::new();
    let mut pythons = Vec::new();
    for p in &cfg.pythons {
        let minor = p.strip_prefix("3.").and_then(|m| m.parse::<u32>().ok())
            .ok_or_else(|| PyError::Config(format!("python target {p:?} must look like 3.11")))?;
        minors.push(minor);
        pythons.push(format!("3.{minor}.0"));
        pythons.push(format!("3.{minor}.99"));
    }
    if minors.is_empty() || cfg.requirements.is_empty() {
        return Err(PyError::Config("at least one python target and one requirement are needed".into()));
    }
    let mut r = Resolver {
        client,
        cfg,
        targets: Targets { minors, arch: cfg.arch.clone(), libcs: cfg.libcs.clone() },
        pythons,
        pages: BTreeMap::new(),
        warnings: Vec::new(),
    };

    // 1. Roots.
    let mut wanted: BTreeMap<String, Wanted> = BTreeMap::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut roots: BTreeSet<String> = BTreeSet::new();
    for line in &cfg.requirements {
        let req = parse_requirement(line).map_err(PyError::Config)?;
        if req.direct_url || req.marker.is_some() {
            return Err(PyError::Config(format!("requirement {line:?}: URLs and markers are not supported here")));
        }
        let w = wanted.entry(req.name.clone()).or_insert_with(|| Wanted {
            spec: SpecifierSet::default(),
            extras: BTreeSet::new(),
            chosen: None,
            metadata: None,
        });
        w.spec.and(&req.spec);
        w.extras.extend(req.extras);
        roots.insert(req.name.clone());
        queue.push_back(req.name);
    }

    // 2. Follow dependencies until nothing changes. When two packages ask
    //    for the same project, its constraints are combined and it is chosen
    //    again. This is not a full solver: it never backtracks.
    let mut rounds = 0;
    while let Some(name) = queue.pop_front() {
        rounds += 1;
        if rounds > 20_000 {
            return Err(index_err("dependency resolution does not settle"));
        }
        let (spec, extras) = {
            let w = &wanted[&name];
            (w.spec.clone(), w.extras.clone())
        };
        let Some(chosen) = r.choose(&name, &spec)? else {
            if roots.contains(&name) {
                return Err(PyError::Unsatisfiable { project: name, spec: spec.to_string() });
            }
            r.warnings.push(format!("{name}: no version satisfies {:?}; not mirrored", spec.to_string()));
            continue;
        };
        let metadata = r.metadata(&chosen)?;
        if metadata.is_none() {
            r.warnings.push(format!(
                "{name} {}: the index offers no metadata for it, so its dependencies were not followed",
                chosen.version_text
            ));
        }
        let deps = metadata.as_ref().map(|(_, m)| r.dependencies(&name, &extras, m)).unwrap_or_default();
        {
            let w = wanted.get_mut(&name).expect("queued names are wanted");
            w.chosen = Some(chosen);
            w.metadata = metadata;
        }
        for dep in deps {
            let w = wanted.entry(dep.name.clone()).or_insert_with(|| Wanted {
                spec: SpecifierSet::default(),
                extras: BTreeSet::new(),
                chosen: None,
                metadata: None,
            });
            let before = (w.spec.clone(), w.extras.len());
            w.spec.and(&dep.spec);
            w.extras.extend(dep.extras);
            let changed = before != (w.spec.clone(), w.extras.len());
            // Re-choose only if the current choice no longer fits, or new extras matter.
            let still_fits = w.chosen.as_ref().is_some_and(|c| w.spec.matches(&c.version)) && before.1 == w.extras.len();
            if (w.chosen.is_none() || (changed && !still_fits)) && !queue.contains(&dep.name) {
                queue.push_back(dep.name);
            }
        }
    }

    // 3. Download and lay out the repository.
    let mut report = PyReport::default();
    let mut entries: BTreeMap<String, Digest> = BTreeMap::new();
    let mut mirrored: Vec<&String> = Vec::new();
    for (name, w) in &wanted {
        let Some(chosen) = &w.chosen else { continue };
        mirrored.push(name);
        report.projects.push((name.clone(), chosen.version_text.clone()));
        let mut listed: Vec<(&UpFile, Option<Digest>)> = Vec::new();
        for f in &chosen.files {
            if !safe_filename(&f.filename) {
                continue;
            }
            if st.has_blob(&f.sha256)? {
                report.reused += 1;
            } else {
                let limit = f.size.unwrap_or(MAX_FILE);
                let mut body = client.get(&f.url, &[])?.into_reader().take(limit + 1);
                let (_, size) = st.ingest(&mut body, Expect { digest: Some(f.sha256), size: f.size })?;
                report.downloaded += 1;
                report.downloaded_bytes += size;
            }
            report.files += 1;
            entries.insert(format!("packages/{name}/{}", f.filename), f.sha256);
            let metadata = match &w.metadata {
                Some((owner, bytes)) if *owner == f.filename => {
                    let (d, _) = st.ingest_bytes(bytes)?;
                    entries.insert(format!("packages/{name}/{}.metadata", f.filename), d);
                    Some(d)
                }
                _ => None,
            };
            listed.push((f, metadata));
        }
        let (d, _) = st.ingest_bytes(project_page(name, &listed).as_bytes())?;
        entries.insert(format!("simple/{name}/index.html"), d);
    }
    let (d, _) = st.ingest_bytes(root_page(&mirrored).as_bytes())?;
    entries.insert("simple/index.html".into(), d);

    // 4. One snapshot: index pages and files change together.
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
    r.warnings.sort();
    r.warnings.dedup();
    report.warnings = r.warnings;
    Ok(report)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn urls_resolve_like_a_browser_would() {
        let page = "https://pypi.org/simple/requests/";
        assert_eq!(resolve_url(page, "https://files.example/a.whl#sha256=ab").unwrap(), "https://files.example/a.whl");
        assert_eq!(resolve_url(page, "../../packages/ab/a.whl").unwrap(), "https://pypi.org/packages/ab/a.whl");
        assert_eq!(resolve_url(page, "a.whl").unwrap(), "https://pypi.org/simple/requests/a.whl");
        assert_eq!(resolve_url(page, "/files/a.whl").unwrap(), "https://pypi.org/files/a.whl");
        assert_eq!(resolve_url("http://h:8080/simple/x", "a.whl").unwrap(), "http://h:8080/simple/a.whl");
        assert_eq!(resolve_url(page, "../../../../../etc/passwd").unwrap(), "https://pypi.org/etc/passwd", "cannot climb above the host");
    }

    #[test]
    fn pages_escape_everything_that_came_from_upstream() {
        let f = UpFile {
            filename: "a-1.0-py3-none-any.whl".into(),
            url: String::new(),
            sha256: Digest::of(b"x"),
            size: None,
            requires_python: Some(">=3.8,<4\"><script>alert(1)</script>".into()),
            metadata: None,
            kind: FileKind::Sdist,
        };
        let page = project_page("a", &[(&f, Some(Digest::of(b"m")))]);
        assert!(!page.contains("<script>"), "{page}");
        assert!(page.contains("data-requires-python=\"&gt;=3.8,&lt;4&quot;&gt;&lt;script&gt;"));
        assert!(page.contains(&format!("#sha256={}", Digest::of(b"x"))));
        assert!(page.contains(&format!("data-core-metadata=\"sha256={}\"", Digest::of(b"m"))));
        assert!(root_page(&[&"a".to_string()]).contains("<a href=\"a/\">a</a>"));
    }
}
