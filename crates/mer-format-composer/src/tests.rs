//! Checks against composer/semver itself.
use crate::version::{normalize, php_version_compare, stability, Constraint};
use serde_json::Value;
use std::cmp::Ordering;

#[test]
fn versions_and_constraints_match_composer() {
    let t: Value = serde_json::from_slice(&std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/composer_truth.json")).unwrap()).unwrap();
    let mut wrong = Vec::new();
    for c in t["normalize"].as_array().unwrap() {
        let (v, want, want_stab) = (c[0].as_str().unwrap(), c[1].as_str().unwrap(), c[2].as_str().unwrap());
        if normalize(v).as_deref() != Some(want) {
            wrong.push(format!("normalize({v:?}) = {:?}, composer says {want:?}", normalize(v)));
        }
        if stability(v) != want_stab {
            wrong.push(format!("stability({v:?}) = {}, composer says {want_stab}", stability(v)));
        }
    }
    for c in t["order"].as_array().unwrap() {
        let (a, b) = (c[0].as_str().unwrap(), c[1].as_str().unwrap());
        let want = match c[2].as_i64().unwrap() {
            -1 => Ordering::Less,
            0 => Ordering::Equal,
            _ => Ordering::Greater,
        };
        if php_version_compare(a, b) != want {
            wrong.push(format!("{a} vs {b}: {:?}, composer says {want:?}", php_version_compare(a, b)));
        }
    }
    for c in t["matches"].as_array().unwrap() {
        let (k, v, want) = (c[0].as_str().unwrap(), c[1].as_str().unwrap(), c[2].as_bool().unwrap());
        let got = Constraint::parse(k).map(|p| p.matches(v));
        if got != Some(want) {
            wrong.push(format!("{v:?} in {k:?}: {got:?}, composer says {want}"));
        }
    }
    assert!(wrong.is_empty(), "{} wrong:\n{}", wrong.len(), wrong[..wrong.len().min(40)].join("\n"));
}

use crate::*;
use mer_fetch::{host_of, HttpClient};
use mer_storage::Storage;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

struct Fake {
    base: String,
    routes: Routes,
}

fn zip(what: &str) -> Vec<u8> {
    [&b"PK\x03\x04"[..], what.repeat(20).as_bytes()].concat()
}

impl Fake {
    /// Packages: name -> [(version, require)], served minified as packagist does.
    fn new(pkgs: &[(&str, &[(&str, Value)])]) -> Fake {
        let routes: Routes = Arc::default();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        for (name, versions) in pkgs {
            let mut list = Vec::new();
            for (i, (v, require)) in versions.iter().enumerate() {
                let file = format!("/zips/{name}/{v}.zip");
                routes.lock().unwrap().insert(file.clone(), zip(&format!("{name} {v}")));
                let norm = crate::version::normalize(v).unwrap();
                let mut e = json!({ "version": v, "version_normalized": norm, "dist": { "type": "zip", "url": format!("{base}{file}"), "reference": format!("ref{i}abcdef0123456789"), "shasum": "" }, "source": { "type": "git", "url": "https://github.com/x/y.git" } });
                if i == 0 {
                    e["name"] = json!(name);
                    e["description"] = json!("a package");
                    e["require"] = require.clone();
                } else if require.is_null() {
                    e["require"] = json!("__unset");
                } else {
                    e["require"] = require.clone();
                }
                list.push(e);
            }
            let doc = json!({ "minified": "composer/2.0", "packages": { *name: list } });
            routes.lock().unwrap().insert(format!("/p2/{name}.json"), serde_json::to_vec(&doc).unwrap());
        }
        let r = routes.clone();
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let body = r.lock().unwrap().get(req.url()).cloned();
                let _ = match body {
                    Some(b) => req.respond(tiny_http::Response::from_data(b)),
                    None => req.respond(tiny_http::Response::from_string("not found").with_status_code(404)),
                };
            }
        });
        Fake { base, routes }
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    st: Storage,
    up: Fake,
    cfg: ComposerConfig,
}

