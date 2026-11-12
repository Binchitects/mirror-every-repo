//! Sync tests against a fake npm registry served over HTTP.
use crate::*;
use base64::Engine;
use mer_fetch::{host_of, HttpClient};
use mer_storage::Storage;
use serde_json::{json, Value};
use sha2::{Digest as _, Sha512};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

struct Registry {
    base: String,
    routes: Routes,
    docs: BTreeMap<String, Value>,
    /// How many times each path was requested.
    hits: Arc<Mutex<HashMap<String, u32>>>,
}

fn tarball(name: &str, version: &str) -> Vec<u8> {
    format!("tarball of {name} {version} ").repeat(30).into_bytes()
}

fn sri(body: &[u8]) -> String {
    format!("sha512-{}", base64::engine::general_purpose::STANDARD.encode(Sha512::digest(body)))
}

impl Registry {
    fn new() -> Registry {
        let routes: Routes = Arc::default();
        let hits: Arc<Mutex<HashMap<String, u32>>> = Arc::default();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let (r, h) = (routes.clone(), hits.clone());
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let path = req.url().to_string();
                *h.lock().unwrap().entry(path.clone()).or_insert(0) += 1;
                let body = r.lock().unwrap().get(&path).cloned();
                let _ = match body {
                    Some(b) => req.respond(tiny_http::Response::from_data(b)),
                    None => req.respond(tiny_http::Response::from_string("{}").with_status_code(404)),
                };
            }
        });
        Registry { base, routes, docs: BTreeMap::new(), hits }
    }

    /// Publish one version. `extra` is merged into the version object
    /// (dependencies, os, cpu, peerDependencies, ...).
    fn publish(&mut self, name: &str, version: &str, extra: Value) -> &mut Self {
        let body = tarball(name, version);
        let file = format!("{}-{version}.tgz", name.rsplit('/').next().unwrap());
        let url_path = format!("/{name}/-/{file}");
        let mut object = json!({
            "name": name,
            "version": version,
            "dist": { "tarball": format!("{}{url_path}", self.base), "integrity": sri(&body) },
        });
        for (k, v) in extra.as_object().unwrap() {
            object[k] = v.clone();
        }
        self.routes.lock().unwrap().insert(url_path, body);
        let doc = self.docs.entry(name.to_string()).or_insert_with(|| json!({ "name": name, "dist-tags": {}, "versions": {}, "modified": "2026-01-01" }));
        doc["versions"][version] = object;
        // "latest" follows the highest published stable version, as npm publish does.
        let mut versions: Vec<Version> = doc["versions"].as_object().unwrap().keys().filter_map(|k| Version::parse(k)).filter(|v| v.pre.is_empty()).collect();
        versions.sort();
        if let Some(top) = versions.last() {
            doc["dist-tags"]["latest"] = json!(top.to_string());
        }
        self.flush();
        self
    }

    fn flush(&self) {
        let mut routes = self.routes.lock().unwrap();
        for (name, doc) in &self.docs {
            routes.insert(format!("/{}", name.replace('/', "%2f")), doc.to_string().into_bytes());
        }
    }

    fn tarball_downloads(&self) -> u32 {
        self.hits.lock().unwrap().iter().filter(|(p, _)| p.contains("/-/")).map(|(_, n)| *n).sum()
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    st: Storage,
    reg: Registry,
    cfg: NpmConfig,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("npm", "npm").unwrap();
    let reg = Registry::new();
    let cfg = NpmConfig { registry: reg.base.clone(), packages: vec!["app".into()], os: "linux".into(), cpu: "x64".into(), libc: "glibc".into() };
    Rig { _dir: dir, st, reg, cfg }
}

impl Rig {
    fn sync(&self) -> Result<NpmReport, NpmError> {
        sync(&self.st, "npm", &HttpClient::new(&[host_of(&self.reg.base).unwrap()]), &self.cfg)
    }

    fn view(&self) -> BTreeMap<String, Vec<u8>> {
        let repo = self.st.repo("npm").unwrap();
        let Some(head) = self.st.head("npm").unwrap() else { return BTreeMap::new() };
        self.st.meta().read(|t| t.entries_at(repo.id, head.seq)).unwrap().into_iter().map(|e| (e.path, self.st.read_blob(&e.digest).unwrap())).collect()
    }

    fn tarballs(&self) -> Vec<String> {
        self.view().keys().filter_map(|p| p.strip_prefix("tarballs/").map(str::to_string)).collect()
    }

    fn doc(&self, name: &str) -> Value {
        serde_json::from_slice(&self.view()[&format!("packuments/{name}.json")]).unwrap()
    }
}

