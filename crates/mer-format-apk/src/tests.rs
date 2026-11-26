//! Checks against apk itself, and sync tests against a small repository that
//! apk installs from (tests/fixture, made by tests/gen_fixture.py).
use crate::*;
use mer_fetch::{host_of, HttpClient};
use mer_storage::Storage;
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixture").join(rel)
}

fn read(rel: &str) -> Vec<u8> {
    std::fs::read(fixture(rel)).unwrap()
}

fn trusted() -> Keys {
    load_keys(&fixture("keys")).unwrap()
}

#[test]
fn version_order_matches_apk() {
    let t: Value = serde_json::from_slice(&std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/apk_truth.json")).unwrap()).unwrap();
    let pairs = t.as_array().unwrap();
    assert!(pairs.len() > 1000);
    for c in pairs {
        let (a, b, want) = (c[0].as_str().unwrap(), c[1].as_str().unwrap(), c[2].as_str().unwrap());
        let want = match want {
            "<" => Ordering::Less,
            "=" => Ordering::Equal,
            _ => Ordering::Greater,
        };
        assert_eq!(version::compare(a, b), want, "{a} vs {b}");
        assert_eq!(version::compare(b, a), want.reverse(), "{b} vs {a}");
    }
    for bad in ["", "abc", "1.0-", "1.0-r", "1.0_foo", "1..0", "1.0~", "1.0-r1-r2", "1.0 2"] {
        assert!(!version::valid(bad), "{bad:?}");
    }
    assert!(version::valid("1.2.3_rc1~abc123-r4"));
}

fn entries() -> Vec<Entry> {
    read_index(&read("main/x86_64/APKINDEX.tar.gz"), Some(&trusted())).unwrap().entries
}

fn entry(name: &str) -> Entry {
    entries().into_iter().find(|e| e.name == name).unwrap()
}

#[test]
fn indexes_are_checked_against_the_trusted_keys() {
    let raw = read("main/x86_64/APKINDEX.tar.gz");
    let index = read_index(&raw, Some(&trusted())).unwrap();
    assert_eq!(index.signed_by.as_deref(), Some("test@example.org-1.rsa.pub"));
    assert_eq!(index.entries.len(), 8);
    let hello = index.entries.iter().find(|e| e.name == "hello").unwrap();
    assert_eq!((hello.version.as_str(), hello.depends.as_slice()), ("1.0-r0", &["so:libhello.so.1".to_string(), "cmd:sh".into(), "!oldhello".into()][..]));
    assert_eq!(index.entries.iter().find(|e| e.name == "busybox").unwrap().priority, 100);

    // SHA-256 signatures (RSA256) too.
    assert_eq!(read_index(&read("APKINDEX-rsa256.tar.gz"), Some(&trusted())).unwrap().entries.len(), 2);

    // A key of the same name that did not sign it; no key of that name; a changed byte.
    let untrusted = load_keys(&fixture("untrusted")).unwrap();
    assert!(read_index(&raw, Some(&untrusted)).unwrap_err().contains("does not verify"));
    assert!(read_index(&raw, Some(&Keys::new())).unwrap_err().contains("not among the trusted keys"));
    let mut changed = raw.clone();
    let last = changed.len() - 30;
    changed[last] ^= 1;
    assert!(read_index(&changed, Some(&trusted())).is_err());
    let mut longer = raw.clone();
    longer.extend_from_slice(&raw);
    assert!(read_index(&longer, Some(&trusted())).is_err(), "nothing may follow the contents");
    // Without keys it is read, and says nobody vouched for it.
    assert_eq!(read_index(&raw, None).unwrap().signed_by, None);
    assert!(read_index(b"not gzip", None).is_err());
    assert!(rsa_key_from_pem("-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n").is_err());
}

#[test]
fn packages_must_match_the_index_in_size_control_and_contents() {
    for e in entries() {
        check_package(&read(&format!("main/x86_64/{}", e.file_name())), &e).unwrap_or_else(|why| panic!("{}: {why}", e.name));
    }
    let hello = entry("hello");
    let good = read("main/x86_64/hello-1.0-r0.apk");
    let flip = |at: usize| {
        let mut b = good.clone();
        b[at] ^= 0x40;
        check_package(&b, &hello)
    };
    assert!(flip(good.len() - 20).unwrap_err().contains("datahash"), "a changed byte in the contents");
    // The control member starts after the signature member; change a byte inside it.
    let sig_end = {
        let mut d = flate2::bufread::GzDecoder::new(&good[..]);
        std::io::copy(&mut d, &mut std::io::sink()).unwrap();
        good.len() - d.into_inner().len()
    };
    assert!(flip(sig_end + 12).is_err(), "a changed byte in the control section");
    assert!(check_package(&good[..good.len() - 1], &hello).unwrap_err().contains("bytes"));
    // Another package's file under this name.
    let lib = read("main/x86_64/libhello-1.0-r0.apk");
    assert!(check_package(&lib, &Entry { size: lib.len() as u64, ..hello.clone() }).is_err());
}

#[test]
fn needs_are_followed_with_priorities_and_install_if() {
    let pkgs: Vec<(usize, Entry)> = entries().into_iter().map(|e| (0, e)).collect();
    let pool = Pool::new(&pkgs);
    let names = |wanted: &[&str]| {
        let sel = resolve(&pool, &wanted.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        let mut n: Vec<String> = sel.selected.iter().map(|&i| pkgs[i].1.name.clone()).collect();
        n.sort();
        (n, sel.warnings)
    };
    // What apk itself installed from this repository (see gen_fixture.py).
    assert_eq!(names(&["hello"]).0, ["busybox", "hello", "libhello"], "busybox has the higher priority for cmd:sh; no docs, no completions");
    assert_eq!(names(&["hello", "bash-completion"]).0, ["bash-completion", "busybox", "hello", "hello-bash-completion", "libhello"], "install-if");
    assert_eq!(names(&["hello=1.0-r0"]).0, ["busybox", "hello", "libhello"], "a version in the name is ignored");
    let (n, warnings) = names(&["unwanted"]);
    assert_eq!(n, ["unwanted"]);
    assert!(warnings[0].contains("needs-nothing-here"), "{warnings:?}");
    assert!(resolve(&pool, &["nosuch".to_string()]).unwrap_err().contains("nosuch"));
}

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

struct Fake {
    base: String,
    routes: Routes,
    hits: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    /// Serves the fixture repository at /alpine/v3.24/main.
    fn new() -> Fake {
        let routes: Routes = Arc::default();
        for e in std::fs::read_dir(fixture("main/x86_64")).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            routes.lock().unwrap().insert(format!("/alpine/v3.24/main/x86_64/{name}"), std::fs::read(e.path()).unwrap());
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

    fn downloads(&self) -> usize {
        self.hits.lock().unwrap().iter().filter(|p| p.ends_with(".apk")).count()
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    st: Storage,
    up: Fake,
    cfg: ApkConfig,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("alpine", "apk").unwrap();
    let up = Fake::new();
    let cfg = ApkConfig { upstreams: vec![format!("{}/alpine/v3.24/main/", up.base)], packages: vec!["hello".into()], arches: vec!["x86_64".into()], keys: Some(fixture("keys")) };
    Rig { _dir: dir, st, up, cfg }
}

impl Rig {
    fn sync(&self) -> Result<ApkReport, ApkError> {
        sync(&self.st, "alpine", &HttpClient::new(&[host_of(&self.up.base).unwrap()]).without_retry_delay(), &self.cfg)
    }

    fn view(&self) -> BTreeMap<String, Vec<u8>> {
        let repo = self.st.repo("alpine").unwrap();
        let Some(head) = self.st.head("alpine").unwrap() else { return BTreeMap::new() };
        self.st.meta().read(|t| t.entries_at(repo.id, head.seq)).unwrap().into_iter().map(|e| (e.path, self.st.read_blob(&e.digest).unwrap())).collect()
    }
}

#[test]
fn a_sync_keeps_the_signed_index_and_the_packages_it_needs() {
    let r = rig();
    let rep = r.sync().unwrap();
    assert_eq!((rep.packages, rep.downloaded, rep.reused, rep.snapshot), (3, 3, 0, Some(1)), "{rep:?}");
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
    let view = r.view();
    assert_eq!(
        view.keys().cloned().collect::<Vec<_>>(),
        ["main/x86_64/APKINDEX.tar.gz", "main/x86_64/busybox-1.37.0-r1.apk", "main/x86_64/hello-1.0-r0.apk", "main/x86_64/libhello-1.0-r0.apk"]
    );
    assert_eq!(view["main/x86_64/APKINDEX.tar.gz"], read("main/x86_64/APKINDEX.tar.gz"), "the index is kept exactly as Alpine signed it");

    // Again: nothing downloaded, nothing new.
    let again = r.sync().unwrap();
    assert_eq!((again.downloaded, again.reused, again.snapshot), (0, 3, None));
    assert_eq!(r.up.downloads(), 3);

    // Fewer packages wanted: the others go.
    let mut r2 = r;
    r2.cfg.packages = vec!["libhello".into()];
    r2.sync().unwrap();
    assert_eq!(r2.view().keys().filter(|k| k.ends_with(".apk")).count(), 1);
}

#[test]
fn a_package_that_does_not_match_the_index_stops_the_sync() {
    let r = rig();
    let path = "/alpine/v3.24/main/x86_64/libhello-1.0-r0.apk";
    let mut bad = r.up.routes.lock().unwrap()[path].clone();
    let n = bad.len();
    bad[n - 20] ^= 1;
    r.up.routes.lock().unwrap().insert(path.into(), bad);
    let e = r.sync().unwrap_err().to_string();
    assert!(e.contains("libhello-1.0-r0.apk"), "{e}");
    assert!(r.st.head("alpine").unwrap().is_none(), "nothing was sealed");
}

#[test]
fn trust_settings_are_checked() {
    let mut r = rig();
    r.cfg.keys = Some(fixture("untrusted"));
    assert!(r.sync().unwrap_err().to_string().contains("not signed by a trusted key"));
    r.cfg.keys = None;
    assert!(r.sync().unwrap().warnings.iter().any(|w| w.contains("TLS alone")));
    r.cfg.upstreams = vec![format!("{}/alpine/v3.24/main", r.up.base), format!("{}/other/main", r.up.base)];
    assert!(r.sync().unwrap_err().to_string().contains("two upstreams"));
    r.cfg.upstreams = vec!["ftp://example.org/main".into()];
    assert!(r.sync().is_err());
    r.cfg.upstreams = vec![format!("{}/alpine/v3.24/main", r.up.base)];
    r.cfg.packages = vec!["nosuch".into()];
    assert!(r.sync().unwrap_err().to_string().contains("nosuch"));
}

/// What apk itself installs into an empty root, for real packages of
/// Alpine's main repository (tests/gen_oracle.sh), against what is mirrored.
#[test]
fn the_selection_matches_what_apk_installs_for_real_packages() {
    let mut text = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle/APKINDEX.gz")).unwrap()[..]), &mut text).unwrap();
    let (entries, skipped) = parse_index(&text);
    assert_eq!(skipped, 0);
    let pkgs: Vec<(usize, Entry)> = entries.into_iter().map(|e| (0, e)).collect();
    let pool = Pool::new(&pkgs);
    let want: BTreeMap<String, Vec<String>> = serde_json::from_slice(&std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/oracle/installs.json")).unwrap()).unwrap();
    assert!(want.len() > 30);
    let mut wrong = Vec::new();
    for (p, apk_installs) in &want {
        let sel = resolve(&pool, std::slice::from_ref(p)).unwrap();
        let mut got: Vec<String> = sel.selected.iter().map(|&i| pkgs[i].1.name.clone()).collect();
        got.sort();
        let missing: Vec<&String> = apk_installs.iter().filter(|n| !got.contains(n)).collect();
        let extra: Vec<&String> = got.iter().filter(|n| !apk_installs.contains(n)).collect();
        if !missing.is_empty() || !extra.is_empty() {
            wrong.push(format!("{p}: missing {missing:?}, extra {extra:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
