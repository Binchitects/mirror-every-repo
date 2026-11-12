//! Sync tests against a fake sparse registry served over HTTP.
use crate::*;
use mer_fetch::{host_of, HttpClient};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

struct Registry {
    base: String,
    routes: Routes,
    hits: Arc<Mutex<Vec<String>>>,
}

fn crate_file(name: &str, vers: &str) -> Vec<u8> {
    format!("crate file {name} {vers} ").repeat(20).into_bytes()
}

impl Registry {
    fn new() -> Registry {
        let routes: Routes = Arc::default();
        let hits: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        routes.lock().unwrap().insert("/index/config.json".into(), format!(r#"{{"dl":"{base}/dl","api":"{base}"}}"#).into_bytes());
        let (r, h) = (routes.clone(), hits.clone());
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let path = req.url().to_string();
                h.lock().unwrap().push(path.clone());
                let body = r.lock().unwrap().get(&path).cloned();
                let _ = match body {
                    Some(b) => req.respond(tiny_http::Response::from_data(b)),
                    None => req.respond(tiny_http::Response::from_string("no").with_status_code(404)),
                };
            }
        });
        Registry { base, routes, hits }
    }

    /// Publish a version. `deps` are (name, requirement, kind) with an
    /// optional `=real-name` suffix on the name for a renamed dependency.
    fn publish(&self, name: &str, vers: &str, deps: &[(&str, &str, &str)]) -> &Self {
        self.publish_line(name, vers, deps, false)
    }

    /// Like publish, with a feature table: ("derive", &["dep:gen", "lib/fast"]).
    fn publish_with(&self, name: &str, vers: &str, deps: &[(&str, &str, &str)], features: &[(&str, &[&str])]) -> &Self {
        self.publish_line(name, vers, deps, false);
        let mut routes = self.routes.lock().unwrap();
        let file = routes.get_mut(&format!("/index/{}", index_path(name))).unwrap();
        let text = String::from_utf8(file.clone()).unwrap();
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        let mut line: Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        // Half in "features", half in "features2", as crates.io does for newer syntax.
        for (k, v) in features {
            let table = if v.iter().any(|i| i.starts_with("dep:") || i.contains("?/")) { "features2" } else { "features" };
            line[table][k] = serde_json::json!(v);
        }
        *lines.last_mut().unwrap() = line.to_string();
        *file = (lines.join("\n") + "\n").into_bytes();
        drop(routes);
        self
    }

    fn publish_line(&self, name: &str, vers: &str, deps: &[(&str, &str, &str)], yanked: bool) -> &Self {
        let body = crate_file(name, vers);
        let deps: Vec<Value> = deps
            .iter()
            .map(|(n, req, kind)| {
                let (alias, package) = match n.split_once('=') {
                    Some((a, p)) => (a, Some(p)),
                    None => (*n, None),
                };
                serde_json::json!({"name": alias, "req": req, "features": [], "optional": *kind == "optional", "default_features": true,
                    "target": null, "kind": if *kind == "optional" { "normal" } else { kind }, "package": package})
            })
            .collect();
        let line = serde_json::json!({"name": name, "vers": vers, "deps": deps, "cksum": Digest::of(&body).to_hex(), "features": {}, "yanked": yanked});
        let mut routes = self.routes.lock().unwrap();
        routes.insert(format!("/dl/{name}/{vers}/download"), body);
        let file = routes.entry(format!("/index/{}", index_path(name))).or_default();
        file.extend_from_slice(line.to_string().as_bytes());
        file.push(b'\n');
        self
    }

    fn downloads(&self) -> usize {
        self.hits.lock().unwrap().iter().filter(|p| p.ends_with("/download")).count()
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    st: Storage,
    reg: Registry,
    cfg: CargoConfig,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("crates", "cargo").unwrap();
    let reg = Registry::new();
    let cfg = CargoConfig { index_url: format!("{}/index/", reg.base), crates: vec!["app".into()], pins: Vec::new(), default_features_only: false, features: Vec::new() };
    Rig { _dir: dir, st, reg, cfg }
}

impl Rig {
    fn sync(&self) -> Result<CargoReport, CargoError> {
        sync(&self.st, "crates", &HttpClient::new(&[host_of(&self.reg.base).unwrap()]), &self.cfg)
    }

