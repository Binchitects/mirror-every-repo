//! Checks against RubyGems itself.
use crate::version::{Requirement, Version};
use serde_json::Value;
use std::cmp::Ordering;

#[test]
fn versions_and_requirements_match_rubygems() {
    let t: Value = serde_json::from_slice(&std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/gem_truth.json")).unwrap()).unwrap();
    let mut wrong = Vec::new();
    for c in t["order"].as_array().unwrap() {
        let (a, b) = (c[0].as_str().unwrap(), c[1].as_str().unwrap());
        let want = match c[2].as_i64().unwrap() {
            -1 => Ordering::Less,
            0 => Ordering::Equal,
            _ => Ordering::Greater,
        };
        let got = Version::parse(a).unwrap().cmp(&Version::parse(b).unwrap());
        if got != want {
            wrong.push(format!("{a} vs {b}: {got:?}, RubyGems says {want:?}"));
        }
    }
    for c in t["matches"].as_array().unwrap() {
        let (r, v, want) = (c[0].as_str().unwrap(), c[1].as_str().unwrap(), c[2].as_bool().unwrap());
        let got = Requirement::parse(r).unwrap_or_else(|| panic!("{r}")).matches(&Version::parse(v).unwrap());
        if got != want {
            wrong.push(format!("{v:?} in {r:?}: {got}, RubyGems says {want}"));
        }
    }
    assert!(wrong.is_empty(), "{} wrong:\n{}", wrong.len(), wrong[..wrong.len().min(30)].join("\n"));
    assert!(Version::parse("1.0 x").is_none() && Version::parse("a.1").is_none() && Version::parse("1..2").is_none());
}

use crate::*;
use md5::Md5;
use mer_fetch::{host_of, HttpClient};
use mer_storage::Storage;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// A fake gem source: gems are (name, version-platform, deps, meta) and get
/// a body of their own, whose SHA-256 goes into the info line.
struct Fake {
    base: String,
    routes: Routes,
    hits: Arc<Mutex<Vec<String>>>,
}

fn body(file: &str) -> Vec<u8> {
    format!("gem {file}").repeat(30).into_bytes()
}

impl Fake {
    fn new(gems: &[(&str, &str, &str, &str)]) -> Fake {
        let routes: Routes = Arc::default();
        let mut infos: BTreeMap<String, String> = BTreeMap::new();
        for (name, full, deps, meta) in gems {
            let file = format!("{name}-{full}");
            let sum = hex::encode(Sha256::digest(body(&file)));
            let meta = if meta.is_empty() { format!("checksum:{sum}") } else { format!("checksum:{sum},{meta}") };
            let info = infos.entry(name.to_string()).or_insert_with(|| "---\n".into());
            info.push_str(&format!("{full} {deps}|{meta}\n"));
            let mut r = routes.lock().unwrap();
            r.insert(format!("/gems/{file}.gem"), body(&file));
            r.insert(format!("/quick/Marshal.4.8/{file}.gemspec.rz"), [&[0x78u8, 0x9c][..], file.as_bytes()].concat());
        }
        for (name, info) in infos {
            routes.lock().unwrap().insert(format!("/info/{name}"), info.into_bytes());
        }
        let hits: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let (r, h) = (routes.clone(), hits.clone());
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let path = req.url().to_string();
                h.lock().unwrap().push(path.clone());
                let body = r.lock().unwrap().get(&path).cloned();
                let _ = match body {
                    Some(b) => req.respond(tiny_http::Response::from_data(b)),
                    None => req.respond(tiny_http::Response::from_string("not found").with_status_code(404)),
                };
            }
        });
        Fake { base, routes, hits }
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    st: Storage,
    up: Fake,
    cfg: GemConfig,
}

fn rig(gems: &[(&str, &str, &str, &str)]) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("gems", "gem").unwrap();
    let up = Fake::new(gems);
    let cfg = GemConfig { upstream: up.base.clone(), gems: vec!["app".into()], lockfiles: vec![], ruby: Some("3.4.1".into()), platforms: vec!["x86_64-linux".into()], pre: false };
    Rig { _dir: dir, st, up, cfg }
}

