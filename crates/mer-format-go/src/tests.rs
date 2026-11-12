//! Checksum math against real files, and sync against a fake proxy.
use crate::*;
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

// ------------------------------------------------- against the real thing

/// Files as proxy.golang.org serves them, with the checksums sum.golang.org
/// publishes for them (see tests/fixtures).
#[test]
fn checksums_equal_those_of_the_go_checksum_database() {
    let gomod = include_bytes!("../tests/fixtures/gin-v1.10.0.mod");
    assert_eq!(h1_gomod(gomod), "h1:4PMNQiOhvDRa013RKVbsiNwoyezlm2rm0uX/T7kzp5Y=");
    let zip = include_bytes!("../tests/fixtures/sse-v0.1.0.zip");
    assert_eq!(h1_zip(zip).unwrap(), "h1:Y/yl/+YNO8GZSjAhjMsSuLt29uWRFHdHYUb5lYOV9qE=");
    assert!(h1_zip(b"not a zip").is_err());
}

#[test]
fn a_real_go_mod_is_read() {
    let m = parse_gomod(include_str!("../tests/fixtures/gin-v1.10.0.mod"));
    assert_eq!((m.module.as_str(), m.go), ("github.com/gin-gonic/gin", Some((1, 20))));
    assert_eq!(m.requires.len(), 29);
    assert!(m.requires.contains(&("github.com/modern-go/concurrent".into(), "v0.0.0-20180306012644-bacd9c7ef1dd".into())));
    assert!(m.lists_all_dependencies());
}

#[test]
fn go_mod_syntax() {
    let m = parse_gomod(
        "module \"example.com/app\" // the app\n\ngo 1.16\ntoolchain go1.22.0\nrequire example.com/a v1.2.3\nrequire (\n\texample.com/b v0.0.0-20200101000000-abcdefabcdef // indirect\n\t\"example.com/c/v2\" v2.0.0+incompatible\n)\nreplace example.com/a => ../a\nexclude (\n\texample.com/x v1.0.0\n)\nretract v1.0.0\n",
    );
    assert_eq!(m.module, "example.com/app");
    assert!(!m.lists_all_dependencies());
    assert_eq!(m.requires.len(), 3);
    assert_eq!(m.requires[2], ("example.com/c/v2".to_string(), "v2.0.0+incompatible".to_string()));
    assert!(!parse_gomod("module x\n").lists_all_dependencies(), "no go directive means an old module");
}

#[test]
fn names_and_versions() {
    assert_eq!(escape("github.com/Azure/azure-sdk"), "github.com/!azure/azure-sdk");
    assert_eq!(escape("v1.0.0-RC1"), "v1.0.0-!r!c1");
    for ok in ["golang.org/x/net", "gopkg.in/yaml.v3", "github.com/a/b/v2"] {
        assert!(valid_module(ok), "{ok}");
    }
    for bad in ["", "a//b", "../x", "a/.hidden", "a b", "a/b/"] {
        assert!(!valid_module(bad), "{bad}");
    }
    assert!(valid_version("v1.2.3") && valid_version("v2.0.0+incompatible") && valid_version("v0.0.0-20180306012644-bacd9c7ef1dd"));
    assert!(!valid_version("1.2.3") && !valid_version("v1.2") && !valid_version("latest"));
    let nodes: BTreeSet<ModVer> =
        [("a", "v1.2.0"), ("a", "v1.10.0"), ("a", "v1.10.0-rc.1"), ("b", "v0.0.0-20200101000000-abcdefabcdef"), ("b", "v0.0.0-20190101000000-ffffffffffff"), ("c", "v2.0.0+incompatible")]
            .iter()
            .map(|(m, v)| (m.to_string(), v.to_string()))
            .collect();
    let picked: Vec<String> = select(&nodes).into_iter().map(|(m, v)| format!("{m}@{v}")).collect();
    assert_eq!(picked, ["a@v1.10.0", "b@v0.0.0-20200101000000-abcdefabcdef", "c@v2.0.0+incompatible"]);
}

// ------------------------------------------------------ a fake proxy

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

struct Proxy {
    base: String,
    routes: Routes,
    hits: Arc<Mutex<Vec<String>>>,
}

fn archive(module: &str, version: &str) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in [("go.mod", format!("module {module}\n")), ("lib.go", format!("package lib // {version}\n").repeat(20))] {
        w.start_file(format!("{module}@{version}/{name}"), opts).unwrap();
        w.write_all(body.as_bytes()).unwrap();
    }
    w.finish().unwrap().into_inner()
}