fn rig(pkgs: &[(&str, &[(&str, Value)])]) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("php", "composer").unwrap();
    let up = Fake::new(pkgs);
    let cfg = ComposerConfig { upstream: up.base.clone(), packages: vec!["acme/app".into()], lockfiles: vec![], php: Some("8.3.14".into()), pre: false };
    Rig { _dir: dir, st, up, cfg }
}

impl Rig {
    fn sync(&self) -> Result<ComposerReport, ComposerError> {
        sync(&self.st, "php", &HttpClient::new(&[host_of(&self.up.base).unwrap()]).without_retry_delay(), &self.cfg)
    }

    fn view(&self) -> BTreeMap<String, Vec<u8>> {
        let repo = self.st.repo("php").unwrap();
        let Some(head) = self.st.head("php").unwrap() else { return BTreeMap::new() };
        self.st.meta().read(|t| t.entries_at(repo.id, head.seq)).unwrap().into_iter().map(|e| (e.path, self.st.read_blob(&e.digest).unwrap())).collect()
    }

    /// name -> versions in the vault's metadata
    fn served(&self) -> BTreeMap<String, Vec<String>> {
        self.view()
            .iter()
            .filter(|(k, _)| k.starts_with("p2/"))
            .flat_map(|(_, b)| {
                let doc: Value = serde_json::from_slice(b).unwrap();
                doc["packages"].as_object().unwrap().iter().map(|(n, l)| (n.clone(), l.as_array().unwrap().iter().map(|e| e["version"].as_str().unwrap().to_string()).collect())).collect::<Vec<_>>()
            })
            .collect()
    }
}

fn source() -> Vec<(&'static str, Vec<(&'static str, Value)>)> {
    vec![
        ("acme/app", vec![("1.0.0", json!({ "php": ">=8.1", "acme/lib": "^1.2", "ext-json": "*", "psr/log-implementation": "1.0" }))]),
        (
            "acme/lib",
            vec![
                ("v1.1.0", Value::Null),
                ("v1.2.0", json!({ "acme/util": "~2.0" })),
                ("v1.3.0-beta1", json!({ "acme/util": "~2.0" })),
                // The newest wants a PHP the air gap does not have.
                ("v1.9.0", json!({ "php": ">=9.0" })),
                ("v2.0.0", Value::Null),
            ],
        ),
        ("acme/util", vec![("2.0.0", Value::Null), ("2.0.5", Value::Null), ("2.1.0", Value::Null)]),
    ]
}

fn run(src: &[(&'static str, Vec<(&'static str, Value)>)]) -> Rig {
    let refs: Vec<(&str, &[(&str, Value)])> = src.iter().map(|(n, v)| (*n, v.as_slice())).collect();
    rig(&refs)
}

#[test]
fn constraints_resolve_for_this_php_and_the_metadata_points_at_the_vault() {
    let r = run(&source());
    let rep = r.sync().unwrap();
    assert_eq!((rep.packages, rep.downloaded, rep.snapshot), (3, 3, Some(1)), "{rep:?}");
    let served = r.served();
    assert_eq!(served["acme/lib"], ["v1.2.0"], "^1.2 for PHP 8.3: not the beta, not the one needing PHP 9, not 2.x");
    assert_eq!(served["acme/util"], ["2.1.0"], "~2.0 means >= 2.0, < 3.0: the newest");
    assert!(rep.warnings.iter().any(|w| w.contains("psr/log-implementation")), "{:?}", rep.warnings);

    let view = r.view();
    let index: Value = serde_json::from_slice(&view["packages.json"]).unwrap();
    assert_eq!(index["metadata-url"], "mer://base/p2/%package%.json");
    let lib: Value = serde_json::from_slice(&view["p2/acme/lib.json"]).unwrap();
    let e = &lib["packages"]["acme/lib"][0];
    let path = e["dist"]["url"].as_str().unwrap().strip_prefix("mer://base/").unwrap().to_string();
    assert!(path.starts_with("dists/acme/lib/1.2.0.0-ref1abcdef01"), "{path}");
    assert_eq!(e["dist"]["shasum"].as_str().unwrap(), hex::encode(<sha1::Sha1 as sha1::Digest>::digest(&view[&path])));
    assert!(e.get("source").is_none(), "nothing to clone from inside");
    assert_eq!(e["require"]["acme/util"], "~2.0", "the minified form is expanded");
    assert_eq!(e["description"], "a package", "fields carried over from the first entry");

    let again = r.sync().unwrap();
    assert_eq!((again.downloaded, again.reused, again.snapshot), (0, 3, None));
}

