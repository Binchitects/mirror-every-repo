//! Sync tests against a fake sparse registry served over HTTP.
use crate::*;
use mer_fetch::{host_of, HttpClient};
use mer_storage::Storage;
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

struct FakeRegistry {
    base: String,
    routes: Routes,
    hits: Arc<Mutex<Vec<String>>>,
    /// name -> index lines
    lines: BTreeMap<String, Vec<Value>>,
}

fn crate_file(name: &str, version: &str) -> Vec<u8> {
    format!("crate {name} {version} ").repeat(30).into_bytes()
}

/// A dependency: (name, req, optional, kind, features, default_features, package).
fn dep(name: &str, req: &str) -> Value {
    json!({ "name": name, "req": req, "features": [], "optional": false, "default_features": true, "target": null, "kind": "normal" })
}

fn opt(name: &str, req: &str) -> Value {
    let mut d = dep(name, req);
    d["optional"] = json!(true);
    d
}

impl FakeRegistry {
    fn new() -> FakeRegistry {
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
        routes.lock().unwrap().insert("/index/config.json".into(), json!({ "dl": format!("{base}/dl"), "api": base }).to_string().into_bytes());
        FakeRegistry { base, routes, hits, lines: BTreeMap::new() }
    }

    fn publish(&mut self, name: &str, version: &str, deps: Vec<Value>, features: Value) -> &mut Self {
        self.publish_with(name, version, deps, features, |_| {})
    }

    fn publish_with(&mut self, name: &str, version: &str, deps: Vec<Value>, features: Value, edit: impl FnOnce(&mut Value)) -> &mut Self {
        let body = crate_file(name, version);
        let mut line = json!({
            "name": name, "vers": version, "deps": deps, "cksum": hex(&Sha256::digest(&body)),
            "features": features, "yanked": false,
        });
        edit(&mut line);
        let mut routes = self.routes.lock().unwrap();
        routes.insert(format!("/dl/{name}/{version}/download"), body);
        let lines = self.lines.entry(name.to_string()).or_default();
        lines.push(line);
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        routes.insert(format!("/index/{}", index_path(name)), text.into_bytes());
        drop(routes);
        self
    }

