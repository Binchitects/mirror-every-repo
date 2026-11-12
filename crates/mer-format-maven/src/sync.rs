//! Collector-side sync of a Maven repository into one repo snapshot.
use crate::pom::{extension_of_packaging, file_of_type, interpolate_dep, merge, parse_pom, valid_coordinate, Dep, Gav, Model, RawPom};
use crate::version::{Range, Version};
use mer_core::{Digest, EntryRole};
use mer_fetch::{host_of, FetchError, HttpClient};
use mer_storage::{Storage, StorageError};
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const MAX_POM: u64 = 32 << 20;
const MAX_ARTIFACT: u64 = 1 << 30;
const MAX_NODES: usize = 100_000;
const MAX_PARENTS: usize = 40;

#[derive(Debug, thiserror::Error)]
pub enum MavenError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("upstream repository: {0}")]
    Repo(String),
    #[error("{0}")]
    Config(String),
    #[error("{what} is not in the upstream repository")]
    NotFound { what: String },
    #[error("{path}: upstream states SHA-1 {expected}, the file has {actual}")]
    Checksum { path: String, expected: String, actual: String },
}

impl From<mer_meta::MetaError> for MavenError {
    fn from(e: mer_meta::MetaError) -> Self {
        MavenError::Storage(e.into())
    }
}

#[derive(Clone, Debug, Default)]
pub struct MavenConfig {
    /// The repository, for example `https://repo.maven.apache.org/maven2`.
    pub repo_url: String,
    /// What to mirror: `group:artifact:version`, or `group:artifact` for
    /// the version a BOM manages, else the latest release. Plugins are
    /// artifacts too.
    pub artifacts: Vec<String>,
    /// BOMs (`group:artifact:version`) the projects inside the air gap
    /// import: the versions they manage replace the ones dependencies ask
    /// for, as they do in those projects.
    pub boms: Vec<String>,
    /// Extra files to mirror next to each jar where they exist, by
    /// classifier: `sources`, `javadoc`.
    pub classifiers: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MavenReport {
    /// Artifact files (jars and the like) mirrored.
    pub artifacts: usize,
    /// POM files mirrored, parents and BOMs included.
    pub poms: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub reused: usize,
    pub warnings: Vec<String>,
    pub snapshot: Option<u64>,
}

type Key = (String, String, String, String);
type Exclusions = BTreeSet<(String, String)>;

struct Node {
    /// Which set of managed versions is in force for this walk (see `sync`).
    context: usize,
    gav: Gav,
    kind: String,
    classifier: Option<String>,
    exclusions: Exclusions,
    root: bool,
}

struct Sync<'a> {
    st: &'a Storage,
    client: &'a HttpClient,
    base: String,
    /// Files of the newest snapshot: what is there is not fetched again.
    previous: BTreeMap<String, Digest>,
    entries: BTreeMap<String, Digest>,
    /// POMs read so far. None: upstream does not have it, or it does not parse.
    poms: BTreeMap<Gav, Option<RawPom>>,
    models: BTreeMap<Gav, Option<Model>>,
    /// Interpolated dependency management of a POM, imports included.
    managed: BTreeMap<Gav, Vec<Dep>>,
    report: MavenReport,
}

fn sha1_hex(body: &[u8]) -> String {
    hex::encode(Sha1::digest(body))
}

fn excluded(exclusions: &Exclusions, group: &str, artifact: &str) -> bool {
    exclusions.iter().any(|(g, a)| (g == "*" || g == group) && (a == "*" || a == artifact))
}

