//! Checks against Go's own implementations, and sync tests against a fake
//! module proxy and checksum database served over HTTP.
use crate::*;
use mer_fetch::{host_of, HttpClient};
use mer_storage::Storage;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn version_order_matches_x_mod_semver() {
    let t: Value = serde_json::from_slice(&fixture("go_truth.json")).unwrap();
    let mut n = 0;
    for c in t["compare"].as_array().unwrap() {
        let (a, b, want) = (c["A"].as_str().unwrap(), c["B"].as_str().unwrap(), c["C"].as_i64().unwrap());
        assert_eq!(compare(a, b) as i64, want, "compare({a:?}, {b:?})");
        n += 1;
    }
    for c in t["valid"].as_array().unwrap() {
        assert_eq!(is_valid(c["V"].as_str().unwrap()), c["Valid"].as_bool().unwrap(), "{}", c["V"]);
    }
    assert!(n > 1000);
}

#[test]
fn h1_hashes_match_what_the_go_command_records() {
    let t: Value = serde_json::from_slice(&fixture("go_truth.json")).unwrap();
    for m in t["modules"].as_array().unwrap() {
        let (path, ver, zip) = (m["Path"].as_str().unwrap(), m["Version"].as_str().unwrap(), m["Zip"].as_str().unwrap());
        let body = fixture(&format!("zips/{zip}"));
        assert_eq!(zip_hash(&body, &format!("{path}@{ver}")).unwrap(), m["Sum"].as_str().unwrap(), "{path}");
        let gomod = fixture(&format!("zips/{}", zip.replace(".zip", ".mod")));
        assert_eq!(gomod_hash(&gomod), m["GoModSum"].as_str().unwrap(), "{path} go.mod");
        // The prefix is part of what is hashed and checked.
        assert!(zip_hash(&body, "other.example/x@v1.0.0").is_err());
    }
}