#[test]
fn unstable_versions_exact_versions_and_lockfiles() {
    let mut r = run(&source());
    r.cfg.packages = vec!["acme/lib:^1.3@beta".into(), "acme/util@2.0.0".into()];
    r.sync().unwrap();
    let served = r.served();
    assert_eq!((served["acme/lib"].clone(), served["acme/util"].clone()), (vec!["v1.3.0-beta1".to_string()], vec!["2.0.0".to_string(), "2.1.0".to_string()]), "1.3 beta asked for; 2.0.0 exactly plus what ~2.0 picks");

    let dir = tempfile::tempdir().unwrap();
    let lock = dir.path().join("composer.lock");
    std::fs::write(&lock, json!({ "packages": [{ "name": "acme/lib", "version": "v1.1.0" }], "packages-dev": [{ "name": "acme/util", "version": "2.1.0" }] }).to_string()).unwrap();
    r.cfg.packages = vec![];
    r.cfg.lockfiles = vec![lock.clone()];
    r.sync().unwrap();
    let served = r.served();
    assert_eq!((served["acme/lib"].clone(), served["acme/util"].clone()), (vec!["v1.1.0".to_string()], vec!["2.1.0".to_string()]), "exactly the lock, dev packages included, nothing more");
    std::fs::write(&lock, json!({ "packages": [{ "name": "acme/lib", "version": "v7.0.0" }] }).to_string()).unwrap();
    assert!(r.sync().unwrap_err().to_string().contains("v7.0.0"));
}

#[test]
fn bad_archives_missing_packages_and_settings() {
    let r = run(&source());
    let path = r.up.routes.lock().unwrap().keys().find(|k| k.contains("acme/util/2.1.0")).cloned().unwrap();
    r.up.routes.lock().unwrap().insert(path, b"<html>not a zip</html>".to_vec());
    assert!(r.sync().unwrap_err().to_string().contains("not a zip"));
    assert!(r.st.head("php").unwrap().is_none());

    let mut r = run(&source());
    r.cfg.packages = vec!["acme/nosuch".into()];
    assert!(r.sync().unwrap_err().to_string().contains("acme/nosuch"));
    for bad in ["Acme App", "noslash", "acme/../x"] {
        r.cfg.packages = vec![bad.into()];
        assert!(r.sync().is_err(), "{bad}");
    }
    r.cfg.packages = vec!["acme/app".into()];
    r.cfg.php = Some("eight".into());
    assert!(r.sync().is_err());
}

#[test]
fn minified_metadata_is_expanded() {
    let doc = json!({ "minified": "composer/2.0", "packages": { "a/b": [
        { "name": "a/b", "version": "2.0.0", "require": { "php": ">=8" }, "license": ["MIT"] },
        { "version": "1.0.0", "require": "__unset" },
        { "version": "0.9.0", "license": ["GPL"] }
    ] } });
    let e = expand(&doc, "a/b");
    assert_eq!(e.len(), 3);
    assert_eq!((e[1]["name"].as_str(), e[1].get("require"), e[1]["license"][0].as_str()), (Some("a/b"), None, Some("MIT")));
    assert_eq!((e[2].get("require"), e[2]["license"][0].as_str()), (None, Some("GPL")));
    assert!(valid_name("symfony/polyfill-php80") && !valid_name("Symfony/Console") && !valid_name("a/b/c"));
}
