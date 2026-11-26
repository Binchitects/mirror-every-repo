//! Collector-side sync of a gem source (rubygems.org) into one repo snapshot.
use crate::info::{parse_info, parse_lockfile, suits_ruby, valid_name, Line};
use crate::version::{Requirement, Version};
use md5::Md5;
use mer_core::{Digest, EntryRole};
use mer_fetch::{FetchError, HttpClient};
use mer_storage::{Storage, StorageError};
use sha2::Digest as _;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::Read;
use std::path::PathBuf;

const MAX_INFO: u64 = 64 << 20;
const MAX_GEM: u64 = 2 << 30;
const MAX_GEMSPEC: u64 = 16 << 20;

#[derive(Debug, thiserror::Error)]
pub enum GemError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("upstream: {0}")]
    Upstream(String),
    #[error("{0}")]
    Config(String),
}

impl From<mer_meta::MetaError> for GemError {
    fn from(e: mer_meta::MetaError) -> Self {
        GemError::Storage(e.into())
    }
}

#[derive(Clone, Debug)]
pub struct GemConfig {
    /// The gem source, for example `https://rubygems.org`.
    pub upstream: String,
    /// What to mirror: `rails` (newest), `rails:~> 7.1`, `rack:= 3.1.8`.
    pub gems: Vec<String>,
    /// `Gemfile.lock` files whose exact gems are mirrored too.
    pub lockfiles: Vec<PathBuf>,
    /// The Ruby inside the air gap, for example `3.4.1`: gems that need
    /// another Ruby are left out. `None`: every Ruby.
    pub ruby: Option<String>,
    /// Platforms to keep besides `ruby` (pure Ruby), for example
    /// `x86_64-linux` and `x86_64-linux-gnu`.
    pub platforms: Vec<String>,
    /// Let gems named without a requirement resolve to a prerelease.
    pub pre: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GemReport {
    pub gems: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub reused: usize,
    pub warnings: Vec<String>,
    pub snapshot: Option<u64>,
}

fn up(msg: impl Into<String>) -> GemError {
    GemError::Upstream(msg.into())
}

/// `rails`, `rails:~> 7.1`, `rails@7.1.3` (exactly that version).
fn parse_spec(s: &str) -> Result<(String, Requirement, bool), String> {
    let (name, req) = match (s.split_once(':'), s.split_once('@')) {
        (Some((n, r)), _) => (n.trim(), r.trim().to_string()),
        (None, Some((n, v))) => (n.trim(), format!("= {}", v.trim())),
        (None, None) => (s.trim(), String::new()),
    };
    if !valid_name(name) {
        return Err(format!("{s:?} is not a gem name"));
    }
    let named = !req.is_empty();
    let r = if named { Requirement::parse(&req).ok_or_else(|| format!("{s:?}: {req:?} is not a requirement"))? } else { Requirement::any() };
    Ok((name.to_string(), r, named))
}

struct Fetcher<'a> {
    client: &'a HttpClient,
    base: String,
    cache: HashMap<String, Option<Vec<Line>>>,
    warnings: Vec<String>,
}

impl Fetcher<'_> {
    /// The info lines of a gem; `None` if the source does not have it.
    fn info(&mut self, name: &str) -> Result<Option<&Vec<Line>>, GemError> {
        if !self.cache.contains_key(name) {
            let url = format!("{}/info/{name}", self.base);
            let lines = match self.client.get_bytes(&url, &[], MAX_INFO) {
                Ok(body) => {
                    let text = String::from_utf8(body).map_err(|_| up(format!("{url}: not UTF-8")))?;
                    let (lines, bad) = parse_info(&text);
                    if bad > 0 {
                        self.warnings.push(format!("{name}: {bad} index lines could not be read and were left out"));
                    }
                    Some(lines)
                }
                Err(FetchError::NotFound(_) | FetchError::Http { status: 404 | 410, .. }) => None,
                Err(e) => return Err(e.into()),
            };
            self.cache.insert(name.to_string(), lines);
        }
        Ok(self.cache[name].as_ref())
    }
}