    fn view(&self) -> BTreeMap<String, Vec<u8>> {
        let repo = self.st.repo("crates").unwrap();
        let Some(head) = self.st.head("crates").unwrap() else { return BTreeMap::new() };
        self.st.meta().read(|t| t.entries_at(repo.id, head.seq)).unwrap().into_iter().map(|e| (e.path, self.st.read_blob(&e.digest).unwrap())).collect()
    }

    /// Mirrored crate files as "name-version".
    fn files(&self) -> Vec<String> {
        self.view().keys().filter_map(|p| p.strip_prefix("crates/")?.rsplit_once('/')?.1.strip_suffix(".crate").map(str::to_string)).collect()
    }
}

#[test]
fn index_paths_follow_the_registry_layout() {
    for (name, path) in [("a", "1/a"), ("ab", "2/ab"), ("abc", "3/a/abc"), ("serde", "se/rd/serde"), ("Inflector", "in/fl/inflector"), ("proc-macro2", "pr/oc/proc-macro2")] {
        assert_eq!(index_path(name), path);
    }
}

#[test]
fn the_newest_version_and_its_dependencies_are_mirrored() {
    let r = rig();
    r.reg
        .publish("app", "1.0.0", &[])
        .publish("app", "1.1.0", &[("lib", "^0.3", "normal"), ("gen", "1", "build"), ("extra", "2.0", "optional"), ("checker", "1", "dev")])
        .publish("app", "2.0.0-beta.1", &[])
        .publish("lib", "0.3.1", &[("abc", ">=1.0, <1.5", "normal")])
        .publish("lib", "0.3.9", &[("abc", ">=1.0, <1.5", "normal")])
        .publish("lib", "0.4.0", &[])
        .publish("gen", "1.4.0", &[])
        .publish("extra", "2.0.3", &[])
        .publish("checker", "1.0.0", &[])
        .publish("abc", "1.4.9", &[])
        .publish("abc", "1.5.0", &[]);
    let rep = r.sync().unwrap();
    assert_eq!(r.files(), ["abc-1.4.9", "app-1.1.0", "extra-2.0.3", "gen-1.4.0", "lib-0.3.9"], "no pre-release, no dev dependency");
    assert_eq!((rep.crates, rep.versions, rep.downloaded, rep.snapshot), (5, 5, 5, Some(1)));

    // The index lists only what was mirrored, with the upstream line unchanged.
    let v = r.view();
    let lines: Vec<Value> = String::from_utf8_lossy(&v["index/3/a/app"]).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["vers"], "1.1.0");
    assert_eq!(lines[0]["cksum"].as_str().unwrap(), Digest::of(&v["crates/app/app-1.1.0.crate"]).to_hex());
    assert_eq!(lines[0]["deps"].as_array().unwrap().len(), 4, "the dev dependency is still listed, as upstream wrote it");
    assert!(v.contains_key("index/3/a/abc") && v.contains_key("index/ex/tr/extra"));
    assert!(!v.contains_key("index/ch/ec/checker"));
}

#[test]
fn requirements_resolve_as_cargo_would() {
    let r = rig();
    for v in ["0.8.4", "0.8.5", "0.9.0", "1.0.0", "1.2.3", "1.3.0-rc.1"] {
        r.reg.publish("app", v, &[]);
    }
    r.reg.publish_line("app", "1.2.4", &[], true);
    let mut r = r;
    for (spec, want) in [("app", "app-1.2.3"), ("app@1", "app-1.2.3"), ("app@0.8", "app-0.8.5"), ("app@=0.8.4", "app-0.8.4"), ("app@~1.0", "app-1.0.0"), ("app@>=1.3.0-rc", "app-1.3.0-rc.1"), ("app@<1", "app-0.9.0")] {
        r.cfg.crates = vec![spec.into()];
        r.sync().unwrap();
        assert_eq!(r.files(), [want], "{spec}");
    }
    // Several requirements on one crate keep several versions.
    r.cfg.crates = vec!["app@0.8".into(), "app@1".into()];
    r.sync().unwrap();
    assert_eq!(r.files(), ["app-0.8.5", "app-1.2.3"]);
    assert_eq!(String::from_utf8_lossy(&r.view()["index/3/a/app"]).lines().count(), 2);
}

