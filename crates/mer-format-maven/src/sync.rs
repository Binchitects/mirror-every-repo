//! Collector-side sync of a Maven repository into one repo snapshot.
use crate::pom::{interpolate, parse_pom, parse_xml, type_file, Dep, Pom};
use crate::version::{compare, Range};
use mer_core::{Digest, EntryRole};
use mer_fetch::{host_of, FetchError, HttpClient};
use mer_storage::{Storage, StorageError};
use sha1::Sha1;
use sha2::{Digest as _, Sha256, Sha512};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

const MAX_POM: u64 = 16 << 20;
const MAX_FILE: u64 = 4 << 30;
const MAX_ARTIFACTS: usize = 50_000;

#[derive(Debug, thiserror::Error)]
pub enum MavenError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("upstream repository: {0}")]
    Upstream(String),
    #[error("{0}")]
    Config(String),
}

impl From<mer_meta::MetaError> for MavenError {
    fn from(e: mer_meta::MetaError) -> Self {
        MavenError::Storage(e.into())
    }
}

#[derive(Clone, Debug)]
pub struct MavenConfig {
    /// Repository root, for example `https://repo.maven.apache.org/maven2`.
    pub repo: String,
    /// Artifacts to mirror with what they need: `group:artifact[:ext[:classifier]]:version`,
    /// or `group:artifact` for the newest release.
    pub artifacts: Vec<String>,
    /// Local Maven repositories (`~/.m2/repository` after a build on a
    /// connected machine): exactly the artifacts they hold are mirrored.
    pub m2_dirs: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MavenReport {
    pub files: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub reused: usize,
    /// Files upstream offers no checksum for.
    pub unverified: usize,
    pub warnings: Vec<String>,
    pub snapshot: Option<u64>,
}

fn up(msg: impl Into<String>) -> MavenError {
    MavenError::Upstream(msg.into())
}

fn segment_ok(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && s.len() <= 200 && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'+' | b'~'))
}

fn coord_ok(g: &str, a: &str, v: &str) -> bool {
    g.split('.').all(segment_ok) && segment_ok(a) && segment_ok(v)
}

/// The repository path of an artifact file.
pub fn file_path(g: &str, a: &str, v: &str, classifier: Option<&str>, ext: &str) -> String {
    let c = classifier.map(|c| format!("-{c}")).unwrap_or_default();
    format!("{}/{a}/{v}/{a}-{v}{c}.{ext}", g.replace('.', "/"))
}

/// `g:a:v`, `g:a:ext:v`, `g:a:ext:classifier:v`, `g:a` -> (g, a, version, ext, classifier).
type Spec = (String, String, Option<String>, String, Option<String>);

fn split_spec(s: &str) -> Option<Spec> {
    let p: Vec<&str> = s.trim().split(':').collect();
    let (g, a, v, ext, cls) = match p[..] {
        [g, a] => (g, a, None, "jar", None),
        [g, a, v] => (g, a, Some(v), "jar", None),
        [g, a, e, v] => (g, a, Some(v), e, None),
        [g, a, e, c, v] => (g, a, Some(v), e, Some(c)),
        _ => return None,
    };
    let ok = coord_ok(g, a, v.unwrap_or("x")) && segment_ok(ext) && cls.is_none_or(segment_ok);
    ok.then(|| (g.into(), a.into(), v.map(String::from), ext.into(), cls.map(String::from)))
}