impl Rig {
    fn sync(&self) -> Result<GemReport, GemError> {
        sync(&self.st, "gems", &HttpClient::new(&[host_of(&self.up.base).unwrap()]).without_retry_delay(), &self.cfg)
    }

    fn view(&self) -> BTreeMap<String, Vec<u8>> {
        let repo = self.st.repo("gems").unwrap();
        let Some(head) = self.st.head("gems").unwrap() else { return BTreeMap::new() };
        self.st.meta().read(|t| t.entries_at(repo.id, head.seq)).unwrap().into_iter().map(|e| (e.path, self.st.read_blob(&e.digest).unwrap())).collect()
    }

    fn gems(&self) -> Vec<String> {
        let mut g: Vec<String> = self.view().keys().filter_map(|k| k.strip_prefix("gems/")).map(|k| k.trim_end_matches(".gem").to_string()).collect();
        g.sort();
        g
    }

    fn text(&self, path: &str) -> String {
        String::from_utf8(self.view()[path].clone()).unwrap()
    }
}

const SOURCE: &[(&str, &str, &str, &str)] = &[
    ("app", "1.0.0", "lib:~> 1.0,native:>= 0", "ruby:>= 3.0"),
    ("lib", "1.0.0", "", ""),
    ("lib", "1.2.0", "", ""),
    ("lib", "2.0.0", "", ""),
    ("lib", "1.3.0.rc1", "", ""),
    // The newest needs a Ruby the air gap does not have.
    ("lib", "1.9.0", "", "ruby:>= 9.0"),
    ("native", "1.0.0", "", ""),
    ("native", "1.0.0-x86_64-linux", "", ""),
    ("native", "1.0.0-arm64-darwin", "", ""),
];

#[test]
fn requirements_resolve_for_this_ruby_and_platform_and_the_index_lists_what_is_mirrored() {
    let r = rig(SOURCE);
    let rep = r.sync().unwrap();
    assert_eq!((rep.gems, rep.downloaded, rep.snapshot), (4, 4, Some(1)), "{rep:?}");
    assert_eq!(r.gems(), ["app-1.0.0", "lib-1.2.0", "native-1.0.0", "native-1.0.0-x86_64-linux"], "~> 1.0 within this Ruby, no prerelease; the ruby build and this platform's");
    // Upstream's lines, unchanged and in their order; /versions names them with the MD5 of each file.
    let info = r.text("info/native");
    assert!(info.starts_with("---\n1.0.0 |checksum:") && info.lines().nth(2).unwrap().starts_with("1.0.0-x86_64-linux |checksum:"), "{info}");
    let versions = r.text("versions");
    assert!(versions.starts_with("created_at: 1970-01-01T00:00:00Z\n---\n"), "{versions}");
    assert!(versions.contains(&format!("native 1.0.0,1.0.0-x86_64-linux {}\n", hex::encode(Md5::digest(info.as_bytes())))), "{versions}");
    assert!(r.view().contains_key("quick/Marshal.4.8/native-1.0.0-x86_64-linux.gemspec.rz"));

    // Again: nothing new.
    let again = r.sync().unwrap();
    assert_eq!((again.downloaded, again.reused, again.snapshot), (0, 4, None));
}

#[test]
fn prereleases_and_exact_versions_are_mirrored_when_asked_for() {
    let mut r = rig(SOURCE);
    r.cfg.gems = vec!["lib:~> 1.3.0.rc1".into(), "lib@2.0.0".into()];
    r.sync().unwrap();
    assert_eq!(r.gems(), ["lib-1.3.0.rc1", "lib-2.0.0"]);
    r.cfg.gems = vec!["lib".into()];
    r.cfg.ruby = None;
    r.sync().unwrap();
    assert_eq!(r.gems(), ["lib-2.0.0"], "the newest, prereleases aside");
}