#[test]
fn mirrors_the_versions_that_the_dependency_ranges_select() {
    let mut r = rig();
    r.reg
        .publish("app", "1.0.0", json!({}))
        .publish("app", "2.1.0", json!({ "dependencies": { "left": "^1.2.0", "right": "~0.4.1", "@scope/util": "2.x" } }))
        .publish("left", "1.2.0", json!({}))
        .publish("left", "1.9.3", json!({ "dependencies": { "deep": ">=1 <3" } }))
        .publish("left", "2.0.0", json!({}))
        .publish("right", "0.4.1", json!({}))
        .publish("right", "0.4.7", json!({}))
        .publish("right", "0.5.0", json!({}))
        .publish("@scope/util", "2.3.4", json!({}))
        .publish("@scope/util", "3.0.0", json!({}))
        .publish("deep", "2.9.0", json!({}))
        .publish("deep", "3.0.0", json!({}))
        .publish("unrelated", "1.0.0", json!({}));
    let rep = r.sync().unwrap();
    assert_eq!(
        r.tarballs(),
        ["@scope/util/util-2.3.4.tgz", "app/app-2.1.0.tgz", "deep/deep-2.9.0.tgz", "left/left-1.9.3.tgz", "right/right-0.4.7.tgz"]
    );
    assert_eq!((rep.packages, rep.versions, rep.downloaded, rep.snapshot), (5, 5, 5, Some(1)));
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
}

#[test]
fn package_documents_list_only_what_the_vault_has() {
    let mut r = rig();
    r.reg.publish("app", "1.0.0", json!({ "dependencies": { "left": "^1.0.0" } })).publish("left", "1.0.0", json!({ "bin": { "left": "cli.js" } })).publish("left", "1.5.0", json!({})).publish("left", "2.0.0", json!({}));
    r.sync().unwrap();
    let doc = r.doc("left");
    assert_eq!(doc["versions"].as_object().unwrap().keys().collect::<Vec<_>>(), ["1.5.0"]);
    // Upstream "latest" is 2.0.0, which is not mirrored: point at what exists.
    assert_eq!(doc["dist-tags"], json!({ "latest": "1.5.0" }));
    // The tarball URL is a placeholder for "this server", not the upstream address.
    assert_eq!(doc["versions"]["1.5.0"]["dist"]["tarball"], "mer://base/left/-/left-1.5.0.tgz");
    assert!(!r.view()["packuments/left.json"].windows(9).any(|w| w == b"127.0.0.1"), "no upstream address leaks into the vault");
    // The integrity hash is the registry's own, and matches the stored tarball.
    let stored = &r.view()["tarballs/left/left-1.5.0.tgz"];
    assert_eq!(doc["versions"]["1.5.0"]["dist"]["integrity"], sri(stored));
    assert!(doc.get("modified").is_none(), "volatile fields are dropped so unchanged content stays identical");
}

#[test]
fn two_ranges_can_need_two_versions_of_one_package() {
    let mut r = rig();
    r.reg
        .publish("app", "1.0.0", json!({ "dependencies": { "old-user": "1.0.0", "new-user": "1.0.0" } }))
        .publish("old-user", "1.0.0", json!({ "dependencies": { "shared": "^1.0.0" } }))
        .publish("new-user", "1.0.0", json!({ "dependencies": { "shared": "^2.0.0" } }))
        .publish("shared", "1.4.0", json!({}))
        .publish("shared", "2.2.0", json!({}));
    r.sync().unwrap();
    assert_eq!(r.doc("shared")["versions"].as_object().unwrap().len(), 2);
    assert_eq!(r.doc("shared")["dist-tags"]["latest"], "2.2.0");
}

#[test]
fn the_version_tagged_latest_is_preferred_when_it_fits() {
    let mut r = rig();
    r.reg.publish("app", "1.0.0", json!({ "dependencies": { "lib": "^1.0.0" } })).publish("lib", "1.1.0", json!({})).publish("lib", "1.2.0", json!({}));
    // The maintainers tagged 1.1.0 as latest (1.2.0 is, say, a canary).
    r.reg.docs.get_mut("lib").unwrap()["dist-tags"]["latest"] = json!("1.1.0");
    r.reg.flush();
    r.sync().unwrap();
    assert_eq!(r.tarballs(), ["app/app-1.0.0.tgz", "lib/lib-1.1.0.tgz"]);
}

