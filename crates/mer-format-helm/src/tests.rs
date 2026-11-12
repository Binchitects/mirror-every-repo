//! Sync tests against a fake chart repository served over HTTP.
use crate::*;
use mer_fetch::{host_of, HttpClient};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

struct Upstream {
    base: String,
    routes: Routes,
    hits: Arc<Mutex<Vec<String>>>,
    /// (chart, version, digest override, url override)
    charts: Vec<(String, String, Option<String>, Option<String>)>,
}

fn chart(name: &str, version: &str) -> Vec<u8> {
    format!("chart archive {name} {version} ").repeat(30).into_bytes()
}

impl Upstream {
    fn new() -> Upstream {
        let routes: Routes = Arc::default();
        let hits: Arc<Mutex<Vec<String>>> = Arc::default();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}/stable", server.server_addr().to_ip().unwrap());
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
        Upstream { base, routes, hits, charts: Vec::new() }
    }

    fn publish(&mut self, name: &str, version: &str) -> &mut Self {
        self.charts.push((name.into(), version.into(), None, None));
        self.flush();
        self
    }

    fn flush(&self) {
        let mut routes = self.routes.lock().unwrap();
        let mut index = String::from("apiVersion: v1\nentries:\n");
        let mut names: Vec<&String> = self.charts.iter().map(|c| &c.0).collect();
        names.sort();
        names.dedup();
        for name in names {
            index.push_str(&format!("  {name}:\n"));
            for (n, version, digest, url) in self.charts.iter().filter(|c| &c.0 == name) {
                let body = chart(n, version);
                routes.insert(format!("/stable/charts/{n}-{version}.tgz"), body.clone());
                let digest = digest.clone().unwrap_or_else(|| Digest::of(&body).to_hex());
                let url = url.clone().unwrap_or_else(|| format!("charts/{n}-{version}.tgz"));
                index.push_str(&format!("  - apiVersion: v2\n    name: {n}\n    version: {version}\n    appVersion: \"1.0\"\n    description: \"a chart: with a colon\"\n"));
                if !digest.is_empty() {
                    index.push_str(&format!("    digest: {digest}\n"));
                }
                index.push_str(&format!("    urls:\n    - {url}\n"));
            }
        }
        index.push_str("generated: \"2026-01-01T00:00:00Z\"\n");
        routes.insert("/stable/index.yaml".into(), index.into_bytes());
    }

    fn chart_downloads(&self) -> usize {
        self.hits.lock().unwrap().iter().filter(|p| p.ends_with(".tgz")).count()
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    st: Storage,
    up: Upstream,
    cfg: HelmConfig,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("charts", "helm").unwrap();
    let up = Upstream::new();
    let cfg = HelmConfig { repo_url: up.base.clone(), charts: vec!["web".into()], prerelease: false };
    Rig { _dir: dir, st, up, cfg }
}

impl Rig {
    fn sync(&self) -> Result<HelmReport, HelmError> {
        sync(&self.st, "charts", &HttpClient::new(&[host_of(&self.up.base).unwrap()]), &self.cfg)
    }

    fn view(&self) -> BTreeMap<String, Vec<u8>> {
        let repo = self.st.repo("charts").unwrap();
        let Some(head) = self.st.head("charts").unwrap() else { return BTreeMap::new() };
        self.st.meta().read(|t| t.entries_at(repo.id, head.seq)).unwrap().into_iter().map(|e| (e.path, self.st.read_blob(&e.digest).unwrap())).collect()
    }

    fn files(&self) -> Vec<String> {
        self.view().keys().filter(|p| p.starts_with("charts/")).cloned().collect()
    }
}

#[test]
fn the_newest_stable_version_is_mirrored_and_the_index_lists_only_that() {
    let mut r = rig();
    r.up.publish("web", "1.0.0").publish("web", "1.2.0").publish("web", "2.0.0-rc.1").publish("db", "9.0.0");
    let rep = r.sync().unwrap();
    assert_eq!(rep.charts, vec![("web".to_string(), "1.2.0".to_string())]);
    assert_eq!((rep.downloaded, rep.snapshot), (1, Some(1)));
    assert_eq!(r.files(), ["charts/web-1.2.0.tgz"]);

    let v = r.view();
    let index: Value = serde_yaml::from_slice(&v["index.yaml"]).unwrap();
    let web = index["entries"]["web"].as_sequence().unwrap();
    assert_eq!(web.len(), 1);
    assert_eq!(web[0]["version"], "1.2.0");
    // The URL is relative, so it resolves against wherever the vault is reached.
    assert_eq!(web[0]["urls"][0], "charts/web-1.2.0.tgz");
    assert_eq!(web[0]["digest"].as_str().unwrap(), Digest::of(&v["charts/web-1.2.0.tgz"]).to_hex());
    assert_eq!(web[0]["description"], "a chart: with a colon", "other fields are carried over");
    assert!(index["entries"].get("db").is_none());
    assert!(!String::from_utf8_lossy(&v["index.yaml"]).contains("127.0.0.1"));
}