impl Proxy {
    fn new() -> Proxy {
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
                    None => req.respond(tiny_http::Response::from_string("not found").with_status_code(if path.contains("gone") { 410 } else { 404 })),
                };
            }
        });
        Proxy { base, routes, hits }
    }

    /// Publish a module version: on the proxy, and in the checksum database.
    fn publish(&self, module: &str, version: &str, go: &str, requires: &[(&str, &str)]) -> &Self {
        let mut gomod = format!("module {module}\n\n");
        if !go.is_empty() {
            gomod.push_str(&format!("go {go}\n\n"));
        }
        for (m, v) in requires {
            gomod.push_str(&format!("require {m} {v}\n"));
        }
        let zip = archive(module, version);
        let (em, ev) = (escape(module), escape(version));
        let mut routes = self.routes.lock().unwrap();
        routes.insert(format!("/sumdb/lookup/{em}@{ev}"), format!("7\n{module} {version} {}\n{module} {version}/go.mod {}\n\ngo.sum database tree\n", h1_zip(&zip).unwrap(), h1_gomod(gomod.as_bytes())).into_bytes());
        routes.insert(format!("/proxy/{em}/@v/{ev}.mod"), gomod.into_bytes());
        routes.insert(format!("/proxy/{em}/@v/{ev}.info"), format!(r#"{{"Version":"{version}","Time":"2026-01-01T00:00:00Z"}}"#).into_bytes());
        routes.insert(format!("/proxy/{em}/@v/{ev}.zip"), zip);
        routes.insert(format!("/proxy/{em}/@latest"), format!(r#"{{"Version":"{version}","Time":"2026-01-01T00:00:00Z"}}"#).into_bytes());
        self
    }

    fn fetched(&self, suffix: &str) -> Vec<String> {
        let mut v: Vec<String> = self.hits.lock().unwrap().iter().filter(|p| p.starts_with("/proxy/") && p.ends_with(suffix)).map(|p| p["/proxy/".len()..].to_string()).collect();
        v.sort();
        v.dedup();
        v
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    st: Storage,
    px: Proxy,
    cfg: GoConfig,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("go", "go").unwrap();
    let px = Proxy::new();
    let cfg = GoConfig { proxy_url: format!("{}/proxy/", px.base), modules: vec!["ex.com/app".into()], gomods: Vec::new(), sumdb_url: Some(format!("{}/sumdb", px.base)) };
    Rig { _dir: dir, st, px, cfg }
}

impl Rig {
    fn sync(&self) -> Result<GoReport, GoError> {
        sync(&self.st, "go", &HttpClient::new(&[host_of(&self.px.base).unwrap()]), &self.cfg)
    }

    fn view(&self) -> BTreeMap<String, Vec<u8>> {
        let repo = self.st.repo("go").unwrap();
        let Some(head) = self.st.head("go").unwrap() else { return BTreeMap::new() };
        self.st.meta().read(|t| t.entries_at(repo.id, head.seq)).unwrap().into_iter().map(|e| (e.path, self.st.read_blob(&e.digest).unwrap())).collect()
    }

    /// Mirrored files with the given extension, as "module@version".
    fn have(&self, ext: &str) -> Vec<String> {
        self.view().keys().filter_map(|p| p.strip_suffix(ext)).filter_map(|p| p.split_once("/@v/")).map(|(m, v)| format!("{m}@{v}")).collect()
    }
}

#[test]
fn a_module_is_mirrored_with_the_graph_the_go_command_would_load() {
    let r = rig();
    r.px
        .publish("ex.com/app", "v1.0.0", "1.21", &[("ex.com/lib", "v1.2.0"), ("ex.com/util", "v0.5.0")])
        .publish("ex.com/app", "v1.1.0", "1.21", &[("ex.com/lib", "v1.2.0"), ("ex.com/util", "v0.5.0")])
        // lib is a modern module: what it requires is in the graph, but the
        // go command does not read on from there.
        .publish("ex.com/lib", "v1.2.0", "1.20", &[("ex.com/util", "v0.4.0"), ("ex.com/testdep", "v1.0.0")])
        .publish("ex.com/lib", "v1.3.0", "1.20", &[])
        .publish("ex.com/util", "v0.4.0", "1.20", &[("ex.com/deep", "v9.0.0")])
        .publish("ex.com/util", "v0.5.0", "1.20", &[])
        .publish("ex.com/testdep", "v1.0.0", "1.20", &[("ex.com/deeper", "v1.0.0")])
        .publish("ex.com/deep", "v9.0.0", "1.20", &[])
        .publish("ex.com/deeper", "v1.0.0", "1.20", &[]);
    let rep = r.sync().unwrap();
    // Latest app; lib as app names it (not the newer one); the higher of the two util versions.
    assert_eq!(r.have(".zip"), ["ex.com/app@v1.1.0", "ex.com/lib@v1.2.0", "ex.com/testdep@v1.0.0", "ex.com/util@v0.5.0"]);
    assert_eq!(r.have(".mod"), r.have(".zip"), "util v0.4.0 is only a version number in the graph: its go.mod is never read");
    assert_eq!(r.have(".info"), r.have(".zip"));
    assert_eq!((rep.modules, rep.archives, rep.gomods, rep.snapshot), (4, 4, 4, Some(1)));
    assert_eq!(rep.verified, 8, "every go.mod and every archive was compared with the checksum database");
    assert_eq!(r.px.fetched(".zip").len(), 4);

    let v = r.view();
    assert_eq!(v["ex.com/app/@v/list"], b"v1.1.0\n");
    assert_eq!(v["ex.com/app/@latest"], v["ex.com/app/@v/v1.1.0.info"]);
    assert!(!v.contains_key("ex.com/deep/@v/list"));
}

#[test]
fn below_a_module_from_before_go_1_17_the_whole_graph_is_read() {
    let r = rig();
    r.px
        .publish("ex.com/app", "v1.0.0", "1.21", &[("ex.com/old", "v1.0.0")])
        // No go directive: its go.mod does not list everything it needs.
        .publish("ex.com/old", "v1.0.0", "", &[("ex.com/mid", "v1.0.0")])
        .publish("ex.com/mid", "v1.0.0", "1.21", &[("ex.com/leaf", "v1.0.0"), ("ex.com/old", "v0.9.0")])
        .publish("ex.com/old", "v0.9.0", "1.15", &[])
        .publish("ex.com/leaf", "v1.0.0", "1.21", &[]);
    r.sync().unwrap();
    assert_eq!(r.have(".zip"), ["ex.com/app@v1.0.0", "ex.com/leaf@v1.0.0", "ex.com/mid@v1.0.0", "ex.com/old@v1.0.0"]);
    assert_eq!(r.have(".mod"), ["ex.com/app@v1.0.0", "ex.com/leaf@v1.0.0", "ex.com/mid@v1.0.0", "ex.com/old@v0.9.0", "ex.com/old@v1.0.0"], "old v0.9.0 is read for the graph but not selected");
}

#[test]
fn a_project_go_mod_names_exact_versions_and_several_roots_are_also_served_together() {
    let mut r = rig();
    r.px
        .publish("ex.com/app", "v1.0.0", "1.21", &[("ex.com/shared", "v1.0.0")])
        .publish("ex.com/tool", "v2.0.0", "1.21", &[("ex.com/shared", "v1.5.0")])
        .publish("ex.com/shared", "v1.0.0", "1.21", &[])
        .publish("ex.com/shared", "v1.5.0", "1.21", &[])
        .publish("ex.com/Upper/Case", "v1.0.0", "1.21", &[]);
    // Each alone, and both in one project: shared is needed at both versions.
    r.cfg.modules = vec!["ex.com/app@v1.0.0".into(), "ex.com/tool@v2.0.0".into()];
    r.sync().unwrap();
    assert_eq!(r.have(".zip"), ["ex.com/app@v1.0.0", "ex.com/shared@v1.0.0", "ex.com/shared@v1.5.0", "ex.com/tool@v2.0.0"]);
    let v = r.view();
    assert_eq!(v["ex.com/shared/@v/list"], b"v1.0.0\nv1.5.0\n");
    assert_eq!(v["ex.com/shared/@latest"], v["ex.com/shared/@v/v1.5.0.info"]);

    r.cfg.modules.clear();
    r.cfg.gomods = vec!["module corp/project\n\ngo 1.22\n\nrequire (\n\tex.com/app v1.0.0\n\tex.com/shared v1.0.0 // indirect\n\tex.com/Upper/Case v1.0.0\n)\n".into()];
    r.sync().unwrap();
    assert_eq!(r.have(".zip"), ["ex.com/!upper/!case@v1.0.0", "ex.com/app@v1.0.0", "ex.com/shared@v1.0.0"], "upper case is escaped as the go command asks for it");
}

#[test]
fn files_that_differ_from_the_checksum_database_are_refused() {
    let r = rig();
    r.px.publish("ex.com/app", "v1.0.0", "1.21", &[]);
    r.px.routes.lock().unwrap().insert("/proxy/ex.com/app/@v/v1.0.0.zip".into(), archive("ex.com/app", "tampered"));
    match r.sync() {
        Err(GoError::Checksum { what, .. }) => assert!(what.contains("archive of ex.com/app@v1.0.0"), "{what}"),
        other => panic!("{other:?}"),
    }
    assert!(r.st.head("go").unwrap().is_none());

    let r = rig();
    r.px.publish("ex.com/app", "v1.0.0", "1.21", &[]);
    r.px.routes.lock().unwrap().insert("/proxy/ex.com/app/@v/v1.0.0.mod".into(), b"module ex.com/app\nrequire evil.example/x v1.0.0\n".to_vec());
    assert!(matches!(r.sync(), Err(GoError::Checksum { what, .. }) if what.contains("go.mod")));

    // A module the database does not know is refused, unless the check is turned off.
    let mut r = rig();
    r.px.publish("ex.com/app", "v1.0.0", "1.21", &[]);
    r.px.routes.lock().unwrap().remove("/sumdb/lookup/ex.com/app@v1.0.0");
    assert!(matches!(r.sync(), Err(GoError::NotFound { .. })));
    r.cfg.sumdb_url = None;
    let rep = r.sync().unwrap();
    assert_eq!((rep.archives, rep.verified), (1, 0));
}

#[test]
fn problems_are_reported_clearly() {
    let mut r = rig();
    r.px.publish("ex.com/app", "v1.0.0", "1.21", &[("ex.com/gone", "v1.0.0")]);
    // A requirement the proxy no longer has is reported, the rest is mirrored.
    let rep = r.sync().unwrap();
    assert_eq!(r.have(".zip"), ["ex.com/app@v1.0.0"]);
    assert_eq!(rep.warnings.len(), 1, "{:?}", rep.warnings);
    for (spec, kind) in [("ex.com/nope", "missing"), ("ex.com/app@v9.9.9", "missing"), ("ex.com/app@1.0.0", "config"), ("../app", "config"), ("", "config")] {
        r.cfg.modules = vec![spec.into()];
        match (r.sync(), kind) {
            (Err(GoError::NotFound { .. }), "missing") | (Err(GoError::Config(_)), "config") => {}
            (other, _) => panic!("{spec:?}: {other:?}"),
        }
    }
    r.cfg.modules.clear();
    assert!(matches!(r.sync(), Err(GoError::Config(_))));
    r.cfg.gomods = vec!["module x\n".into()];
    assert!(matches!(r.sync(), Err(GoError::Config(_))));
    r.cfg.modules = vec!["ex.com/app".into()];
    r.st.create_repo("files", "raw").unwrap();
    let client = HttpClient::new(&[host_of(&r.px.base).unwrap()]);
    assert!(matches!(sync(&r.st, "files", &client, &r.cfg), Err(GoError::Config(_))));
}

#[test]
fn a_second_sync_downloads_only_what_is_new() {
    let r = rig();
    r.px.publish("ex.com/app", "v1.0.0", "1.21", &[("ex.com/lib", "v1.0.0")]).publish("ex.com/lib", "v1.0.0", "1.21", &[]);
    let first = r.sync().unwrap();
    assert_eq!((first.snapshot, first.downloaded), (Some(1), 6));
    let again = r.sync().unwrap();
    assert_eq!((again.snapshot, again.downloaded, again.reused, again.verified), (None, 0, 6, 0));
    r.px.publish("ex.com/app", "v1.1.0", "1.21", &[("ex.com/lib", "v1.0.0")]);
    let rep = r.sync().unwrap();
    assert_eq!((rep.downloaded, rep.reused, rep.snapshot.is_some()), (3, 3, true));
    assert_eq!(r.have(".zip"), ["ex.com/app@v1.1.0", "ex.com/lib@v1.0.0"]);
    assert_eq!(r.px.fetched(".zip").len(), 3);
}