#[test]
fn tags_aliases_and_explicit_versions_as_roots() {
    let mut r = rig();
    r.reg.publish("app", "1.0.0", json!({ "dependencies": { "renamed": "npm:real-name@^2.0.0" } })).publish("app", "2.0.0-beta.1", json!({})).publish("real-name", "2.5.0", json!({}));
    r.reg.docs.get_mut("app").unwrap()["dist-tags"]["next"] = json!("2.0.0-beta.1");
    r.reg.flush();
    r.sync().unwrap();
    assert_eq!(r.tarballs(), ["app/app-1.0.0.tgz", "real-name/real-name-2.5.0.tgz"], "alias is mirrored under its real name");
    r.cfg.packages = vec!["app@next".into()];
    r.sync().unwrap();
    assert_eq!(r.tarballs(), ["app/app-2.0.0-beta.1.tgz"]);
    r.cfg.packages = vec!["app@1.0.0".into(), "app@next".into()];
    r.sync().unwrap();
    assert_eq!(r.doc("app")["versions"].as_object().unwrap().len(), 2);
}

#[test]
fn optional_packages_for_other_platforms_are_left_out() {
    let mut r = rig();
    r.reg
        .publish("app", "1.0.0", json!({
            "dependencies": { "native-linux-x64": "1.0.0", "native-win32-x64": "1.0.0" },
            "optionalDependencies": { "native-linux-x64": "1.0.0", "native-win32-x64": "1.0.0", "native-linux-arm64": "1.0.0", "native-musl": "1.0.0", "gone": "1.0.0" },
        }))
        .publish("native-linux-x64", "1.0.0", json!({ "os": ["linux"], "cpu": ["x64"], "libc": ["glibc"] }))
        .publish("native-win32-x64", "1.0.0", json!({ "os": ["win32"], "cpu": ["x64"] }))
        .publish("native-linux-arm64", "1.0.0", json!({ "os": ["linux"], "cpu": ["arm64"] }))
        .publish("native-musl", "1.0.0", json!({ "os": ["linux"], "cpu": ["x64"], "libc": ["musl"] }));
    let rep = r.sync().unwrap();
    assert_eq!(r.tarballs(), ["app/app-1.0.0.tgz", "native-linux-x64/native-linux-x64-1.0.0.tgz"]);
    assert!(rep.warnings.is_empty(), "missing or foreign optional packages are normal: {:?}", rep.warnings);
}

#[test]
fn peer_dependencies_are_mirrored_unless_marked_optional() {
    let mut r = rig();
    r.reg
        .publish("app", "1.0.0", json!({ "peerDependencies": { "host": "^3.0.0", "maybe": "*" }, "peerDependenciesMeta": { "maybe": { "optional": true } }, "devDependencies": { "test-tool": "*" } }))
        .publish("host", "3.1.0", json!({}))
        .publish("maybe", "1.0.0", json!({}))
        .publish("test-tool", "1.0.0", json!({}));
    r.sync().unwrap();
    assert_eq!(r.tarballs(), ["app/app-1.0.0.tgz", "host/host-3.1.0.tgz"], "no optional peers, no dev dependencies");
}

#[test]
fn what_cannot_be_mirrored_is_reported() {
    let mut r = rig();
    r.reg.publish("app", "1.0.0", json!({ "dependencies": { "from-git": "git+https://example.com/x.git", "from-file": "file:../x", "too-new": "^9.0.0", "ghost": "*", "ok": "*" } })).publish("from-git", "1.0.0", json!({})).publish("from-file", "1.0.0", json!({})).publish("too-new", "1.0.0", json!({})).publish("ok", "1.0.0", json!({}));
    let rep = r.sync().unwrap();
    assert_eq!(r.tarballs(), ["app/app-1.0.0.tgz", "ok/ok-1.0.0.tgz"]);
    let w = rep.warnings.join("\n");
    for needle in ["from-git", "from-file", "too-new", "ghost"] {
        assert!(w.contains(needle), "warning about {needle} expected:\n{w}");
    }
    // A root that cannot be resolved is an error.
    for bad in ["nope", "app@^9", "app@git+https://x/y.git"] {
        r.cfg.packages = vec![bad.into()];
        assert!(matches!(r.sync(), Err(NpmError::Unsatisfiable { .. })), "{bad}");
    }
    for bad in ["", "../x", "@scope"] {
        r.cfg.packages = vec![bad.into()];
        assert!(matches!(r.sync(), Err(NpmError::Config(_))), "{bad:?}");
    }
}

#[test]
fn cycles_terminate() {
    let mut r = rig();
    r.reg.publish("app", "1.0.0", json!({ "dependencies": { "a": "*" } })).publish("a", "1.0.0", json!({ "dependencies": { "b": "*" } })).publish("b", "1.0.0", json!({ "dependencies": { "a": "*", "app": "*" } }));
    assert_eq!(r.sync().unwrap().versions, 3);
}

