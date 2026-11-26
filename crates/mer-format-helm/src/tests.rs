//! Sync tests against a fake chart repository served over HTTP.
use crate::*;
use mer_fetch::{host_of, HttpClient};
use mer_storage::Storage;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

struct Fake {
    base: String,
    routes: Routes,
    hits: Arc<Mutex<Vec<String>>>,
    /// (name, version, yaml lines of the entry beyond name/version/urls/digest)
    charts: Vec<(String, String, bool, String)>,
}

fn chart(name: &str, version: &str) -> Vec<u8> {
    format!("chart {name} {version} ").repeat(25).into_bytes()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
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
        Fake { base, routes, hits, charts: Vec::new() }
    }

    fn publish(&mut self, name: &str, version: &str) -> &mut Self {
        self.publish_with(name, version, true, "")
    }

    /// `with_digest`: state a digest in the index. `extra`: more YAML for the entry.
    fn publish_with(&mut self, name: &str, version: &str, with_digest: bool, extra: &str) -> &mut Self {
        self.routes.lock().unwrap().insert(format!("/repo/charts/{name}-{version}.tgz"), chart(name, version));
        self.charts.push((name.into(), version.into(), with_digest, extra.into()));
        let mut y = String::from("apiVersion: v1\nentries:\n");
        let mut names: Vec<&String> = self.charts.iter().map(|c| &c.0).collect();
        names.dedup();
        for n in names {
            y.push_str(&format!("  {n}:\n"));
            for (_, v, dig, extra) in self.charts.iter().filter(|c| &c.0 == n) {
                y.push_str(&format!("  - name: {n}\n    version: {v}\n    appVersion: 1.0\n    urls:\n    - charts/{n}-{v}.tgz\n"));
                if *dig {
                    y.push_str(&format!("    digest: {}\n", hex(&Sha256::digest(chart(n, v)))));
                }
                y.push_str(extra);
            }
        }
        y.push_str("generated: \"2024-01-01T00:00:00Z\"\n");
        self.routes.lock().unwrap().insert("/repo/index.yaml".into(), y.into_bytes());
        self
    }

    fn downloads(&self) -> usize {
        self.hits.lock().unwrap().iter().filter(|p| p.ends_with(".tgz")).count()
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    st: Storage,
    up: Fake,
    cfg: HelmConfig,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("helm", "helm").unwrap();
    let up = Fake::new();
    let cfg = HelmConfig { repo: format!("{}/repo/", up.base), charts: vec!["app".into()] };
    Rig { _dir: dir, st, up, cfg }
}

impl Rig {
    fn sync(&self) -> Result<HelmReport, HelmError> {
        sync(&self.st, "helm", &HttpClient::new(&[host_of(&self.up.base).unwrap()]), &self.cfg)
    }

    fn view(&self) -> BTreeMap<String, Vec<u8>> {
        let repo = self.st.repo("helm").unwrap();
        let Some(head) = self.st.head("helm").unwrap() else { return BTreeMap::new() };
        self.st.meta().read(|t| t.entries_at(repo.id, head.seq)).unwrap().into_iter().map(|e| (e.path, self.st.read_blob(&e.digest).unwrap())).collect()
    }

    fn charts(&self) -> Vec<String> {
        self.view().keys().filter_map(|p| p.strip_prefix("charts/")).map(str::to_string).collect()
    }
}