/// What a local repository file is: skip Maven's bookkeeping files.
fn m2_file_wanted(name: &str) -> bool {
    let skip_ext = [".sha1", ".md5", ".sha256", ".sha512", ".asc", ".lastUpdated", ".part", ".lock", ".tmp"];
    !(name == "_remote.repositories" || name == "resolver-status.properties" || name.starts_with("maven-metadata") || skip_ext.iter().any(|e| name.ends_with(e)))
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        let t = e.file_type()?;
        if t.is_dir() {
            walk(&e.path(), root, out)?;
        } else if t.is_file() {
            let name = e.file_name().to_string_lossy().into_owned();
            if m2_file_wanted(&name) {
                if let Ok(rel) = e.path().strip_prefix(root) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    Ok(())
}

/// (group, artifact, version) of a repository file path, if it has that shape.
fn coords_of(path: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() < 4 {
        return None;
    }
    let n = parts.len();
    let (v, a) = (parts[n - 2], parts[n - 3]);
    let g = parts[..n - 3].join(".");
    let file = parts[n - 1];
    (file.starts_with(&format!("{a}-{v}")) && coord_ok(&g, a, v)).then(|| (g, a.to_string(), v.to_string()))
}

struct Remote<'a> {
    st: &'a Storage,
    client: &'a HttpClient,
    base: String,
    previous: &'a BTreeMap<String, Digest>,
    files: BTreeMap<String, Digest>,
    poms: BTreeMap<(String, String, String), Option<Pom>>,
    report: MavenReport,
}

impl Remote<'_> {
    /// Mirror one file with the strongest checksum upstream offers.
    /// `Ok(false)` when upstream does not have it.
    fn file(&mut self, path: &str) -> Result<bool, MavenError> {
        if self.files.contains_key(path) {
            return Ok(true);
        }
        if mer_core::validate_path(path).is_err() || !path.split('/').all(segment_ok) {
            return Err(up(format!("refusing repository path {path:?}")));
        }
        if self.files.len() >= MAX_ARTIFACTS {
            return Err(up("more files than plausible"));
        }
        if let Some(d) = self.previous.get(path) {
            if self.st.has_blob(d)? {
                self.report.reused += 1;
                self.files.insert(path.to_string(), *d);
                self.checksum_files(path, d)?;
                return Ok(true);
            }
        }
        let url = format!("{}/{path}", self.base);
        let body = match self.client.get(&url, &[]) {
            Ok(r) => r.bytes(&url, MAX_FILE)?,
            Err(FetchError::NotFound(_)) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let mut verified = false;
        for (ext, len) in [("sha512", 128), ("sha256", 64), ("sha1", 40)] {
            let sum_url = format!("{url}.{ext}");
            let text = match self.client.get(&sum_url, &[]) {
                Ok(r) => String::from_utf8_lossy(&r.bytes(&sum_url, 4096)?).into_owned(),
                Err(FetchError::NotFound(_)) => continue,
                Err(e) => return Err(e.into()),
            };
            let want = text.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            if want.len() != len || !want.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let got = match ext {
                "sha512" => hex::encode(Sha512::digest(&body)),
                "sha256" => hex::encode(Sha256::digest(&body)),
                _ => hex::encode(Sha1::digest(&body)),
            };
            if got != want {
                return Err(up(format!("{path}: does not match its {ext} checksum")));
            }
            verified = true;
            break;
        }
        if !verified {
            self.report.unverified += 1;
            self.report.warnings.push(format!("{path}: upstream offers no checksum; mirrored as downloaded"));
        }
        let (d, size) = self.st.ingest_bytes(&body)?;
        self.report.downloaded += 1;
        self.report.downloaded_bytes += size;
        self.files.insert(path.to_string(), d);
        self.checksum_files(path, &d)?;
        Ok(true)
    }

    /// `.sha1` next to each file, which Maven checks by default.
    fn checksum_files(&mut self, path: &str, d: &Digest) -> Result<(), MavenError> {
        let body = self.st.read_blob(d)?;
        let (s1, _) = self.st.ingest_bytes(hex::encode(Sha1::digest(&body)).as_bytes())?;
        let (s256, _) = self.st.ingest_bytes(d.to_string().as_bytes())?;
        self.files.insert(format!("{path}.sha1"), s1);
        self.files.insert(format!("{path}.sha256"), s256);
        Ok(())
    }

    fn pom(&mut self, g: &str, a: &str, v: &str) -> Result<Option<Pom>, MavenError> {
        let key = (g.to_string(), a.to_string(), v.to_string());
        if let Some(p) = self.poms.get(&key) {
            return Ok(p.clone());
        }
        let path = file_path(g, a, v, None, "pom");
        let parsed = if self.file(&path)? {
            let text = String::from_utf8_lossy(&self.st.read_blob(&self.files[&path])?).into_owned();
            if text.len() as u64 > MAX_POM {
                return Err(up(format!("{path}: too large")));
            }
            Some(parse_pom(&text).map_err(|e| up(format!("{path}: {e}")))?)
        } else {
            None
        };
        self.poms.insert(key, parsed.clone());
        Ok(parsed)
    }

    /// Versions upstream lists for an artifact.
    fn versions(&mut self, g: &str, a: &str) -> Result<Vec<String>, MavenError> {
        let url = format!("{}/{}/{a}/maven-metadata.xml", self.base, g.replace('.', "/"));
        let body = match self.client.get(&url, &[]) {
            Ok(r) => r.bytes(&url, MAX_POM)?,
            Err(FetchError::NotFound(_)) => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let md = parse_xml(&String::from_utf8_lossy(&body)).map_err(|e| up(format!("{url}: {e}")))?;
        Ok(md.all(&["versioning", "versions", "version"]).iter().map(|v| v.text.trim().to_string()).filter(|v| !v.is_empty()).collect())
    }
}

/// The effective model of a POM: inheritance, properties, imports applied.
struct Effective {
    packaging: String,
    deps: Vec<Dep>,
    managed: BTreeMap<String, Dep>,
    relocation: Option<(String, String, String)>,
}

fn effective(r: &mut Remote, g: &str, a: &str, v: &str, depth: usize) -> Result<Option<Effective>, MavenError> {
    if depth > 24 {
        return Err(up(format!("{g}:{a}:{v}: parent or import chain is too long")));
    }
    let Some(pom) = r.pom(g, a, v)? else { return Ok(None) };
    // Raw inherited model: the parent's, then this POM over it.
    let mut props: BTreeMap<String, String> = BTreeMap::new();
    let mut deps: BTreeMap<String, Dep> = BTreeMap::new();
    let mut managed: BTreeMap<String, Dep> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut chain: Vec<Pom> = vec![pom.clone()];
    let mut parent = pom.parent.clone();
    while let Some((pg, pa, pv)) = parent {
        if chain.len() > 24 {
            return Err(up(format!("{g}:{a}:{v}: parent chain is too long")));
        }
        match r.pom(&pg, &pa, &pv)? {
            Some(p) => {
                parent = p.parent.clone();
                chain.push(p);
            }
            None => {
                r.report.warnings.push(format!("{g}:{a}:{v}: parent {pg}:{pa}:{pv} is not in the repository"));
                break;
            }
        }
    }
    for p in chain.iter().rev() {
        props.extend(p.props.clone());
        for d in &p.managed {
            managed.insert(d.key(), d.clone());
        }
        for d in &p.deps {
            if !deps.contains_key(&d.key()) {
                order.push(d.key());
            }
            deps.insert(d.key(), d.clone());
        }
    }
    let group = pom.group.clone().or_else(|| pom.parent.as_ref().map(|p| p.0.clone())).unwrap_or_default();
    let version = pom.version.clone().or_else(|| pom.parent.as_ref().map(|p| p.2.clone())).unwrap_or_default();
    for (k, val) in [("project.groupId", &group), ("project.artifactId", &pom.artifact), ("project.version", &version), ("pom.version", &version), ("version", &version), ("pom.groupId", &group), ("groupId", &group)] {
        props.insert(k.to_string(), val.clone());
    }
    if let Some((pg, _, pv)) = &pom.parent {
        props.insert("project.parent.version".into(), pv.clone());
        props.insert("parent.version".into(), pv.clone());
        props.insert("project.parent.groupId".into(), pg.clone());
    }
    let snapshot: BTreeMap<String, String> = props.clone();
    let props: BTreeMap<String, String> = snapshot.iter().map(|(k, v)| (k.clone(), interpolate(v, &snapshot))).collect();

    // Interpolate, then import BOMs (scope import) into dependency management.
    let mut resolved: BTreeMap<String, Dep> = BTreeMap::new();
    let mut imports = Vec::new();
    for (_, mut d) in managed {
        d.interpolate(&props);
        if d.scope.as_deref() == Some("import") && d.kind == "pom" {
            imports.push(d);
        } else {
            resolved.insert(d.key(), d);
        }
    }
    for d in imports {
        let Some(v) = d.version.clone() else { continue };
        if !coord_ok(&d.group, &d.artifact, &v) {
            r.report.warnings.push(format!("{g}:{a}:{v}: imports {}:{}:{v}, which is not a usable coordinate", d.group, d.artifact));
            continue;
        }
        match effective(r, &d.group, &d.artifact, &v, depth + 1)? {
            Some(bom) => {
                for (k, m) in bom.managed {
                    resolved.entry(k).or_insert(m);
                }
            }
            None => r.report.warnings.push(format!("{g}:{a}:{v}: imported BOM {}:{}:{v} is not in the repository", d.group, d.artifact)),
        }
    }
    let mut out_deps = Vec::new();
    for k in order {
        let mut d = deps[&k].clone();
        d.interpolate(&props);
        if let Some(m) = resolved.get(&d.key()) {
            if d.version.is_none() {
                d.version = m.version.clone();
            }
            if d.scope.is_none() {
                d.scope = m.scope.clone();
            }
            d.exclusions.extend(m.exclusions.clone());
        }
        out_deps.push(d);
    }
    let relocation = pom.relocation.map(|(rg, ra, rv)| (rg.unwrap_or_else(|| group.clone()), ra.unwrap_or_else(|| pom.artifact.clone()), rv.unwrap_or_else(|| version.clone())));
    Ok(Some(Effective { packaging: interpolate(&pom.packaging, &props), deps: out_deps, managed: resolved, relocation }))
}

/// Mirror the configured artifacts into a new snapshot of `repo`.
///
/// What is trusted is the TLS connection to the repository and the
/// checksum files it serves; every file is checked against the strongest of
/// SHA-512, SHA-256 and SHA-1 that upstream has. PGP signatures are not
/// verified.
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &MavenConfig) -> Result<MavenReport, MavenError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "maven" {
        return Err(MavenError::Config(format!("repo {repo} has format {}, not maven", repo_row.format)));
    }
    let base = cfg.repo.trim_end_matches('/').to_string();
    if host_of(&base).is_none() {
        return Err(MavenError::Config(format!("{:?} is not an http(s) URL", cfg.repo)));
    }
    if cfg.artifacts.is_empty() && cfg.m2_dirs.is_empty() {
        return Err(MavenError::Config("name at least one artifact or local repository".into()));
    }
    let mut specs = Vec::new();
    for s in &cfg.artifacts {
        specs.push(split_spec(s).ok_or_else(|| MavenError::Config(format!("artifact {s:?} is not understood (group:artifact[:ext[:classifier]]:version)")))?);
    }
    let mut exact = Vec::new();
    for dir in &cfg.m2_dirs {
        walk(dir, dir, &mut exact).map_err(|e| MavenError::Config(format!("reading {}: {e}", dir.display())))?;
    }
    exact.sort();

    let previous: BTreeMap<String, Digest> = match st.head(repo)? {
        Some(h) => st.meta().read(|t| t.entries_at(repo_row.id, h.seq))?.into_iter().map(|e| (e.path, e.digest)).collect(),
        None => BTreeMap::new(),
    };
    let mut r = Remote { st, client, base, previous: &previous, files: BTreeMap::new(), poms: BTreeMap::new(), report: MavenReport::default() };

    // 1. Exactly what local repositories hold.
    for path in &exact {
        if coords_of(path).is_none() {
            r.report.warnings.push(format!("{path}: not an artifact path; skipped"));
            continue;
        }
        if !r.file(path)? {
            return Err(up(format!("{path}: the local repository has it but upstream does not (built locally, or from another repository?)")));
        }
    }
    // POMs of the exact set, for the version and plugin metadata.
    for path in exact.iter().filter(|p| p.ends_with(".pom")) {
        if let Some((g, a, v)) = coords_of(path) {
            r.pom(&g, &a, &v)?;
        }
    }

    // 2. Artifacts with what they need: compile and runtime dependencies,
    //    transitively, minus optional ones and exclusions.
    type Node = (String, String, String, String, Option<String>, BTreeSet<(String, String)>);
    let mut queue: VecDeque<Node> = VecDeque::new();
    for (g, a, v, ext, cls) in specs {
        let v = match v {
            Some(v) => v,
            None => {
                let mut all = r.versions(&g, &a)?;
                all.retain(|v| !v.ends_with("-SNAPSHOT"));
                all.sort_by(|x, y| compare(x, y));
                all.pop().ok_or_else(|| up(format!("{g}:{a}: the repository lists no release")))?
            }
        };
        if r.pom(&g, &a, &v)?.is_none() && ext != "pom" {
            return Err(up(format!("{g}:{a}:{v} is not in the repository")));
        }
        queue.push_back((g, a, v, ext, cls, BTreeSet::new()));
    }
    let mut seen = BTreeSet::new();
    while let Some((g, a, v, ext, cls, excl)) = queue.pop_front() {
        if !seen.insert((g.clone(), a.clone(), v.clone(), ext.clone(), cls.clone())) {
            continue;
        }
        let Some(model) = effective(&mut r, &g, &a, &v, 0)? else {
            r.report.warnings.push(format!("{g}:{a}:{v}: not in the repository; left out"));
            continue;
        };
        if let Some((rg, ra, rv)) = model.relocation.clone().filter(|(rg, ra, rv)| (rg, ra, rv) != (&g, &a, &v)) {
            queue.push_back((rg, ra, rv, ext, cls, excl));
            continue;
        }
        if ext != "pom" && model.packaging != "pom" {
            let path = file_path(&g, &a, &v, cls.as_deref(), &ext);
            if !r.file(&path)? {
                r.report.warnings.push(format!("{path}: not in the repository"));
            }
        }
        let _ = &model.managed;
        for d in model.deps {
            let scope = d.scope.as_deref().unwrap_or("compile");
            if !matches!(scope, "compile" | "runtime") || d.optional {
                continue;
            }
            if excl.iter().any(|(eg, ea)| (eg == "*" || *eg == d.group) && (ea == "*" || *ea == d.artifact)) {
                continue;
            }
            let Some(spec) = d.version.clone() else {
                r.report.warnings.push(format!("{g}:{a}:{v}: dependency {}:{} has no version; left out", d.group, d.artifact));
                continue;
            };
            let dv = match Range::parse(&spec) {
                None => spec,
                Some(Ok(range)) => {
                    let mut all = r.versions(&d.group, &d.artifact)?;
                    all.retain(|x| range.contains(x));
                    all.sort_by(|x, y| compare(x, y));
                    match all.pop() {
                        Some(x) => x,
                        None => {
                            r.report.warnings.push(format!("{g}:{a}:{v}: no version of {}:{} is in {spec}", d.group, d.artifact));
                            continue;
                        }
                    }
                }
                Some(Err(e)) => {
                    r.report.warnings.push(format!("{g}:{a}:{v}: {e}"));
                    continue;
                }
            };
            if !coord_ok(&d.group, &d.artifact, &dv) {
                r.report.warnings.push(format!("{g}:{a}:{v}: dependency {}:{}:{dv} is not a usable coordinate (unresolved property?)", d.group, d.artifact));
                continue;
            }
            let (dext, implied) = type_file(&d.kind);
            let dcls = d.classifier.clone().or(implied.map(String::from));
            let mut sub = excl.clone();
            sub.extend(d.exclusions.iter().cloned());
            queue.push_back((d.group, d.artifact, dv, dext.to_string(), dcls, sub));
        }
    }

    // 3. Version metadata for every artifact, plugin prefixes per group.
    let mut versions: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let mut plugins: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for path in r.files.keys().filter(|p| p.ends_with(".pom")) {
        if let Some((g, a, v)) = coords_of(path) {
            versions.entry((g.clone(), a.clone())).or_default().insert(v.clone());
            if r.poms.get(&(g.clone(), a.clone(), v)).and_then(|p| p.as_ref()).is_some_and(|p| p.packaging == "maven-plugin") {
                plugins.entry(g).or_default().insert(a);
            }
        }
    }
    let mut generated: Vec<(String, String)> = Vec::new();
    for ((g, a), vs) in &versions {
        let mut vs: Vec<&String> = vs.iter().collect();
        vs.sort_by(|x, y| compare(x, y));
        let latest = vs.last().map(|s| s.as_str()).unwrap_or("");
        let release = vs.iter().rev().find(|v| !v.ends_with("-SNAPSHOT")).map(|s| s.as_str()).unwrap_or("");
        let list: String = vs.iter().map(|v| format!("      <version>{v}</version>\n")).collect();
        generated.push((
            format!("{}/{a}/maven-metadata.xml", g.replace('.', "/")),
            format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata>\n  <groupId>{g}</groupId>\n  <artifactId>{a}</artifactId>\n  <versioning>\n    <latest>{latest}</latest>\n    <release>{release}</release>\n    <versions>\n{list}    </versions>\n    <lastUpdated>20000101000000</lastUpdated>\n  </versioning>\n</metadata>\n"),
        ));
    }
    for (g, arts) in &plugins {
        let entries: String = arts
            .iter()
            .map(|a| {
                let prefix = a.strip_prefix("maven-").and_then(|x| x.strip_suffix("-plugin")).or_else(|| a.strip_suffix("-maven-plugin")).unwrap_or(a);
                format!("    <plugin>\n      <name>{a}</name>\n      <prefix>{prefix}</prefix>\n      <artifactId>{a}</artifactId>\n    </plugin>\n")
            })
            .collect();
        generated.push((format!("{}/maven-metadata.xml", g.replace('.', "/")), format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata>\n  <plugins>\n{entries}  </plugins>\n</metadata>\n")));
    }
    let mut files = std::mem::take(&mut r.files);
    let mut report = r.report;
    for (path, text) in generated {
        let (d, _) = st.ingest_bytes(text.as_bytes())?;
        let (s1, _) = st.ingest_bytes(hex::encode(Sha1::digest(text.as_bytes())).as_bytes())?;
        files.insert(format!("{path}.sha1"), s1);
        files.insert(path, d);
    }
    report.files = files.keys().filter(|p| !p.ends_with(".sha1") && !p.ends_with(".sha256") && !p.contains("maven-metadata")).count();

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
    fn specs_paths_and_local_repositories() {
        assert_eq!(split_spec("com.google.guava:guava:33.3.1-jre"), Some(("com.google.guava".into(), "guava".into(), Some("33.3.1-jre".into()), "jar".into(), None)));
        assert_eq!(split_spec("org.x:y:pom:1.0").unwrap().3, "pom");
        assert_eq!(split_spec("org.x:y:jar:sources:1.0").unwrap().4.as_deref(), Some("sources"));
        assert_eq!(split_spec("org.x:y").unwrap().2, None);
        assert_eq!(split_spec("../x:y:1"), None);
        assert_eq!(split_spec("a:b:c:d:e:f"), None);
        assert_eq!(file_path("org.apache.maven.plugins", "maven-jar-plugin", "3.4.1", None, "pom"), "org/apache/maven/plugins/maven-jar-plugin/3.4.1/maven-jar-plugin-3.4.1.pom");
        assert_eq!(file_path("g", "a", "1", Some("tests"), "jar"), "g/a/1/a-1-tests.jar");
        assert_eq!(coords_of("org/slf4j/slf4j-api/2.0.16/slf4j-api-2.0.16.jar"), Some(("org.slf4j".into(), "slf4j-api".into(), "2.0.16".into())));
        assert_eq!(coords_of("org/x/a/1/other-1.jar"), None);
        for skip in ["_remote.repositories", "a-1.jar.sha1", "maven-metadata-central.xml", "a-1.pom.lastUpdated", "resolver-status.properties"] {
            assert!(!m2_file_wanted(skip), "{skip}");
        }
        assert!(m2_file_wanted("a-1.jar") && m2_file_wanted("a-1.pom"));
    }
}