#[test]
fn a_lock_file_pins_exact_versions_even_yanked_ones() {
    let lock = r#"
version = 3

[[package]]
name = "myproject"
version = "0.1.0"
dependencies = [
 "app",
]

[[package]]
name = "app"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "aa"
dependencies = [
 "lib",
]

[[package]]
name = "lib"
version = "0.3.1"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "fromgit"
version = "0.1.0"
source = "git+https://example.com/x#abc"
"#;
    assert_eq!(parse_lockfile(lock), [("app".to_string(), "1.0.0".to_string()), ("lib".to_string(), "0.3.1".to_string())]);

    let mut r = rig();
    r.reg.publish("app", "1.0.0", &[("lib", "0.3", "normal")]).publish("app", "1.1.0", &[]).publish("lib", "0.3.9", &[]);
    r.reg.publish_line("lib", "0.3.1", &[], true);
    r.cfg.crates.clear();
    r.cfg.pins = parse_lockfile(lock);
    r.sync().unwrap();
    assert_eq!(r.files(), ["app-1.0.0", "lib-0.3.1"], "exactly the locked versions: a lock file is complete");
    // Named as well as locked: the fresh resolve is mirrored next to the locked one.
    r.cfg.crates = vec!["app@=1.0.0".into()];
    r.sync().unwrap();
    assert_eq!(r.files(), ["app-1.0.0", "lib-0.3.1", "lib-0.3.9"]);
    r.cfg.crates.clear();
    r.cfg.pins = vec![("app".into(), "9.9.9".into())];
    assert!(matches!(r.sync(), Err(CargoError::Unsatisfiable { .. })));
}

#[test]
fn renamed_dependencies_and_other_registries() {
    let r = rig();
    r.reg.publish("app", "1.0.0", &[("old_lib=lib", "0.3", "normal"), ("ghost", "1", "optional")]).publish("lib", "0.3.0", &[]);
    // A dependency that lives in another registry is reported, not followed.
    let mut routes = r.reg.routes.lock().unwrap();
    let file = routes.get_mut("/index/3/a/app").unwrap();
    let mut line: Value = serde_json::from_slice(file).unwrap();
    line["deps"].as_array_mut().unwrap().push(serde_json::json!({"name": "corp", "req": "1", "kind": "normal", "registry": "https://corp.example/index"}));
    *file = format!("{line}\n").into_bytes();
    drop(routes);
    let rep = r.sync().unwrap();
    assert_eq!(r.files(), ["app-1.0.0", "lib-0.3.0"]);
    assert!(rep.warnings.iter().any(|w| w.contains("corp") && w.contains("another registry")), "{:?}", rep.warnings);
    assert_eq!(rep.warnings.iter().filter(|w| w.contains("ghost")).count(), 1, "an unsatisfiable dependency is reported once: {:?}", rep.warnings);
}

#[test]
fn a_crate_that_differs_from_the_index_checksum_is_refused() {
    let r = rig();
    r.reg.publish("app", "1.0.0", &[]);
    r.reg.routes.lock().unwrap().insert("/dl/app/1.0.0/download".into(), b"tampered".to_vec());
    assert!(matches!(r.sync(), Err(CargoError::Storage(_))));
    assert!(r.st.head("crates").unwrap().is_none());
}