#[test]
fn the_newest_stable_version_or_an_exact_one_is_mirrored_with_an_index_of_the_subset() {
    let mut r = rig();
    r.up.publish("app", "1.0.0").publish("app", "1.2.0").publish("app", "2.0.0-rc.1").publish("other", "0.1.0");
    let rep = r.sync().unwrap();
    assert_eq!(r.charts(), ["app-1.2.0.tgz"]);
    assert_eq!((rep.charts, rep.versions, rep.downloaded, rep.unverified, rep.snapshot), (1, 1, 1, 0, Some(1)));

    let index: Value = serde_json::from_slice(&r.view()["index.yaml"]).unwrap();
    let entry = &index["entries"]["app"][0];
    assert_eq!(index["apiVersion"], "v1");
    assert_eq!(entry["urls"][0], "charts/app-1.2.0.tgz", "relative to the vault repository");
    assert_eq!(entry["appVersion"], "1.0", "kept as the text Helm reads, not a number");
    assert!(index["entries"].get("other").is_none());

    r.cfg.charts = vec!["app@1.0.0".into(), "app@2.0.0-rc.1".into(), "other".into()];
    r.sync().unwrap();
    assert_eq!(r.charts(), ["app-1.0.0.tgz", "app-2.0.0-rc.1.tgz", "other-0.1.0.tgz"]);
    let index: Value = serde_json::from_slice(&r.view()["index.yaml"]).unwrap();
    assert_eq!(index["entries"]["app"].as_array().unwrap().len(), 2);

    // Unchanged upstream: nothing downloaded again.
    let again = r.sync().unwrap();
    assert_eq!((again.downloaded, again.reused, again.snapshot), (0, 3, None));
    assert_eq!(r.up.downloads(), 4);
}

#[test]
fn versions_with_a_v_prefix_and_absolute_urls_work() {
    let mut r = rig();
    r.up.publish("cert-manager", "v1.16.0").publish("cert-manager", "v1.16.1");
    r.cfg.charts = vec!["cert-manager".into()];
    r.sync().unwrap();
    assert_eq!(r.charts(), ["cert-manager-v1.16.1.tgz"]);
    r.cfg.charts = vec!["cert-manager@1.16.0".into()];
    r.sync().unwrap();
    assert_eq!(r.charts(), ["cert-manager-v1.16.0.tgz"]);

    // An index that names its charts by absolute URL.
    let abs = String::from_utf8(r.up.routes.lock().unwrap()["/repo/index.yaml"].clone()).unwrap().replace("- charts/", &format!("- {}/repo/charts/", r.up.base));
    r.up.routes.lock().unwrap().insert("/repo/index.yaml".into(), abs.into_bytes());
    r.cfg.charts = vec!["cert-manager".into()];
    r.sync().unwrap();
    assert_eq!(r.charts(), ["cert-manager-v1.16.1.tgz"]);
}

#[test]
fn downloads_must_match_the_index_digest() {
    let mut r = rig();
    r.up.publish("app", "1.0.0");
    r.up.routes.lock().unwrap().insert("/repo/charts/app-1.0.0.tgz".into(), b"tampered".to_vec());
    assert!(matches!(r.sync(), Err(HelmError::Upstream(m)) if m.contains("digest")));
    assert!(r.view().is_empty());

    let mut r = rig();
    r.up.publish_with("app", "1.0.0", false, "");
    let rep = r.sync().unwrap();
    assert_eq!((rep.unverified, rep.warnings.len()), (1, 1));
    let index: Value = serde_json::from_slice(&r.view()["index.yaml"]).unwrap();
    assert_eq!(index["entries"]["app"][0]["digest"].as_str().unwrap().len(), 64, "the vault index states the digest it serves");
}

#[test]
fn unknown_charts_versions_and_bad_config_are_refused() {
    let mut r = rig();
    r.up.publish("app", "1.0.0");
    r.cfg.charts = vec!["nothere".into()];
    assert!(matches!(r.sync(), Err(HelmError::Upstream(_))));
    r.cfg.charts = vec!["app@9.9.9".into()];
    assert!(matches!(r.sync(), Err(HelmError::Upstream(_))));
    r.cfg.charts = vec!["../x".into()];
    assert!(matches!(r.sync(), Err(HelmError::Config(_))));
    r.cfg.charts = vec!["app".into()];
    r.cfg.repo = "ftp://x".into();
    assert!(matches!(r.sync(), Err(HelmError::Config(_))));
    r.st.create_repo("other", "npm").unwrap();
    r.cfg.repo = format!("{}/repo", r.up.base);
    assert!(matches!(sync(&r.st, "other", &HttpClient::new(&[host_of(&r.up.base).unwrap()]), &r.cfg), Err(HelmError::Config(_))));
}