/// A module zip with stored (uncompressed) entries.
fn make_zip(prefix: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
    let crc = |d: &[u8]| {
        let mut c: u32 = !0;
        for &b in d {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 };
            }
        }
        !c
    };
    let (mut out, mut central) = (Vec::new(), Vec::new());
    for (name, data) in files {
        let name = format!("{prefix}/{name}");
        let off = out.len() as u32;
        let (n, size, c) = (name.len() as u16, data.len() as u32, crc(data));
        out.extend([0x50, 0x4b, 3, 4, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend(c.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(n.to_le_bytes());
        out.extend([0, 0]);
        out.extend(name.as_bytes());
        out.extend(*data);
        central.extend([0x50, 0x4b, 1, 2, 20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        central.extend(c.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend(size.to_le_bytes());
        central.extend(n.to_le_bytes());
        central.extend([0; 12]);
        central.extend(off.to_le_bytes());
        central.extend(name.as_bytes());
    }
    let (cd_off, cd_len, count) = (out.len() as u32, central.len() as u32, files.len() as u16);
    out.extend(central);
    out.extend([0x50, 0x4b, 5, 6, 0, 0, 0, 0]);
    out.extend(count.to_le_bytes());
    out.extend(count.to_le_bytes());
    out.extend(cd_len.to_le_bytes());
    out.extend(cd_off.to_le_bytes());
    out.extend([0, 0]);
    out
}

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

struct Fake {
    base: String,
    routes: Routes,
    hits: Arc<Mutex<Vec<String>>>,
    versions: BTreeMap<String, Vec<String>>,
}

impl Fake {
    fn new() -> Fake {
        let routes: Routes = Arc::default();
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
        Fake { base, routes, hits, versions: BTreeMap::new() }
    }

    /// Publish `m@v` with a go.mod of `go <go>` and these requirements.
    fn publish(&mut self, m: &str, v: &str, go: &str, reqs: &[(&str, &str)]) -> &mut Self {
        let mut gomod = format!("module {m}\n\ngo {go}\n");
        for (rm, rv) in reqs {
            gomod.push_str(&format!("require {rm} {rv}\n"));
        }
        self.publish_raw(m, v, &gomod)
    }

    fn publish_raw(&mut self, m: &str, v: &str, gomod: &str) -> &mut Self {
        let zip = make_zip(&format!("{m}@{v}"), &[("go.mod", gomod.as_bytes()), ("lib.go", format!("package x // {m} {v}").as_bytes())]);
        let (em, ev) = (escape(m), escape(v));
        let mut r = self.routes.lock().unwrap();
        r.insert(format!("/proxy/{em}/@v/{ev}.mod"), gomod.as_bytes().to_vec());
        r.insert(format!("/proxy/{em}/@v/{ev}.info"), json!({ "Version": v, "Time": "2024-01-01T00:00:00Z" }).to_string().into_bytes());
        r.insert(
            format!("/sumdb/lookup/{em}@{ev}"),
            format!("1234\n{m} {v} {}\n{m} {v}/go.mod {}\n\ngo.sum database tree\n", zip_hash(&zip, &format!("{m}@{v}")).unwrap(), gomod_hash(gomod.as_bytes())).into_bytes(),
        );
        r.insert(format!("/proxy/{em}/@v/{ev}.zip"), zip);
        let list = self.versions.entry(m.to_string()).or_default();
        list.push(v.to_string());
        r.insert(format!("/proxy/{em}/@v/list"), list.iter().map(|v| format!("{v}\n")).collect::<String>().into_bytes());
        drop(r);
        self
    }

    fn zip_downloads(&self) -> usize {
        self.hits.lock().unwrap().iter().filter(|p| p.ends_with(".zip")).count()
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    st: Storage,
    up: Fake,
    cfg: GoConfig,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("go", "go").unwrap();
    let up = Fake::new();
    let cfg = GoConfig { proxy: format!("{}/proxy", up.base), sumdb: Some(format!("{}/sumdb", up.base)), modules: vec!["example.com/app@v1.0.0".into()], gomods: vec![], gosums: vec![] };
    Rig { _dir: dir, st, up, cfg }
}

impl Rig {
    fn sync(&self) -> Result<GoReport, GoError> {
        sync(&self.st, "go", &HttpClient::new(&[host_of(&self.up.base).unwrap()]), &self.cfg)
    }

    fn view(&self) -> BTreeMap<String, Vec<u8>> {
        let repo = self.st.repo("go").unwrap();
        let Some(head) = self.st.head("go").unwrap() else { return BTreeMap::new() };
        self.st.meta().read(|t| t.entries_at(repo.id, head.seq)).unwrap().into_iter().map(|e| (e.path, self.st.read_blob(&e.digest).unwrap())).collect()
    }

    fn with_ext(&self, ext: &str) -> Vec<String> {
        self.view().keys().filter_map(|p| p.strip_suffix(&format!(".{ext}"))).map(|p| p.replace("/@v/", " ")).collect()
    }
}

#[test]
fn a_root_gets_the_versions_minimal_version_selection_can_choose() {
    let mut r = rig();
    r.up
        .publish("example.com/app", "v1.0.0", "1.21", &[("example.com/lib", "v1.2.0"), ("example.com/old", "v1.0.0")])
        // An old-style module: its whole graph counts, and it wants a newer lib.
        .publish("example.com/old", "v1.0.0", "1.16", &[("example.com/lib", "v1.5.0")])
        .publish("example.com/lib", "v1.2.0", "1.21", &[("example.com/dep", "v0.1.0")])
        .publish("example.com/lib", "v1.5.0", "1.21", &[("example.com/dep", "v0.2.0")])
        .publish("example.com/lib", "v1.9.0", "1.21", &[])
        .publish("example.com/dep", "v0.1.0", "1.21", &[])
        .publish("example.com/dep", "v0.2.0", "1.21", &[]);
    let rep = r.sync().unwrap();
    // lib v1.5.0 is the full-graph choice; lib v1.2.0 is what app names. dep v0.1.0's go.mod
    // is in the graph but no build selects it; lib v1.9.0 is not in the graph at all.
    assert_eq!(r.with_ext("zip"), ["example.com/app v1.0.0", "example.com/dep v0.2.0", "example.com/lib v1.2.0", "example.com/lib v1.5.0", "example.com/old v1.0.0"]);
    assert_eq!(r.with_ext("mod"), ["example.com/app v1.0.0", "example.com/dep v0.1.0", "example.com/dep v0.2.0", "example.com/lib v1.2.0", "example.com/lib v1.5.0", "example.com/old v1.0.0"]);
    assert_eq!((rep.modules, rep.zips, rep.gomods, rep.unverified, rep.snapshot), (4, 5, 6, 0, Some(1)));
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
    let view = r.view();
    assert_eq!(view["example.com/lib/@v/list"], b"v1.2.0\nv1.5.0\n", "only versions with a zip are listed");
    assert_eq!(serde_json::from_slice::<Value>(&view["example.com/lib/@latest"]).unwrap()["Version"], "v1.5.0");

    // Nothing new: nothing downloaded again, no new snapshot.
    let again = r.sync().unwrap();
    assert_eq!((again.downloaded, again.snapshot), (0, None));
    assert_eq!(r.up.zip_downloads(), 5);
}

#[test]
fn a_module_named_without_a_version_is_its_newest_release_that_is_not_retracted() {
    let mut r = rig();
    r.up
        .publish("example.com/app", "v1.0.0", "1.21", &[])
        .publish("example.com/app", "v1.1.0", "1.21", &[])
        .publish("example.com/app", "v2.0.0-rc.1", "1.21", &[])
        .publish_raw("example.com/app", "v1.2.0", "module example.com/app\n\ngo 1.21\n\nretract v1.1.0\n");
    // v1.2.0 retracts v1.1.0 but not itself: it is the newest.
    r.cfg.modules = vec!["example.com/app".into()];
    r.sync().unwrap();
    assert_eq!(r.with_ext("zip"), ["example.com/app v1.2.0"]);
    r.up.publish_raw("example.com/app", "v1.3.0", "module example.com/app\n\ngo 1.21\n\nretract (\n\tv1.3.0 // oops\n\t[v1.2.0, v1.2.9]\n)\n");
    r.sync().unwrap();
    // Only the newest release's go.mod counts: it retracts itself and v1.2.x, and v1.1.0
    // (retracted only by v1.2.0) is acceptable again. The pre-release is never chosen.
    assert_eq!(r.with_ext("zip"), ["example.com/app v1.1.0"]);
}

#[test]
fn incompatible_versions_are_mirrored_as_go_names_them() {
    let mut r = rig();
    r.up
        .publish("example.com/app", "v1.0.0", "1.21", &[("example.com/legacy", "v2.1.0+incompatible")])
        .publish_raw("example.com/legacy", "v2.1.0+incompatible", "module example.com/legacy\n");
    r.sync().unwrap();
    assert_eq!(r.with_ext("zip"), ["example.com/app v1.0.0", "example.com/legacy v2.1.0+incompatible"]);
    assert_eq!(r.view()["example.com/legacy/@v/list"], b"v2.1.0+incompatible\n");
}

#[test]
fn upper_case_module_paths_are_escaped_as_the_proxy_protocol_wants() {
    let mut r = rig();
    r.up.publish("github.com/BurntSushi/toml", "v1.4.0", "1.18", &[]);
    r.cfg.modules = vec!["github.com/BurntSushi/toml@v1.4.0".into()];
    r.sync().unwrap();
    let view = r.view();
    assert!(view.contains_key("github.com/!burnt!sushi/toml/@v/v1.4.0.zip"));
    assert!(view.contains_key("github.com/!burnt!sushi/toml/@v/list"));
}

#[test]
fn a_go_mod_names_roots_and_a_go_sum_names_exactly_what_to_mirror() {
    let mut r = rig();
    r.up
        .publish("example.com/a", "v1.0.0", "1.21", &[("example.com/b", "v1.0.0")])
        .publish("example.com/b", "v1.0.0", "1.21", &[])
        .publish("example.com/c", "v1.0.0", "1.21", &[]);
    r.cfg.modules = vec![];
    r.cfg.gomods = vec!["module my.corp/app\n\ngo 1.22\n\nrequire example.com/a v1.0.0\nreplace example.com/a => ../a\n".into()];
    let rep = r.sync().unwrap();
    assert_eq!(r.with_ext("zip"), ["example.com/a v1.0.0", "example.com/b v1.0.0"]);
    assert!(rep.warnings.iter().any(|w| w.contains("replace")), "{:?}", rep.warnings);

    let zip = |m: &str| {
        let body = r.up.routes.lock().unwrap()[&format!("/proxy/{m}/@v/v1.0.0.zip")].clone();
        zip_hash(&body, &format!("{m}@v1.0.0")).unwrap()
    };
    let gm = |m: &str| gomod_hash(&r.up.routes.lock().unwrap()[&format!("/proxy/{m}/@v/v1.0.0.mod")]);
    let sum = format!("example.com/c v1.0.0 {}\nexample.com/c v1.0.0/go.mod {}\nexample.com/a v1.0.0/go.mod {}\n", zip("example.com/c"), gm("example.com/c"), gm("example.com/a"));
    r.cfg.gomods = vec![];
    r.cfg.gosums = vec![sum.clone()];
    r.cfg.sumdb = None;
    let rep = r.sync().unwrap();
    assert_eq!(r.with_ext("zip"), ["example.com/c v1.0.0"]);
    assert_eq!(r.with_ext("mod"), ["example.com/a v1.0.0", "example.com/c v1.0.0"]);
    assert_eq!(rep.unverified, 0, "the go.sum hashes were enough");

    r.cfg.gosums = vec![sum.replace(&zip("example.com/c"), "h1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")];
    let fresh = rig();
    let mut cfg = r.cfg.clone();
    cfg.proxy = r.cfg.proxy.clone();
    match sync(&fresh.st, "go", &HttpClient::new(&[host_of(&r.up.base).unwrap()]), &cfg) {
        Err(GoError::Checksum(m)) if m.source_ == "go.sum" => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn downloads_that_disagree_with_the_checksum_database_are_refused() {
    let mut r = rig();
    r.up.publish("example.com/app", "v1.0.0", "1.21", &[]);
    let tampered = make_zip("example.com/app@v1.0.0", &[("go.mod", b"module example.com/app\n"), ("evil.go", b"package x")]);
    r.up.routes.lock().unwrap().insert("/proxy/example.com/app/@v/v1.0.0.zip".into(), tampered);
    assert!(matches!(r.sync(), Err(GoError::Checksum(m)) if m.source_ == "checksum database"));
    // A zip with files outside module@version/ is refused before any hash is compared.
    let outside = make_zip("example.com/app@v1.0.0", &[("../go.mod", b"x")]);
    r.up.routes.lock().unwrap().insert("/proxy/example.com/app/@v/v1.0.0.zip".into(), outside);
    assert!(matches!(r.sync(), Err(GoError::Upstream(m)) if m.contains("outside")));
    // A version the checksum database does not know is refused.
    r.up.routes.lock().unwrap().remove("/sumdb/lookup/example.com/app@v1.0.0");
    assert!(matches!(r.sync(), Err(GoError::Upstream(m)) if m.contains("checksum database")));
    // Without a checksum database, it is mirrored and counted as unverified.
    r.up.publish("example.com/other", "v1.0.0", "1.21", &[]);
    r.cfg.sumdb = None;
    r.cfg.modules = vec!["example.com/other@v1.0.0".into()];
    let rep = r.sync().unwrap();
    assert_eq!(rep.unverified, 2);
    assert!(r.view().is_empty() || r.with_ext("zip") == ["example.com/other v1.0.0"]);
}

#[test]
fn missing_dependencies_warn_and_bad_config_is_refused() {
    let mut r = rig();
    r.up.publish("example.com/app", "v1.0.0", "1.21", &[("example.com/gone", "v1.0.0")]);
    let rep = r.sync().unwrap();
    assert_eq!(r.with_ext("zip"), ["example.com/app v1.0.0"]);
    assert_eq!(rep.warnings.len(), 1, "{:?}", rep.warnings);
    r.cfg.modules = vec!["example.com/nothere".into()];
    assert!(matches!(r.sync(), Err(GoError::Upstream(_))));
    r.cfg.modules = vec!["example.com/app@v9.9.9".into()];
    assert!(matches!(r.sync(), Err(GoError::Upstream(m)) if m.contains("does not have")), "a named version that does not exist");
    r.cfg.modules = vec![];
    r.cfg.sumdb = None;
    r.cfg.gosums = vec!["example.com/app v1.0.0 h1:x=\nexample.com/app v1.0.1 h1:y=\n".into()];
    assert!(matches!(r.sync(), Err(GoError::Upstream(m)) if m.contains("v1.0.1")), "a go.sum entry the proxy lacks");
    r.cfg.gosums = vec![];
    r.cfg.modules = vec!["not a module".into()];
    assert!(matches!(r.sync(), Err(GoError::Config(_))));
    r.st.create_repo("other", "npm").unwrap();
    r.cfg.modules = vec!["example.com/app@v1.0.0".into()];
    assert!(matches!(sync(&r.st, "other", &HttpClient::new(&[host_of(&r.up.base).unwrap()]), &r.cfg), Err(GoError::Config(_))));
}