    fn downloads(&self) -> usize {
        self.hits.lock().unwrap().iter().filter(|p| p.starts_with("/dl/")).count()
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct Rig {
    _dir: tempfile::TempDir,
    st: Storage,
    reg: FakeRegistry,
    cfg: CargoConfig,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("cargo", "cargo").unwrap();
    let reg = FakeRegistry::new();
    let cfg = CargoConfig { index: format!("sparse+{}/index/", reg.base), crates: vec!["app".into()], lockfiles: vec![], rust_version: None };
    Rig { _dir: dir, st, reg, cfg }
}

impl Rig {
    fn sync(&self) -> Result<CargoReport, CargoError> {
        sync(&self.st, "cargo", &HttpClient::new(&[host_of(&self.reg.base).unwrap()]), &self.cfg)
    }

    fn view(&self) -> BTreeMap<String, Vec<u8>> {
        let repo = self.st.repo("cargo").unwrap();
        let Some(head) = self.st.head("cargo").unwrap() else { return BTreeMap::new() };
        self.st.meta().read(|t| t.entries_at(repo.id, head.seq)).unwrap().into_iter().map(|e| (e.path, self.st.read_blob(&e.digest).unwrap())).collect()
    }

    /// Mirrored crates as "name version".
    fn crates(&self) -> Vec<String> {
        self.view()
            .keys()
            .filter_map(|p| p.strip_prefix("crates/"))
            .map(|p| {
                let (name, file) = p.split_once('/').unwrap();
                format!("{name} {}", file.strip_prefix(&format!("{name}-")).unwrap().strip_suffix(".crate").unwrap())
            })
            .collect()
    }
}

#[test]
fn requirements_resolve_to_the_newest_match_and_dev_dependencies_are_skipped() {
    let mut r = rig();
    let mut tester = dep("tester", "1");
    tester["kind"] = json!("dev");
    let mut build = dep("cc", "1");
    build["kind"] = json!("build");
    r.reg
        .publish("app", "1.0.0", vec![dep("lib", "^1.2"), dep("exact", "=2.0.0"), tester, build], json!({}))
        .publish("lib", "1.0.0", vec![], json!({}))
        .publish("lib", "1.2.0", vec![], json!({}))
        .publish("lib", "1.9.0", vec![], json!({}))
        .publish("lib", "2.0.0", vec![], json!({}))
        .publish("exact", "2.0.0", vec![], json!({}))
        .publish("exact", "2.0.1", vec![], json!({}))
        .publish("tester", "1.0.0", vec![], json!({}))
        .publish("cc", "1.1.0", vec![], json!({}));
    let rep = r.sync().unwrap();
    assert_eq!(r.crates(), ["app 1.0.0", "cc 1.1.0", "exact 2.0.0", "lib 1.9.0"]);
    assert_eq!((rep.crates, rep.versions, rep.downloaded, rep.reused, rep.snapshot), (4, 4, 4, 0, Some(1)));
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);

    // The index lists only what was mirrored, line for line as upstream wrote it.
    let view = r.view();
    let lib = String::from_utf8(view["3/l/lib"].clone()).unwrap();
    assert_eq!(lib.lines().count(), 1);
    assert_eq!(serde_json::from_str::<Value>(lib.lines().next().unwrap()).unwrap()["vers"], "1.9.0");
    let config: Value = serde_json::from_slice(&view["config.json"]).unwrap();
    assert_eq!(config["dl"], "mer://base/crates/{crate}/{crate}-{version}.crate");
    assert_eq!(view["crates/lib/lib-1.9.0.crate"], crate_file("lib", "1.9.0"));

    // Nothing new upstream: nothing downloaded again, no new snapshot.
    let again = r.sync().unwrap();
    assert_eq!((again.downloaded, again.reused, again.snapshot), (0, 4, None));
    assert_eq!(r.reg.downloads(), 4);
}

#[test]
fn optional_dependencies_follow_the_features_that_are_turned_on() {
    let mut r = rig();
    let mut serde_dep = opt("serde", "1");
    serde_dep["features"] = json!(["derive"]);
    r.reg
        .publish("app", "1.0.0", vec![dep("lib", "1")], json!({}))
        .publish(
            "lib",
            "1.0.0",
            vec![serde_dep, opt("rayon", "1"), opt("log", "0.4"), opt("unused", "1"), opt("tracing", "0.1"), dep("core2", "1")],
            json!({
                "default": ["std", "serde"],
                "std": ["log?/std", "core2/std"],
                "parallel": ["dep:rayon"],
                "trace": ["tracing/attributes"],
            }),
        )
        .publish("serde", "1.0.0", vec![opt("serde_derive", "1")], json!({ "derive": ["serde_derive"] }))
        .publish("serde_derive", "1.0.0", vec![], json!({}))
        .publish("rayon", "1.0.0", vec![], json!({}))
        .publish("log", "0.4.0", vec![], json!({ "std": [] }))
        .publish("unused", "1.0.0", vec![], json!({}))
        .publish("tracing", "0.1.0", vec![], json!({ "attributes": [] }))
        .publish("core2", "1.0.0", vec![], json!({ "std": [] }));
    r.sync().unwrap();
    // default -> serde (implicit feature) -> its "derive" -> serde_derive. log?/std is weak: log stays off.
    assert_eq!(r.crates(), ["app 1.0.0", "core2 1.0.0", "lib 1.0.0", "serde 1.0.0", "serde_derive 1.0.0"]);

    // Features named on the root reach the crate they belong to.
    r.cfg.crates = vec!["lib:parallel,trace,log".into()];
    r.sync().unwrap();
    assert_eq!(r.crates(), ["core2 1.0.0", "lib 1.0.0", "log 0.4.0", "rayon 1.0.0", "serde 1.0.0", "serde_derive 1.0.0", "tracing 0.1.0"]);
}

#[test]
fn renamed_dependencies_are_fetched_under_their_real_name() {
    let mut r = rig();
    let mut renamed = dep("old_rand", "0.7");
    renamed["package"] = json!("rand");
    r.reg.publish("app", "1.0.0", vec![renamed, dep("rand", "0.8")], json!({})).publish("rand", "0.7.3", vec![], json!({})).publish("rand", "0.8.5", vec![], json!({}));
    r.sync().unwrap();
    assert_eq!(r.crates(), ["app 1.0.0", "rand 0.7.3", "rand 0.8.5"]);
    let index = String::from_utf8(r.view()["ra/nd/rand"].clone()).unwrap();
    assert_eq!(index.lines().count(), 2);
}

#[test]
fn yanked_versions_and_versions_needing_a_newer_toolchain_are_passed_over() {
    let mut r = rig();
    r.reg
        .publish("app", "1.0.0", vec![], json!({}))
        .publish_with("app", "1.1.0", vec![], json!({}), |l| l["rust_version"] = json!("1.70"))
        .publish_with("app", "1.2.0", vec![], json!({}), |l| l["rust_version"] = json!("1.85"))
        .publish_with("app", "1.3.0", vec![], json!({}), |l| l["yanked"] = json!(true))
        .publish("app", "2.0.0-beta.1", vec![], json!({}));
    r.sync().unwrap();
    assert_eq!(r.crates(), ["app 1.2.0"], "1.3.0 is yanked and a pre-release only matches when asked for");
    r.cfg.rust_version = Some("1.80".into());
    r.sync().unwrap();
    assert_eq!(r.crates(), ["app 1.1.0"]);
    r.cfg.rust_version = Some("1.60".into());
    let rep = r.sync().unwrap();
    assert_eq!(r.crates(), ["app 1.0.0"], "1.0.0 declares no rust-version");
    assert!(rep.warnings.is_empty());
    r.cfg.crates = vec!["app@2.0.0-beta.1".into()];
    r.sync().unwrap();
    assert_eq!(r.crates(), ["app 2.0.0-beta.1"]);
}

#[test]
fn a_lockfile_is_mirrored_exactly_and_its_checksums_must_agree() {
    let mut r = rig();
    r.reg
        .publish("serde", "1.0.100", vec![], json!({}))
        .publish("serde", "1.0.200", vec![], json!({}))
        .publish_with("anyhow", "1.0.0", vec![], json!({}), |l| l["yanked"] = json!(true));
    let sum = hex(&Sha256::digest(crate_file("serde", "1.0.100")));
    let lock = format!(
        "version = 4\n\n[[package]]\nname = \"app\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.100\"\nsource = \"sparse+{}/index/\"\nchecksum = \"{sum}\"\n\n[[package]]\nname = \"anyhow\"\nversion = \"1.0.0\"\nsource = \"sparse+{}/index/\"\n\n[[package]]\nname = \"other\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
        r.reg.base, r.reg.base
    );
    r.cfg.crates = vec![];
    r.cfg.lockfiles = vec![lock.clone()];
    let rep = r.sync().unwrap();
    assert_eq!(r.crates(), ["anyhow 1.0.0", "serde 1.0.100"], "exact versions, yanked or not, nothing resolved");
    assert_eq!(rep.warnings.len(), 1, "{:?}", rep.warnings);
    assert!(rep.warnings[0].contains("other"));

    r.cfg.lockfiles = vec![lock.replace(&sum, &"0".repeat(64))];
    assert!(matches!(r.sync(), Err(CargoError::Registry(m)) if m.contains("checksum")));
    r.cfg.lockfiles = vec![lock.replace("1.0.100", "1.0.150")];
    assert!(matches!(r.sync(), Err(CargoError::Unsatisfiable { .. })));
}

#[test]
fn a_download_that_does_not_match_the_index_is_refused() {
    let mut r = rig();
    r.reg.publish("app", "1.0.0", vec![], json!({}));
    r.reg.routes.lock().unwrap().insert("/dl/app/1.0.0/download".into(), b"tampered".to_vec());
    assert!(matches!(r.sync(), Err(CargoError::Registry(m)) if m.contains("checksum")));
    assert!(r.view().is_empty());
}

#[test]
fn missing_roots_fail_and_missing_dependencies_warn() {
    let mut r = rig();
    r.reg.publish("app", "1.0.0", vec![dep("gone", "1"), dep("lib", "^3")], json!({})).publish("lib", "1.0.0", vec![], json!({}));
    let rep = r.sync().unwrap();
    assert_eq!(r.crates(), ["app 1.0.0"]);
    assert_eq!(rep.warnings.len(), 2, "{:?}", rep.warnings);
    r.cfg.crates = vec!["nothere".into()];
    assert!(matches!(r.sync(), Err(CargoError::Unsatisfiable { .. })));
    r.cfg.crates = vec!["app@^5".into()];
    assert!(matches!(r.sync(), Err(CargoError::Unsatisfiable { .. })));
}

#[test]
fn a_wrong_repo_format_or_upstream_is_refused() {
    let r = rig();
    r.st.create_repo("other", "npm").unwrap();
    let client = HttpClient::new(&[host_of(&r.reg.base).unwrap()]);
    assert!(matches!(sync(&r.st, "other", &client, &r.cfg), Err(CargoError::Config(_))));
    let mut cfg = r.cfg.clone();
    cfg.index = "ftp://x/".into();
    assert!(matches!(sync(&r.st, "cargo", &client, &cfg), Err(CargoError::Config(_))));
    // Hosts not on the allowlist are never contacted.
    assert!(sync(&r.st, "cargo", &HttpClient::new(&["elsewhere".into()]), &r.cfg).is_err());
}

#[test]
fn features_asked_of_one_version_apply_to_every_mirrored_version_of_the_crate() {
    // X wants foo ^1.2 with feature "a"; Y pins foo =1.3.0. Cargo settles on
    // 1.3.0 with "a" on, so 1.3.0 needs a's optional dependency too.
    let mut r = rig();
    let mut foo_a = dep("foo", "^1.2");
    foo_a["features"] = json!(["a"]);
    r.reg
        .publish("app", "1.0.0", vec![dep("x", "1"), dep("y", "1")], json!({}))
        .publish("x", "1.0.0", vec![foo_a], json!({}))
        .publish("y", "1.0.0", vec![dep("foo", "=1.3.0")], json!({}))
        .publish("foo", "1.3.0", vec![opt("extra13", "1")], json!({ "a": ["dep:extra13"] }))
        .publish("foo", "1.9.0", vec![opt("extra19", "1")], json!({ "a": ["dep:extra19"] }))
        .publish("extra13", "1.0.0", vec![], json!({}))
        .publish("extra19", "1.0.0", vec![], json!({}));
    r.sync().unwrap();
    assert_eq!(r.crates(), ["app 1.0.0", "extra13 1.0.0", "extra19 1.0.0", "foo 1.3.0", "foo 1.9.0", "x 1.0.0", "y 1.0.0"]);
}