#[test]
fn download_url_templates_are_filled_like_cargo_does() {
    let line = parse_line(&format!(r#"{{"name":"Serde_x","vers":"1.2.3","deps":[],"cksum":"{}","yanked":false}}"#, "ab".repeat(32)), &mut Vec::new()).unwrap();
    assert_eq!(download_url("https://h/api/v1/crates", &line), "https://h/api/v1/crates/Serde_x/1.2.3/download");
    assert_eq!(download_url("https://h/{crate}/{crate}-{version}.crate", &line), "https://h/Serde_x/Serde_x-1.2.3.crate");
    assert_eq!(download_url("https://h/{prefix}/{lowerprefix}/{sha256-checksum}", &line), format!("https://h/Se/rd/se/rd/{}", "ab".repeat(32)));
}

#[test]
fn problems_are_reported_clearly() {
    let mut r = rig();
    r.reg.publish("app", "1.0.0", &[]);
    for (spec, kind) in [("nope", "unsat"), ("app@^9", "unsat"), ("app@not a req", "config"), ("../app", "config"), ("", "config")] {
        r.cfg.crates = vec![spec.into()];
        match (r.sync(), kind) {
            (Err(CargoError::Unsatisfiable { .. }), "unsat") | (Err(CargoError::Config(_)), "config") => {}
            (other, _) => panic!("{spec:?}: {other:?}"),
        }
    }
    r.cfg.crates.clear();
    assert!(matches!(r.sync(), Err(CargoError::Config(_))));
    r.cfg.crates = vec!["app".into()];
    r.st.create_repo("files", "raw").unwrap();
    let client = HttpClient::new(&[host_of(&r.reg.base).unwrap()]);
    assert!(matches!(sync(&r.st, "files", &client, &r.cfg), Err(CargoError::Config(_))));
}

#[test]
fn a_second_sync_downloads_only_what_is_new() {
    let r = rig();
    r.reg.publish("app", "1.0.0", &[("lib", "1", "normal")]).publish("lib", "1.0.0", &[]);
    assert_eq!(r.sync().unwrap().snapshot, Some(1));
    let again = r.sync().unwrap();
    assert_eq!((again.snapshot, again.downloaded, again.reused), (None, 0, 2));
    r.reg.publish("lib", "1.0.1", &[]);
    let rep = r.sync().unwrap();
    assert_eq!((rep.downloaded, rep.reused, r.reg.downloads()), (1, 1, 3));
    assert_eq!(r.files(), ["app-1.0.0", "lib-1.0.1"]);
}

#[test]
fn optional_dependencies_are_followed_only_where_a_feature_turns_them_on() {
    let mut r = rig();
    r.reg
        .publish_with(
            "app",
            "1.0.0",
            &[("lib", "1", "normal"), ("gen", "1", "optional"), ("big", "1", "optional"), ("net", "1", "optional"), ("tls", "1", "optional")],
            &[("default", &["std"]), ("std", &["lib/std"]), ("derive", &["dep:gen"]), ("secure", &["net", "tls?/strong", "net?/v6"]), ("everything", &["big"])],
        )
        // lib: its optional dependencies must not be pulled in by a crate that only uses lib.
        .publish_with("lib", "1.0.0", &[("heavy", "1", "optional"), ("alloc", "1", "optional"), ("simd", "1", "optional")], &[("std", &["dep:alloc"]), ("fast", &["dep:simd"])])
        .publish("gen", "1.0.0", &[])
        .publish("big", "1.0.0", &[("huge", "1", "normal")])
        .publish("huge", "1.0.0", &[])
        .publish_with("net", "1.0.0", &[("v6lib", "1", "optional")], &[("v6", &["dep:v6lib"])])
        .publish_with("tls", "1.0.0", &[("strongbox", "1", "optional")], &[("strong", &["dep:strongbox"])])
        .publish("heavy", "1.0.0", &[])
        .publish("alloc", "1.0.0", &[])
        .publish("simd", "1.0.0", &[])
        .publish("v6lib", "1.0.0", &[])
        .publish("strongbox", "1.0.0", &[]);

    // Default features only: default -> std -> lib/std -> alloc.
    r.cfg.default_features_only = true;
    r.sync().unwrap();
    assert_eq!(r.files(), ["alloc-1.0.0", "app-1.0.0", "lib-1.0.0"]);

    // One more feature, asked for by name.
    r.cfg.features = vec!["app/derive".into()];
    r.sync().unwrap();
    assert_eq!(r.files(), ["alloc-1.0.0", "app-1.0.0", "gen-1.0.0", "lib-1.0.0"]);

    // "secure" turns net on and passes v6 to it; tls stays off, so "strong" is not passed.
    r.cfg.features = vec!["app/secure".into()];
    r.sync().unwrap();
    assert_eq!(r.files(), ["alloc-1.0.0", "app-1.0.0", "lib-1.0.0", "net-1.0.0", "v6lib-1.0.0"]);

    // By default a named crate gets every feature; its dependencies only what is asked of them.
    r.cfg.features.clear();
    r.cfg.default_features_only = false;
    r.sync().unwrap();
    assert_eq!(
        r.files(),
        ["alloc-1.0.0", "app-1.0.0", "big-1.0.0", "gen-1.0.0", "huge-1.0.0", "lib-1.0.0", "net-1.0.0", "strongbox-1.0.0", "tls-1.0.0", "v6lib-1.0.0"],
        "heavy and simd are optional in lib and nothing asked for them"
    );

    r.cfg.features = vec!["lib/fast".into()];
    assert!(matches!(r.sync(), Err(CargoError::Config(m)) if m.contains("lib")), "a feature for a crate that is not named");
}