/// Mirror the configured gems, with what they depend on, into a new snapshot of `repo`.
///
/// Each requirement resolves to its newest matching version on its own (not
/// a solver), among the versions for the configured Ruby and platforms;
/// every platform build of that version is kept. Every `.gem` is checked
/// against the SHA-256 the compact index states. The vault's index lists
/// exactly what is mirrored, with upstream's lines unchanged.
pub fn sync(st: &Storage, repo: &str, client: &HttpClient, cfg: &GemConfig) -> Result<GemReport, GemError> {
    let repo_row = st.repo(repo)?;
    if repo_row.format != "gem" {
        return Err(GemError::Config(format!("repo {repo} has format {}, not gem", repo_row.format)));
    }
    if cfg.gems.is_empty() && cfg.lockfiles.is_empty() {
        return Err(GemError::Config("name a gem or a Gemfile.lock".into()));
    }
    let ruby = cfg.ruby.as_deref().map(|r| Version::parse(r).ok_or_else(|| GemError::Config(format!("{r:?} is not a Ruby version")))).transpose()?;
    let mut platforms: BTreeSet<String> = cfg.platforms.iter().cloned().collect();
    platforms.insert("ruby".into());
    for p in &platforms {
        if !valid_name(p) {
            return Err(GemError::Config(format!("{p:?} is not a platform")));
        }
    }
    let mut f = Fetcher { client, base: cfg.upstream.trim_end_matches('/').to_string(), cache: HashMap::new(), warnings: Vec::new() };

    // name -> chosen lines (by their full version)
    let mut chosen: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    // (gem, requirement, the requirement as written, who needs it, prereleases allowed)
    let mut queue: VecDeque<(String, Requirement, String, String, bool)> = VecDeque::new();
    for s in &cfg.gems {
        let (name, req, named) = parse_spec(s).map_err(GemError::Config)?;
        queue.push_back((name, req, s.clone(), "asked for".into(), named || cfg.pre));
    }
    // Exactly what each Gemfile.lock names.
    for path in &cfg.lockfiles {
        let text = std::fs::read_to_string(path).map_err(|e| GemError::Config(format!("{}: {e}", path.display())))?;
        let gems = parse_lockfile(&text);
        if gems.is_empty() {
            return Err(GemError::Config(format!("{} names no gems from a gem source", path.display())));
        }
        for (name, version, platform) in gems {
            let full = if platform == "ruby" { version.clone() } else { format!("{version}-{platform}") };
            let found = f.info(&name)?.is_some_and(|lines| lines.iter().any(|l| l.full_version() == full));
            if !found {
                return Err(up(format!("{}: {name} {full} is not in the gem source", path.display())));
            }
            chosen.entry(name).or_default().insert(full);
        }
    }

    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    while let Some((name, req, written, who, pre_ok)) = queue.pop_front() {
        if !seen.insert((name.clone(), written)) {
            continue;
        }
        let asked = who == "asked for";
        let Some(lines) = f.info(&name)? else {
            if asked {
                return Err(up(format!("{name} is not in the gem source")));
            }
            f.warnings.push(format!("{who} needs {name}, which the gem source does not have"));
            continue;
        };
        let pre = pre_ok || req.allows_prerelease();
        let best = lines
            .iter()
            .filter(|l| platforms.contains(&l.platform) && suits_ruby(l, ruby.as_ref()) && (pre || !l.version.is_prerelease()) && req.matches(&l.version))
            .map(|l| l.version.clone())
            .max();
        let Some(best) = best else {
            let msg = if asked {
                format!("no version of {name} matches what was asked, for this Ruby and these platforms")
            } else {
                format!("{who} needs {name}, but no version of it matches, for this Ruby and these platforms")
            };
            if asked {
                return Err(up(msg));
            }
            f.warnings.push(msg);
            continue;
        };
        let picked: Vec<Line> = lines.iter().filter(|l| l.version == best && platforms.contains(&l.platform) && suits_ruby(l, ruby.as_ref())).cloned().collect();
        for l in &picked {
            chosen.entry(name.clone()).or_default().insert(l.full_version());
            for (dep, dreq) in &l.deps {
                match Requirement::parse(dreq) {
                    Some(r) => queue.push_back((dep.clone(), r, dreq.clone(), format!("{name} {}", l.full_version()), false)),
                    None => f.warnings.push(format!("{name} {}: requirement {dreq:?} on {dep} is not understood; ignored", l.full_version())),
                }
            }
        }
    }

    // Download, check, and write the vault's own index of what is mirrored.
    let previous: BTreeMap<String, Digest> = match st.head(repo)? {
        Some(h) => st.meta().read(|t| t.entries_at(repo_row.id, h.seq))?.into_iter().map(|e| (e.path, e.digest)).collect(),
        None => BTreeMap::new(),
    };
    let mut report = GemReport::default();
    let mut files: BTreeMap<String, Digest> = BTreeMap::new();
    let mut versions = String::new();
    let mut newest_created = String::new();
    for (name, fulls) in &chosen {
        let lines = f.info(name)?.cloned().unwrap_or_default();
        // Upstream's order, which is the order versions were published.
        let kept: Vec<&Line> = lines.iter().filter(|l| fulls.contains(&l.full_version())).collect();
        let mut info = String::from("---\n");
        for l in &kept {
            let full = l.full_version();
            let file = format!("{name}-{full}");
            if !valid_name(&file) {
                return Err(up(format!("{file:?} is not a usable file name")));
            }
            let sum = l.checksum.ok_or_else(|| up(format!("{file}: the index states no checksum, so the gem cannot be checked")))?;
            let digest = Digest::from_hex(&hex::encode(sum)).map_err(|_| up("bad checksum"))?;
            if st.has_blob(&digest)? {
                report.reused += 1;
            } else {
                let url = format!("{}/gems/{file}.gem", f.base);
                let mut body = client.get(&url, &[])?.into_reader().take(MAX_GEM + 1);
                let (_, size) = st.ingest(&mut body, mer_blob::Expect { digest: Some(digest), size: None }).map_err(|e| match e {
                    StorageError::Store(mer_blob::StoreError::DigestMismatch { .. }) => up(format!("{url} does not match the checksum in the index")),
                    other => other.into(),
                })?;
                report.downloaded += 1;
                report.downloaded_bytes += size;
            }
            files.insert(format!("gems/{file}.gem"), digest);
            // `gem install` reads the specification from here. Nothing in the
            // index vouches for it; it is kept as the source serves it.
            let spec_path = format!("quick/Marshal.4.8/{file}.gemspec.rz");
            match previous.get(&spec_path) {
                Some(d) => {
                    files.insert(spec_path, *d);
                }
                None => {
                    let url = format!("{}/{spec_path}", f.base);
                    let body = client.get_bytes(&url, &[], MAX_GEMSPEC)?;
                    if body.first() != Some(&0x78) {
                        return Err(up(format!("{url} is not a compressed gem specification")));
                    }
                    let (d, _) = st.ingest_bytes(&body)?;
                    files.insert(spec_path, d);
                }
            }
            info.push_str(&l.raw);
            info.push('\n');
            if let Some(c) = &l.created_at {
                if *c > newest_created {
                    newest_created = c.clone();
                }
            }
            report.gems += 1;
        }
        if kept.is_empty() {
            continue;
        }
        let md5 = hex::encode(Md5::digest(info.as_bytes()));
        let list: Vec<String> = kept.iter().map(|l| l.full_version()).collect();
        versions.push_str(&format!("{name} {} {md5}\n", list.join(",")));
        let (d, _) = st.ingest_bytes(info.as_bytes())?;
        files.insert(format!("info/{name}"), d);
    }
    // Deterministic, so an unchanged selection makes no new snapshot.
    let created = if newest_created.is_empty() { "1970-01-01T00:00:00Z".to_string() } else { newest_created };
    let versions = format!("created_at: {created}\n---\n{versions}");
    let (d, _) = st.ingest_bytes(versions.as_bytes())?;
    files.insert("versions".into(), d);

    let mut writer = st.begin(repo)?;
    for (path, digest) in &files {
        writer.put(path, EntryRole::Artifact, digest)?;
    }
    for gone in previous.keys().filter(|p| !files.contains_key(*p)) {
        writer.remove(gone)?;
    }
    report.snapshot = writer.seal(None)?.map(|s| s.seq);
    let mut warnings = f.warnings;
    warnings.sort();
    warnings.dedup();
    report.warnings = warnings;
    Ok(report)
}