#[test]
fn versions_and_ranges_can_be_named_and_several_kept() {
    let mut r = rig();
    for v in ["1.0.0", "1.4.2", "1.9.0", "2.3.0"] {
        r.up.publish("web", v);
    }
    for (spec, want) in [("web@1.4.2", "charts/web-1.4.2.tgz"), ("web@^1.0.0", "charts/web-1.9.0.tgz"), ("web@~1.4", "charts/web-1.4.2.tgz"), ("web@2.x", "charts/web-2.3.0.tgz"), ("web@>=1.2, <1.9", "charts/web-1.4.2.tgz"), ("web@v1.9.0", "charts/web-1.9.0.tgz")] {
        r.cfg.charts = vec![spec.into()];
        r.sync().unwrap();
        assert_eq!(r.files(), [want], "{spec}");
    }
    r.cfg.charts = vec!["web@1.0.0".into(), "web".into()];
    r.sync().unwrap();
    assert_eq!(r.files(), ["charts/web-1.0.0.tgz", "charts/web-2.3.0.tgz"]);
    let index: Value = serde_yaml::from_slice(&r.view()["index.yaml"]).unwrap();
    let versions: Vec<&str> = index["entries"]["web"].as_sequence().unwrap().iter().map(|e| e["version"].as_str().unwrap()).collect();
    assert_eq!(versions, ["2.3.0", "1.0.0"], "newest first");

    r.cfg.charts = vec!["web".into()];
    r.cfg.prerelease = true;
    r.up.publish("web", "3.0.0-beta.1");
    r.sync().unwrap();
    assert_eq!(r.files(), ["charts/web-3.0.0-beta.1.tgz"]);
}

#[test]
fn a_chart_that_differs_from_the_index_digest_is_refused() {
    let mut r = rig();
    r.up.publish("web", "1.0.0");
    r.up.routes.lock().unwrap().insert("/stable/charts/web-1.0.0.tgz".into(), b"tampered chart".to_vec());
    assert!(matches!(r.sync(), Err(HelmError::Storage(_))));
    assert!(r.st.head("charts").unwrap().is_none());

    // No digest in the index: mirrored, but said so.
    let mut r = rig();
    r.up.charts.push(("web".into(), "1.0.0".into(), Some(String::new()), None));
    r.up.flush();
    let rep = r.sync().unwrap();
    assert_eq!(rep.charts.len(), 1);
    assert!(rep.warnings[0].contains("no digest"));
}

#[test]
fn download_urls_are_resolved_and_confined() {
    // Absolute URL on another host: refused by the allowlist.
    let mut r = rig();
    r.up.charts.push(("web".into(), "1.0.0".into(), None, Some("http://elsewhere.example/web-1.0.0.tgz".into())));
    r.up.flush();
    match r.sync() {
        Err(HelmError::Fetch(e)) => assert!(e.to_string().contains("allowlist"), "{e}"),
        other => panic!("{other:?}"),
    }
    // Traversal or another scheme in the URL: not followed.
    for bad in ["../../etc/passwd", "file:///etc/passwd"] {
        let mut r = rig();
        r.up.charts.push(("web".into(), "1.0.0".into(), None, Some(bad.into())));
        r.up.flush();
        assert!(matches!(r.sync(), Err(HelmError::Repo(m)) if m.contains("no usable download URL")), "{bad}");
    }
    assert_eq!(resolve("https://h/charts", "web-1.0.0.tgz").as_deref(), Some("https://h/charts/web-1.0.0.tgz"));
    assert_eq!(resolve("https://h/a/b/", "/files/web.tgz").as_deref(), Some("https://h/files/web.tgz"));
    assert_eq!(resolve("https://h/a", "https://cdn/x.tgz").as_deref(), Some("https://cdn/x.tgz"));
}

#[test]
fn problems_are_reported_clearly() {
    let mut r = rig();
    r.up.publish("web", "1.0.0");
    for (spec, kind) in [("nope", "unsat"), ("web@^9", "unsat"), ("web@not a range", "config"), ("../web", "config"), ("Web", "config"), ("", "config")] {
        r.cfg.charts = vec![spec.into()];
        match (r.sync(), kind) {
            (Err(HelmError::Unsatisfiable { .. }), "unsat") | (Err(HelmError::Config(_)), "config") => {}
            (other, _) => panic!("{spec:?}: {other:?}"),
        }
    }
    r.cfg.charts = vec!["web".into()];
    r.up.routes.lock().unwrap().insert("/stable/index.yaml".into(), b"entries: [not, a, mapping".to_vec());
    assert!(matches!(r.sync(), Err(HelmError::Repo(_))));
    r.st.create_repo("files", "raw").unwrap();
    let client = HttpClient::new(&[host_of(&r.up.base).unwrap()]);
    assert!(matches!(sync(&r.st, "files", &client, &r.cfg), Err(HelmError::Config(_))));
}

#[test]
fn a_second_sync_downloads_only_what_is_new() {
    let mut r = rig();
    r.up.publish("web", "1.0.0");
    assert_eq!(r.sync().unwrap().snapshot, Some(1));
    let again = r.sync().unwrap();
    assert_eq!((again.snapshot, again.downloaded, again.reused), (None, 0, 1));
    assert_eq!(r.up.chart_downloads(), 1);
    r.up.publish("web", "1.1.0");
    let rep = r.sync().unwrap();
    assert_eq!((rep.downloaded, r.files()), (1, vec!["charts/web-1.1.0.tgz".to_string()]));
    assert!(r.st.get_at("charts", 1, "charts/web-1.0.0.tgz").unwrap().is_some());
}