#[test]
fn a_tarball_that_does_not_match_its_integrity_hash_is_refused() {
    let mut r = rig();
    r.reg.publish("app", "1.0.0", json!({}));
    r.reg.routes.lock().unwrap().insert("/app/-/app-1.0.0.tgz".into(), b"backdoored tarball".to_vec());
    match r.sync() {
        Err(NpmError::Registry(m)) => assert!(m.contains("integrity"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(r.st.head("npm").unwrap().is_none(), "nothing is sealed");
}

#[test]
fn old_packages_with_only_a_sha1_are_still_verified() {
    use sha1::Sha1;
    let mut r = rig();
    r.reg.publish("app", "1.0.0", json!({}));
    let body = tarball("app", "1.0.0");
    let dist = &mut r.reg.docs.get_mut("app").unwrap()["versions"]["1.0.0"]["dist"];
    dist.as_object_mut().unwrap().remove("integrity");
    dist["shasum"] = json!(hex::encode(Sha1::digest(&body)));
    r.reg.flush();
    assert_eq!(r.sync().unwrap().versions, 1);

    // The same, with a wrong SHA-1.
    let r2 = {
        let mut r2 = rig();
        r2.reg.publish("app", "1.0.0", json!({}));
        let dist = &mut r2.reg.docs.get_mut("app").unwrap()["versions"]["1.0.0"]["dist"];
        dist.as_object_mut().unwrap().remove("integrity");
        dist["shasum"] = json!("0000000000000000000000000000000000000000");
        r2.reg.flush();
        r2
    };
    assert!(matches!(r2.sync(), Err(NpmError::Registry(_))));

    // And with no hash at all: skipped with a warning, never stored unverified.
    let mut r3 = rig();
    r3.reg.publish("app", "1.0.0", json!({}));
    r3.reg.docs.get_mut("app").unwrap()["versions"]["1.0.0"]["dist"].as_object_mut().unwrap().remove("integrity");
    r3.reg.flush();
    let rep = r3.sync().unwrap();
    assert_eq!(rep.versions, 0);
    assert!(rep.warnings[0].contains("no usable tarball or hash"));
}

#[test]
fn tarballs_on_other_hosts_or_with_odd_names_are_refused() {
    let mut r = rig();
    r.reg.publish("app", "1.0.0", json!({}));
    r.reg.docs.get_mut("app").unwrap()["versions"]["1.0.0"]["dist"]["tarball"] = json!("http://elsewhere.example/app/-/app-1.0.0.tgz");
    r.reg.flush();
    match r.sync() {
        Err(NpmError::Fetch(e)) => assert!(e.to_string().contains("allowlist"), "{e}"),
        other => panic!("{other:?}"),
    }
    let mut r = rig();
    r.reg.publish("app", "1.0.0", json!({}));
    r.reg.docs.get_mut("app").unwrap()["versions"]["1.0.0"]["dist"]["tarball"] = json!(format!("{}/app/-/..%2f..%2fevil.tgz", r.reg.base));
    r.reg.flush();
    let rep = r.sync().unwrap();
    assert_eq!(rep.versions, 0);
    assert!(r.tarballs().is_empty());
}

#[test]
fn a_second_sync_downloads_only_what_is_new() {
    let mut r = rig();
    r.reg.publish("app", "1.0.0", json!({ "dependencies": { "lib": "^1.0.0" } })).publish("lib", "1.0.0", json!({}));
    assert_eq!(r.sync().unwrap().snapshot, Some(1));
    assert_eq!(r.reg.tarball_downloads(), 2);

    // Nothing changed upstream (apart from the registry's "modified" stamp).
    r.reg.docs.get_mut("lib").unwrap()["modified"] = json!("2026-06-06");
    r.reg.flush();
    let again = r.sync().unwrap();
    assert_eq!((again.snapshot, again.downloaded, again.reused), (None, 0, 2));
    assert_eq!(r.reg.tarball_downloads(), 2, "no tarball was fetched again");

    r.reg.publish("lib", "1.1.0", json!({}));
    let rep = r.sync().unwrap();
    assert_eq!((rep.downloaded, rep.reused), (1, 1));
    assert_eq!(r.tarballs(), ["app/app-1.0.0.tgz", "lib/lib-1.1.0.tgz"], "the superseded version leaves the new snapshot");
    assert!(r.st.get_at("npm", 1, "tarballs/lib/lib-1.0.0.tgz").unwrap().is_some(), "and stays in the old one for rollback");
}

#[test]
fn only_npm_repos_are_accepted() {
    let mut r = rig();
    r.reg.publish("app", "1.0.0", json!({}));
    r.st.create_repo("files", "raw").unwrap();
    let client = HttpClient::new(&[host_of(&r.reg.base).unwrap()]);
    assert!(matches!(sync(&r.st, "files", &client, &r.cfg), Err(NpmError::Config(_))));
}