impl Sync<'_> {
    fn warn(&mut self, w: String) {
        if !self.report.warnings.contains(&w) {
            self.report.warnings.push(w);
        }
    }

    /// Bring one file into the snapshot, with the `.sha1` file Maven clients
    /// ask for next to it. `None`: upstream does not have it.
    fn file(&mut self, path: &str) -> Result<Option<Digest>, MavenError> {
        if let Some(d) = self.entries.get(path) {
            return Ok(Some(*d));
        }
        let sums = format!("{path}.sha1");
        if let (Some(d), Some(s)) = (self.previous.get(path).copied(), self.previous.get(&sums).copied()) {
            self.entries.insert(path.to_string(), d);
            self.entries.insert(sums, s);
            self.report.reused += 1;
            return Ok(Some(d));
        }
        let url = format!("{}/{path}", self.base);
        let limit = if path.ends_with(".pom") { MAX_POM } else { MAX_ARTIFACT };
        let body = match self.client.get(&url, &[]) {
            Ok(r) => r.bytes(&url, limit)?,
            Err(FetchError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let actual = sha1_hex(&body);
        // The checksum Maven itself would check the download against.
        let sums_url = format!("{url}.sha1");
        match self.client.get(&sums_url, &[]) {
            Ok(r) => {
                let text = String::from_utf8_lossy(&r.bytes(&sums_url, 4096)?).to_ascii_lowercase();
                // Some files hold "hash  filename", some a path and then the hash.
                let stated = text.split_whitespace().find(|w| w.len() == 40 && w.bytes().all(|b| b.is_ascii_hexdigit())).map(str::to_string);
                match stated {
                    Some(expected) if expected != actual => return Err(MavenError::Checksum { path: path.to_string(), expected, actual }),
                    Some(_) => {}
                    None => self.warn(format!("{path}: the upstream checksum file is not usable; kept as downloaded")),
                }
            }
            Err(FetchError::NotFound(_)) => self.warn(format!("{path}: upstream has no checksum for it; kept as downloaded")),
            Err(e) => return Err(e.into()),
        }
        let (d, size) = self.st.ingest_bytes(&body)?;
        let (s, _) = self.st.ingest_bytes(actual.as_bytes())?;
        self.report.downloaded += 1;
        self.report.downloaded_bytes += size;
        self.entries.insert(path.to_string(), d);
        self.entries.insert(sums, s);
        Ok(Some(d))
    }

    fn pom(&mut self, gav: &Gav) -> Result<Option<RawPom>, MavenError> {
        if let Some(p) = self.poms.get(gav) {
            return Ok(p.clone());
        }
        let parsed = match self.file(&gav.file(None, "pom"))? {
            Some(d) => match parse_pom(&String::from_utf8_lossy(&self.st.read_blob(&d)?)) {
                Ok(p) => Some(p),
                Err(e) => {
                    self.warn(format!("the POM of {gav} cannot be read ({e}); what it depends on is not followed"));
                    None
                }
            },
            None => None,
        };
        self.poms.insert(gav.clone(), parsed.clone());
        Ok(parsed)
    }

    /// The POM with everything inherited from its parents.
    fn model(&mut self, gav: &Gav) -> Result<Option<Model>, MavenError> {
        if let Some(m) = self.models.get(gav) {
            return Ok(m.clone());
        }
        let mut chain = Vec::new();
        let mut next = Some(gav.clone());
        while let Some(g) = next {
            let Some(pom) = self.pom(&g)? else {
                if chain.is_empty() {
                    self.models.insert(gav.clone(), None);
                    return Ok(None);
                }
                self.warn(format!("{g}, a parent of {gav}, is not available; what it would contribute is missing"));
                break;
            };
            next = pom.parent.clone().filter(|p| valid_coordinate(&p.group) && valid_coordinate(&p.artifact) && valid_coordinate(&p.version));
            chain.push(pom);
            if chain.len() > MAX_PARENTS {
                self.warn(format!("{gav} has more than {MAX_PARENTS} ancestors; the rest are ignored"));
                break;
            }
        }
        let model = merge(gav, &chain);
        self.models.insert(gav.clone(), Some(model.clone()));
        Ok(Some(model))
    }

    /// The dependency management of a POM: its own and inherited entries
    /// first, then what imported BOMs add, first import first.
    fn managed(&mut self, gav: &Gav, depth: usize) -> Result<Vec<Dep>, MavenError> {
        if let Some(m) = self.managed.get(gav) {
            return Ok(m.clone());
        }
        let Some(model) = self.model(gav)? else { return Ok(Vec::new()) };
        let mut out: Vec<Dep> = Vec::new();
        let mut imports = Vec::new();
        for d in &model.managed {
            let Some(d) = interpolate_dep(d, &model.properties) else { continue };
            if d.scope.as_deref() == Some("import") {
                imports.push(d);
            } else if !out.iter().any(|o| o.key() == d.key()) {
                out.push(d);
            }
        }
        // Stored before the imports are followed, so that a cycle of BOMs ends.
        self.managed.insert(gav.clone(), out.clone());
        for bom in imports {
            let Some(version) = bom.version.clone() else { continue };
            if depth > 20 || !valid_coordinate(&bom.group) || !valid_coordinate(&bom.artifact) || !valid_coordinate(&version) {
                continue;
            }
            for d in self.managed(&Gav::new(&bom.group, &bom.artifact, &version), depth + 1)? {
                if !out.iter().any(|o| o.key() == d.key()) {
                    out.push(d);
                }
            }
        }
        self.managed.insert(gav.clone(), out.clone());
        Ok(out)
    }

    /// The dependencies of a POM as Maven sees them: properties filled in,
    /// and version, scope and exclusions taken from dependency management
    /// where the dependency itself leaves them out.
    fn dependencies(&mut self, gav: &Gav) -> Result<Vec<Dep>, MavenError> {
        let Some(model) = self.model(gav)? else { return Ok(Vec::new()) };
        let managed = self.managed(gav, 0)?;
        let mut out = Vec::new();
        for raw in &model.deps {
            let Some(mut d) = interpolate_dep(raw, &model.properties) else {
                self.warn(format!("{gav}: dependency {}:{} uses a property that is not defined; skipped", raw.group, raw.artifact));
                continue;
            };
            if let Some(m) = managed.iter().find(|m| m.key() == d.key()) {
                d.version = d.version.or_else(|| m.version.clone());
                d.scope = d.scope.or_else(|| m.scope.clone());
                d.exclusions.extend(m.exclusions.iter().cloned());
            }
            out.push(d);
        }
        Ok(out)
    }

    /// Versions upstream lists for an artifact, and the one it calls the release.
    fn upstream_versions(&mut self, group: &str, artifact: &str) -> Result<(Vec<String>, Option<String>), MavenError> {
        let url = format!("{}/{}/{artifact}/maven-metadata.xml", self.base, group.replace('.', "/"));
        let body = match self.client.get(&url, &[]) {
            Ok(r) => r.bytes(&url, MAX_POM)?,
            Err(FetchError::NotFound(_)) => return Ok((Vec::new(), None)),
            Err(e) => return Err(e.into()),
        };
        let text = String::from_utf8_lossy(&body).to_string();
        let doc = roxmltree::Document::parse(&text).map_err(|e| MavenError::Repo(format!("{url}: {e}")))?;
        let all = |name: &str| -> Vec<String> { doc.descendants().filter(|n| n.is_element() && n.tag_name().name() == name).filter_map(|n| n.text()).map(|t| t.trim().to_string()).collect() };
        Ok((all("version"), all("release").into_iter().next().or_else(|| all("latest").into_iter().next())))
    }

    /// A version requirement as one version: a range becomes the highest
    /// version upstream has inside it.
    fn pick_version(&mut self, group: &str, artifact: &str, wanted: &str) -> Result<Option<String>, MavenError> {
        let Some(range) = Range::parse(wanted) else { return Ok(Some(wanted.to_string())) };
        let (versions, _) = self.upstream_versions(group, artifact)?;
        Ok(versions.into_iter().map(|v| Version::parse(&v)).filter(|v| range.contains(v)).max().map(|v| v.as_str().to_string()))
    }
}

fn parse_gav(spec: &str) -> Option<(String, String, Option<String>)> {
    let parts: Vec<&str> = spec.trim().split(':').map(str::trim).collect();
    match parts.as_slice() {
        [g, a] if valid_coordinate(g) && valid_coordinate(a) => Some((g.to_string(), a.to_string(), None)),
        [g, a, v] if valid_coordinate(g) && valid_coordinate(a) && valid_coordinate(v) => Some((g.to_string(), a.to_string(), Some(v.to_string()))),
        _ => None,
    }
}

/// Mirror the configured artifacts and what they depend on into a new snapshot.
///
/// What is trusted is the TLS connection to the repository and the SHA-1
/// checksum it publishes next to each file, which is what Maven checks too.
/// Signatures (`.asc`) are not verified.
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &MavenConfig) -> Result<MavenReport, MavenError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "maven" {
        return Err(MavenError::Config(format!("repo {repo} has format {}, not maven", repo_row.format)));
    }
    if host_of(&cfg.repo_url).is_none() {
        return Err(MavenError::Config(format!("{:?} is not an http(s) URL", cfg.repo_url)));
    }
    if cfg.artifacts.is_empty() {
        return Err(MavenError::Config("at least one artifact is needed".into()));
    }
    if let Some(bad) = cfg.classifiers.iter().find(|c| !valid_coordinate(c)) {
        return Err(MavenError::Config(format!("classifier {bad:?} is not acceptable")));
    }
    let previous = match st.head(repo)? {
        Some(h) => st.meta().read(|t| t.entries_at(repo_row.id, h.seq))?.into_iter().map(|e| (e.path, e.digest)).collect(),
        None => BTreeMap::new(),
    };
    let mut s = Sync {
        st,
        client,
        base: cfg.repo_url.trim_end_matches('/').to_string(),
        previous,
        entries: BTreeMap::new(),
        poms: BTreeMap::new(),
        models: BTreeMap::new(),
        managed: BTreeMap::new(),
        report: MavenReport::default(),
    };

    // Versions the BOMs manage: they win over what dependencies ask for.
    let mut bom_versions: BTreeMap<Key, String> = BTreeMap::new();
    for spec in &cfg.boms {
        let Some((g, a, Some(v))) = parse_gav(spec) else {
            return Err(MavenError::Config(format!("{spec:?}: a BOM is written group:artifact:version")));
        };
        let gav = Gav::new(&g, &a, &v);
        if s.pom(&gav)?.is_none() {
            return Err(MavenError::NotFound { what: format!("BOM {gav}") });
        }
        for d in s.managed(&gav, 0)? {
            if let Some(v) = d.version.clone() {
                bom_versions.entry(d.key()).or_insert(v);
            }
        }
    }

    // Whose dependency management decides the versions of transitive
    // dependencies depends on what is being resolved. An artifact used as a
    // dependency of a project: the project's (the BOMs given here, context
    // 0). An artifact resolved on its own, as every build plugin is: its
    // own. Both are walked, each with its own context, so either use works.
    let mut contexts: Vec<BTreeMap<Key, String>> = vec![bom_versions.clone()];
    let mut queue: VecDeque<Node> = VecDeque::new();
    for spec in &cfg.artifacts {
        let Some((g, a, v)) = parse_gav(spec) else {
            return Err(MavenError::Config(format!("{spec:?}: an artifact is written group:artifact or group:artifact:version")));
        };
        let version = match v {
            Some(v) => v,
            None => match bom_versions.get(&(g.clone(), a.clone(), "jar".into(), String::new())) {
                Some(v) => v.clone(),
                None => s.upstream_versions(&g, &a)?.1.filter(|v| valid_coordinate(v)).ok_or_else(|| MavenError::NotFound { what: format!("{g}:{a}") })?,
            },
        };
        let gav = Gav::new(&g, &a, &version);
        queue.push_back(Node { context: 0, gav: gav.clone(), kind: "jar".into(), classifier: None, exclusions: Exclusions::new(), root: true });
        let own: BTreeMap<Key, String> = s.managed(&gav, 0)?.into_iter().filter_map(|d| d.version.clone().map(|v| (d.key(), v))).collect();
        if !own.is_empty() {
            contexts.push(own);
            queue.push_back(Node { context: contexts.len() - 1, gav, kind: "jar".into(), classifier: None, exclusions: Exclusions::new(), root: true });
        }
    }

    // Per artifact: the exclusions it has been walked with. Walked again
    // only if it is reached with fewer, because then more may follow from it.
    let mut seen: BTreeMap<(usize, Gav, String, String), Exclusions> = BTreeMap::new();
    while let Some(node) = queue.pop_front() {
        let key = (node.context, node.gav.clone(), node.kind.clone(), node.classifier.clone().unwrap_or_default());
        let exclusions = match seen.get(&key) {
            Some(before) if before.is_subset(&node.exclusions) => continue,
            Some(before) => before.intersection(&node.exclusions).cloned().collect(),
            None => node.exclusions.clone(),
        };
        seen.insert(key, exclusions.clone());
        if seen.len() > MAX_NODES {
            return Err(MavenError::Repo(format!("more than {MAX_NODES} artifacts to mirror")));
        }
        let gav = &node.gav;
        let Some(model) = s.model(gav)? else {
            if s.entries.contains_key(&gav.file(None, "pom")) {
                // The POM is there but cannot be read (already reported).
                // Maven then uses the jar without dependencies; so can we.
                s.file(&gav.file(None, "jar"))?;
            } else if node.root {
                return Err(MavenError::NotFound { what: gav.to_string() });
            } else {
                s.warn(format!("{gav} is depended on but upstream does not have it; skipped"));
            }
            continue;
        };
        if let Some((g, a, v)) = &model.relocation {
            // The artifact has moved; Maven follows, so do we.
            let to = Gav::new(g.as_deref().unwrap_or(&gav.group), a.as_deref().unwrap_or(&gav.artifact), v.as_deref().unwrap_or(&gav.version));
            if valid_coordinate(&to.group) && valid_coordinate(&to.artifact) && valid_coordinate(&to.version) && to != *gav {
                queue.push_back(Node { context: node.context, gav: to, kind: node.kind.clone(), classifier: node.classifier.clone(), exclusions: exclusions.clone(), root: false });
            }
            continue;
        }

        // The artifact file itself.
        let wanted: Option<(String, Option<String>)> = if node.kind == "jar" && node.classifier.is_none() {
            extension_of_packaging(&model.packaging).map(|ext| (ext.to_string(), None))
        } else if node.kind == "pom" {
            None
        } else {
            Some(file_of_type(&node.kind, node.classifier.as_deref()))
        };
        if let Some((ext, classifier)) = wanted {
            if !valid_coordinate(&ext) || classifier.as_deref().is_some_and(|c| !valid_coordinate(c)) {
                s.warn(format!("{gav}: type {:?} is not something this mirror can name; skipped", node.kind));
            } else if s.file(&gav.file(classifier.as_deref(), &ext))?.is_some() {
                if ext == "jar" && classifier.is_none() {
                    for extra in &cfg.classifiers {
                        s.file(&gav.file(Some(extra), "jar"))?; // fine if absent
                    }
                }
            } else {
                s.warn(format!("{gav}: upstream has no {} for it", gav.file(classifier.as_deref(), &ext).rsplit('/').next().unwrap_or("file")));
            }
        }
        if model.gradle_metadata {
            s.file(&gav.file(None, "module"))?; // for Gradle clients; fine if absent
        }

        // What it brings with it: compile and runtime dependencies that are not optional.
        for d in s.dependencies(gav)? {
            let scope = d.scope.as_deref().unwrap_or("compile");
            if !matches!(scope, "compile" | "runtime") || d.optional.as_deref() == Some("true") {
                continue;
            }
            if excluded(&exclusions, &d.group, &d.artifact) {
                continue;
            }
            if !valid_coordinate(&d.group) || !valid_coordinate(&d.artifact) {
                continue;
            }
            let Some(wanted) = contexts[node.context].get(&d.key()).cloned().or_else(|| d.version.clone()) else {
                s.warn(format!("{gav}: dependency {}:{} has no version (a BOM the project imports may manage it: see --bom); skipped", d.group, d.artifact));
                continue;
            };
            let Some(version) = s.pick_version(&d.group, &d.artifact, &wanted)?.filter(|v| valid_coordinate(v)) else {
                s.warn(format!("{gav}: no version of {}:{} satisfies {wanted:?}; skipped", d.group, d.artifact));
                continue;
            };
            let mut below = exclusions.clone();
            below.extend(d.exclusions.iter().cloned());
            queue.push_back(Node {
                context: node.context,
                gav: Gav::new(&d.group, &d.artifact, &version),
                kind: d.kind.clone().unwrap_or_else(|| "jar".into()),
                classifier: d.classifier.clone(),
                exclusions: below,
                root: false,
            });
        }
    }

    // maven-metadata.xml for every artifact that has a POM here, so that
    // version ranges and "latest" resolve against what is mirrored.
    let mut versions: BTreeMap<(String, String), Vec<Version>> = BTreeMap::new();
    for gav in s.poms.iter().filter(|(g, p)| p.is_some() || s.entries.contains_key(&g.file(None, "pom"))).map(|(g, _)| g) {
        versions.entry((gav.group.clone(), gav.artifact.clone())).or_default().push(Version::parse(&gav.version));
    }
    for ((group, artifact), mut list) in versions {
        list.sort();
        let latest = list.last().expect("not empty");
        let release = list.iter().rev().find(|v| !v.is_prerelease()).unwrap_or(latest);
        let mut xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata>\n  <groupId>{group}</groupId>\n  <artifactId>{artifact}</artifactId>\n  <versioning>\n    <latest>{}</latest>\n    <release>{}</release>\n    <versions>\n",
            latest.as_str(),
            release.as_str()
        );
        for v in &list {
            xml.push_str(&format!("      <version>{}</version>\n", v.as_str()));
        }
        xml.push_str("    </versions>\n  </versioning>\n</metadata>\n");
        let path = format!("{}/{artifact}/maven-metadata.xml", group.replace('.', "/"));
        let (d, _) = st.ingest_bytes(xml.as_bytes())?;
        let (sum, _) = st.ingest_bytes(sha1_hex(xml.as_bytes()).as_bytes())?;
        s.entries.insert(format!("{path}.sha1"), sum);
        s.entries.insert(path, d);
    }
    s.report.poms = s.entries.keys().filter(|p| p.ends_with(".pom")).count();
    s.report.artifacts = s.entries.keys().filter(|p| !p.ends_with(".pom") && !p.ends_with(".sha1") && !p.ends_with(".xml") && !p.ends_with(".module")).count();

    let mut writer = st.begin(repo)?;
    for (path, digest) in &s.entries {
        writer.put(path, EntryRole::Artifact, digest)?;
    }
    for gone in s.previous.keys().filter(|p| !s.entries.contains_key(*p)) {
        writer.remove(gone)?;
    }
    s.report.snapshot = writer.seal(None)?.map(|x| x.seq);
    Ok(s.report)
}