#[test]
fn a_gemfile_lock_is_mirrored_exactly() {
    let mut r = rig(SOURCE);
    let dir = tempfile::tempdir().unwrap();
    let lock = dir.path().join("Gemfile.lock");
    std::fs::write(&lock, "GEM\n  remote: https://rubygems.org/\n  specs:\n    lib (1.0.0)\n    native (1.0.0-arm64-darwin)\n      lib (~> 1.0)\n\nPLATFORMS\n  arm64-darwin\n\nDEPENDENCIES\n  native\n\nBUNDLED WITH\n   2.6.9\n").unwrap();
    r.cfg.gems = vec![];
    r.cfg.lockfiles = vec![lock.clone()];
    r.sync().unwrap();
    assert_eq!(r.gems(), ["lib-1.0.0", "native-1.0.0-arm64-darwin"], "exactly the lock, whatever the platforms setting");
    std::fs::write(&lock, "GEM\n  specs:\n    lib (7.7.7)\n").unwrap();
    assert!(r.sync().unwrap_err().to_string().contains("lib 7.7.7"));
}

#[test]
fn checksums_missing_gems_and_bad_settings() {
    let r = rig(SOURCE);
    r.up.routes.lock().unwrap().insert("/gems/lib-1.2.0.gem".into(), b"something else".to_vec());
    assert!(r.sync().unwrap_err().to_string().contains("does not match the checksum"));
    assert!(r.st.head("gems").unwrap().is_none());

    let mut r = rig(&[("app", "1.0.0", "gone:>= 1", "")]);
    let rep = r.sync().unwrap();
    assert!(rep.warnings.iter().any(|w| w.contains("needs gone")), "{:?}", rep.warnings);
    r.cfg.gems = vec!["nosuch".into()];
    assert!(r.sync().unwrap_err().to_string().contains("nosuch"));
    r.cfg.gems = vec!["../x".into()];
    assert!(r.sync().is_err());
    r.cfg.gems = vec!["app".into()];
    r.cfg.ruby = Some("three".into());
    assert!(r.sync().is_err());
    assert!(r.up.hits.lock().unwrap().iter().all(|p| !p.contains("specs.4.8")), "the Marshal indexes are never needed");
}

#[test]
fn info_lines_and_lockfiles_are_read() {
    let (lines, bad) = parse_info("---\n1.19.7-x86_64-linux-gnu racc:~> 1.4|checksum:25865ccb462f714e712fdf8906fde07849cb6704a808e3d629d75baa5af5dd3b,ruby:< 4.1.dev&>= 3.2,rubygems:>= 3.3.22,created_at:2026-11-02T18:22:52Z\n1.3.1 |checksum:72355dfd8169956870b2c581111e638059e2c62251c6ffb8d40f7e3f36a246b9\nbroken line|\n");
    assert_eq!(bad, 1);
    assert_eq!((lines[0].version.as_str(), lines[0].platform.as_str(), lines[0].full_version()), ("1.19.7", "x86_64-linux-gnu", "1.19.7-x86_64-linux-gnu".to_string()));
    assert_eq!(lines[0].deps, [("racc".to_string(), "~> 1.4".to_string())]);
    assert_eq!(lines[0].ruby.as_deref(), Some("< 4.1.dev&>= 3.2"));
    assert_eq!((lines[1].platform.as_str(), lines[1].deps.len()), ("ruby", 0));
    let lock = parse_lockfile("GIT\n  remote: x\n  specs:\n    mine (1.0)\n\nGEM\n  remote: https://rubygems.org/\n  specs:\n    nokogiri (1.19.7-x86_64-linux-gnu)\n      racc (~> 1.4)\n    racc (1.8.1)\n\nPLATFORMS\n  x86_64-linux\n");
    assert_eq!(lock, [("nokogiri".to_string(), "1.19.7".to_string(), "x86_64-linux-gnu".to_string()), ("racc".into(), "1.8.1".into(), "ruby".into())], "gems from git are not from a gem source");
}
