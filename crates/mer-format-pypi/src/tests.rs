//! Sync tests against a fake package index served over HTTP.
use crate::*;
use mer_core::Digest;
use mer_fetch::{host_of, HttpClient};
use mer_storage::Storage;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

/// A fake upstream: an index host and, optionally, a separate file host.
struct Upstream {
    base: String,
    routes: Routes,
    /// project -> file entries of its index page
    pages: BTreeMap<String, Vec<Value>>,
}

fn serve(routes: Routes) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let path = req.url().split('?').next().unwrap().to_string();
            let body = routes.lock().unwrap().get(&path).cloned();
            let _ = match body {
                Some(b) => req.respond(tiny_http::Response::from_data(b)),
                None => req.respond(tiny_http::Response::from_string("no").with_status_code(404)),
            };
        }
    });
    format!("http://{addr}")
}

fn body_of(filename: &str) -> Vec<u8> {
    format!("content of {filename} ").repeat(20).into_bytes()
}

impl Upstream {
    fn new() -> Upstream {
        let routes: Routes = Arc::default();
        Upstream { base: serve(routes.clone()), routes, pages: BTreeMap::new() }
    }

    /// Publish one file. `requires` are Requires-Dist lines for its metadata
    /// (None = the index offers no metadata for this file).
    fn file(&mut self, project: &str, filename: &str, requires_python: Option<&str>, requires: Option<&[&str]>) -> &mut Self {
        let body = body_of(filename);
        let path = format!("/files/{filename}");
        let mut entry = json!({
            "filename": filename,
            "url": format!("../../files/{filename}"),
            "hashes": { "sha256": Digest::of(&body).to_hex() },
            "size": body.len(),
        });
        if let Some(rp) = requires_python {
            entry["requires-python"] = json!(rp);
        }
        if let Some(reqs) = requires {
            let mut meta = format!("Metadata-Version: 2.1\nName: {project}\n");
            for r in reqs {
                meta.push_str(&format!("Requires-Dist: {r}\n"));
            }
            meta.push_str("\nLong description, with a line that looks like one:\nRequires-Dist: not-a-real-dependency\n");
            entry["core-metadata"] = json!({ "sha256": Digest::of(meta.as_bytes()).to_hex() });
            self.routes.lock().unwrap().insert(format!("{path}.metadata"), meta.into_bytes());
        }
        self.routes.lock().unwrap().insert(path, body);
        self.pages.entry(project.to_string()).or_default().push(entry);
        self.publish();
        self
    }

    fn wheel(&mut self, project: &str, version: &str, requires: &[&str]) -> &mut Self {
        self.file(project, &format!("{}-{version}-py3-none-any.whl", project.replace('-', "_")), None, Some(requires))
    }

    fn publish(&self) {
        let mut routes = self.routes.lock().unwrap();
        for (project, files) in &self.pages {
            let page = json!({ "meta": { "api-version": "1.1" }, "name": project, "files": files });
            routes.insert(format!("/simple/{project}/"), page.to_string().into_bytes());
        }
    }

    fn edit(&mut self, project: &str, filename: &str, f: impl Fn(&mut Value)) {
        for entry in self.pages.get_mut(project).unwrap() {
            if entry["filename"] == filename {
                f(entry);
            }
        }
        self.publish();
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    st: Storage,
    up: Upstream,
    cfg: PyConfig,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("pypi", "pypi").unwrap();
    let up = Upstream::new();
    let cfg = PyConfig {
        index_url: format!("{}/simple", up.base),
        requirements: vec!["app".into()],
        pythons: vec!["3.11".into()],
        arch: "x86_64".into(),
        libcs: vec!["manylinux".into()],
        sdists: Sdists::IfNoWheel,
        prereleases: false,
    };
    Rig { _dir: dir, st, up, cfg }
}

impl Rig {
    fn sync(&self) -> Result<PyReport, PyError> {
        let client = HttpClient::new(&[host_of(&self.up.base).unwrap()]);
        sync(&self.st, "pypi", &client, &self.cfg)
    }

    fn view(&self) -> BTreeMap<String, Vec<u8>> {
        let repo = self.st.repo("pypi").unwrap();
        let Some(head) = self.st.head("pypi").unwrap() else { return BTreeMap::new() };
        self.st
            .meta()
            .read(|t| t.entries_at(repo.id, head.seq))
            .unwrap()
            .into_iter()
            .map(|e| (e.path, self.st.read_blob(&e.digest).unwrap()))
            .collect()
    }

    fn packages(&self) -> Vec<String> {
        self.view().keys().filter(|p| p.starts_with("packages/") && !p.ends_with(".metadata")).cloned().collect()
    }
}

fn versions(rep: &PyReport) -> Vec<(&str, &str)> {
    rep.projects.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect()
}

#[test]
fn mirrors_the_requested_project_and_what_it_needs_on_the_targets() {
    let mut r = rig();
    r.up.wheel("app", "1.0", &[])
        .wheel("app", "2.0", &[
            "lib-core (>=1.0,<2)",
            "Typing_Extensions>=4 ; python_version < '3.12'",
            "win-only ; sys_platform == 'win32'",
            "backport ; python_version < '3.8'",
            "extra-thing ; extra == 'fancy'",
        ])
        .wheel("lib-core", "1.4", &["leaf"])
        .wheel("lib-core", "2.0", &[])
        .wheel("typing-extensions", "4.9.0", &[])
        .wheel("leaf", "0.1", &[])
        .wheel("win-only", "1.0", &[])
        .wheel("backport", "1.0", &[])
        .wheel("extra-thing", "1.0", &[])
        .wheel("unrelated", "9.0", &[]);
    let rep = r.sync().unwrap();
    assert_eq!(
        versions(&rep),
        [("app", "2.0"), ("leaf", "0.1"), ("lib-core", "1.4"), ("typing-extensions", "4.9.0")],
        "newest app; lib-core held below 2; marker-excluded and unrelated projects left out"
    );
    assert_eq!((rep.files, rep.downloaded, rep.reused, rep.snapshot), (4, 4, 0, Some(1)));
    assert!(rep.warnings.is_empty(), "{:?}", rep.warnings);
    assert_eq!(
        r.packages(),
        [
            "packages/app/app-2.0-py3-none-any.whl",
            "packages/leaf/leaf-0.1-py3-none-any.whl",
            "packages/lib-core/lib_core-1.4-py3-none-any.whl",
            "packages/typing-extensions/typing_extensions-4.9.0-py3-none-any.whl",
        ]
    );
}

#[test]
fn the_generated_index_points_at_files_that_exist_with_the_stated_hash() {
    let mut r = rig();
    r.up.file("app", "app-1.0-py3-none-any.whl", Some(">=3.8"), Some(&["leaf"])).wheel("leaf", "0.1", &[]);
    r.sync().unwrap();
    let v = r.view();
    let root = String::from_utf8(v["simple/index.html"].clone()).unwrap();
    assert!(root.contains("<a href=\"app/\">app</a>") && root.contains("<a href=\"leaf/\">leaf</a>"));

    let page = String::from_utf8(v["simple/app/index.html"].clone()).unwrap();
    assert!(page.contains("pypi:repository-version"));
    assert!(page.contains("data-requires-python=\"&gt;=3.8\""));
    // Every link resolves, relative to /simple/<project>/, to a stored file with that hash.
    let mut links = 0;
    for part in page.split("<a href=\"").skip(1) {
        let href = part.split('"').next().unwrap();
        let (rel, hash) = href.split_once("#sha256=").unwrap();
        let path = rel.strip_prefix("../../").unwrap();
        assert_eq!(Digest::of(&v[path]).to_hex(), hash, "{path}");
        links += 1;
    }
    assert_eq!(links, 1);
    // Metadata is served next to the wheel and advertised with its own hash.
    let meta = &v["packages/app/app-1.0-py3-none-any.whl.metadata"];
    assert!(page.contains(&format!("data-core-metadata=\"sha256={}\"", Digest::of(meta))));
    assert!(String::from_utf8_lossy(meta).contains("Requires-Dist: leaf"));
}

#[test]
fn text_in_the_long_description_is_not_mistaken_for_a_dependency() {
    let mut r = rig();
    r.up.wheel("app", "1.0", &[]).wheel("not-a-real-dependency", "1.0", &[]);
    assert_eq!(versions(&r.sync().unwrap()), [("app", "1.0")]);
}

#[test]
fn only_files_the_targets_can_install_are_mirrored() {
    let mut r = rig();
    r.cfg.requirements = vec!["native".into()];
    for f in [
        "native-1.0-cp311-cp311-manylinux_2_17_x86_64.manylinux2014_x86_64.whl",
        "native-1.0-cp310-cp310-manylinux_2_17_x86_64.manylinux2014_x86_64.whl",
        "native-1.0-cp311-cp311-win_amd64.whl",
        "native-1.0-cp311-cp311-macosx_11_0_arm64.whl",
        "native-1.0-cp311-cp311-manylinux_2_17_aarch64.whl",
        "native-1.0.tar.gz",
    ] {
        r.up.file("native", f, None, Some(&[]));
    }
    r.sync().unwrap();
    assert_eq!(r.packages(), ["packages/native/native-1.0-cp311-cp311-manylinux_2_17_x86_64.manylinux2014_x86_64.whl"]);

    // A second interpreter adds its wheel; "always" adds the source archive.
    r.cfg.pythons = vec!["3.10".into(), "3.11".into()];
    r.cfg.sdists = Sdists::Always;
    let rep = r.sync().unwrap();
    assert_eq!((r.packages().len(), rep.downloaded, rep.reused), (3, 2, 1));
}

#[test]
fn versions_that_need_a_newer_python_are_skipped() {
    let mut r = rig();
    r.up.file("app", "app-1.5-py3-none-any.whl", Some(">=3.8"), Some(&[]))
        .file("app", "app-2.0-py3-none-any.whl", Some(">=3.12"), Some(&[]));
    assert_eq!(versions(&r.sync().unwrap()), [("app", "1.5")]);
    r.cfg.pythons = vec!["3.12".into()];
    assert_eq!(versions(&r.sync().unwrap()), [("app", "2.0")]);
}

#[test]
fn yanked_and_pre_releases_are_avoided_unless_asked_for() {
    let mut r = rig();
    r.up.wheel("app", "1.0", &[]).wheel("app", "1.1", &[]).wheel("app", "2.0rc1", &[]);
    r.up.edit("app", "app-1.1-py3-none-any.whl", |e| e["yanked"] = json!("broken release"));
    assert_eq!(versions(&r.sync().unwrap()), [("app", "1.0")]);
    r.cfg.requirements = vec!["app>=2.0rc1".into()];
    assert_eq!(versions(&r.sync().unwrap()), [("app", "2.0rc1")], "asked for by name");
    r.cfg.requirements = vec!["app".into()];
    r.cfg.prereleases = true;
    assert_eq!(versions(&r.sync().unwrap()), [("app", "2.0rc1")]);
}

#[test]
fn a_project_with_only_pre_releases_still_resolves_like_pip_does() {
    let mut r = rig();
    r.up.wheel("app", "0.1a1", &[]).wheel("app", "0.1b2", &[]);
    assert_eq!(versions(&r.sync().unwrap()), [("app", "0.1b2")]);
}

#[test]
fn constraints_from_two_parents_are_combined() {
    let mut r = rig();
    r.cfg.requirements = vec!["a".into(), "b".into()];
    r.up.wheel("a", "1.0", &["shared>=2"])
        .wheel("b", "1.0", &["shared<3"])
        .wheel("shared", "1.9", &[])
        .wheel("shared", "2.5", &[])
        .wheel("shared", "3.1", &[]);
    let rep = r.sync().unwrap();
    assert_eq!(versions(&rep), [("a", "1.0"), ("b", "1.0"), ("shared", "2.5")]);
    assert_eq!(r.packages().iter().filter(|p| p.contains("shared")).count(), 1, "one version, not three");
}

#[test]
fn extras_pull_in_their_dependencies() {
    let mut r = rig();
    r.up.wheel("app", "1.0", &["base", "socks-lib ; extra == 'socks'", "tls-lib ; extra == \"tls\" and python_version >= '3'"])
        .wheel("base", "1.0", &[])
        .wheel("socks-lib", "1.0", &[])
        .wheel("tls-lib", "1.0", &[]);
    assert_eq!(versions(&r.sync().unwrap()), [("app", "1.0"), ("base", "1.0")]);
    r.cfg.requirements = vec!["app[socks]".into()];
    assert_eq!(versions(&r.sync().unwrap()), [("app", "1.0"), ("base", "1.0"), ("socks-lib", "1.0")]);
}

#[test]
fn dependency_cycles_terminate() {
    let mut r = rig();
    r.up.wheel("app", "1.0", &["loop-a"]).wheel("loop-a", "1.0", &["loop-b"]).wheel("loop-b", "1.0", &["loop-a", "app"]);
    assert_eq!(versions(&r.sync().unwrap()).len(), 3);
}

#[test]
fn source_only_projects_follow_the_sdist_policy_and_warn_about_dependencies() {
    let mut r = rig();
    r.up.file("app", "app-1.0.tar.gz", None, None);
    let rep = r.sync().unwrap();
    assert_eq!(r.packages(), ["packages/app/app-1.0.tar.gz"]);
    assert!(rep.warnings.iter().any(|w| w.contains("no metadata")), "{:?}", rep.warnings);
    r.cfg.sdists = Sdists::Never;
    assert!(matches!(r.sync(), Err(PyError::Unsatisfiable { .. })));
}

#[test]
fn problems_are_reported_clearly() {
    let mut r = rig();
    r.up.wheel("app", "1.0", &["ghost>=5", "by-url @ https://example.com/x.whl", "old<1"]).wheel("old", "2.0", &[]);
    // A dependency that does not exist upstream at all stops the sync...
    assert!(matches!(r.sync(), Err(PyError::Index(m)) if m.contains("ghost")));
    // ...one that exists but cannot be satisfied is left out with a warning.
    r.up.wheel("ghost", "1.0", &[]);
    let rep = r.sync().unwrap();
    assert_eq!(versions(&rep), [("app", "1.0")]);
    let w = rep.warnings.join("\n");
    assert!(w.contains("ghost") && w.contains("old") && w.contains("by-url"), "{w}");

    // A root that cannot be satisfied is an error, not a silent hole.
    r.cfg.requirements = vec!["app>=9".into()];
    assert!(matches!(r.sync(), Err(PyError::Unsatisfiable { project, .. }) if project == "app"));
    for bad in ["", "app @ https://x/y.whl", "app ; python_version < '3'", "app==what"] {
        r.cfg.requirements = vec![bad.into()];
        assert!(matches!(r.sync(), Err(PyError::Config(_))), "{bad:?}");
    }
    r.cfg.requirements = vec!["app".into()];
    r.cfg.pythons = vec!["2.7".into()];
    assert!(matches!(r.sync(), Err(PyError::Config(_))));
}

#[test]
fn tampered_downloads_never_enter_the_store() {
    // A file whose bytes differ from the hash in the index.
    let mut r = rig();
    r.up.wheel("app", "1.0", &[]);
    r.up.routes.lock().unwrap().insert("/files/app-1.0-py3-none-any.whl".into(), b"malicious payload".to_vec());
    assert!(matches!(r.sync(), Err(PyError::Storage(_))));
    assert!(r.st.head("pypi").unwrap().is_none());
    assert_eq!(r.st.meta().read(|t| t.blob_count()).unwrap().0, 0);

    // Metadata that differs from its stated hash.
    let mut r = rig();
    r.up.wheel("app", "1.0", &[]);
    r.up.routes.lock().unwrap().insert("/files/app-1.0-py3-none-any.whl.metadata".into(), b"Requires-Dist: evil\n".to_vec());
    assert!(matches!(r.sync(), Err(PyError::Index(m)) if m.contains("hash")));
    assert!(r.st.head("pypi").unwrap().is_none());
}

#[test]
fn files_on_a_host_that_is_not_allowed_are_refused() {
    let mut r = rig();
    r.up.wheel("app", "1.0", &[]);
    r.up.edit("app", "app-1.0-py3-none-any.whl", |e| e["url"] = json!("http://files.elsewhere.example/app-1.0-py3-none-any.whl"));
    r.up.edit("app", "app-1.0-py3-none-any.whl", |e| {
        e.as_object_mut().unwrap().remove("core-metadata");
    });
    match r.sync() {
        Err(PyError::Fetch(e)) => assert!(e.to_string().contains("allowlist"), "{e}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn hostile_index_entries_are_ignored() {
    let mut r = rig();
    r.up.wheel("app", "1.0", &[]);
    let good = r.up.pages["app"][0].clone();
    for name in ["../../etc/passwd-1.0.tar.gz", "app-9.9-py3-none-any.whl\"><script>", "sub/dir-9.9.tar.gz", ".hidden-9.9.tar.gz"] {
        let mut evil = good.clone();
        evil["filename"] = json!(name);
        r.up.pages.get_mut("app").unwrap().push(evil);
    }
    let mut no_hash = good.clone();
    no_hash["filename"] = json!("app-8.0-py3-none-any.whl");
    no_hash["hashes"] = json!({ "md5": "abc" });
    r.up.pages.get_mut("app").unwrap().push(no_hash);
    r.up.publish();
    let rep = r.sync().unwrap();
    assert_eq!(versions(&rep), [("app", "1.0")]);
    assert!(rep.warnings.iter().any(|w| w.contains("app-8.0") && w.contains("SHA-256")), "{:?}", rep.warnings);
    for path in r.view().keys() {
        assert!(!path.contains("..") && !path.contains('<'), "{path}");
    }
}

#[test]
fn unchanged_upstream_gives_no_new_snapshot_and_updates_replace_old_versions() {
    let mut r = rig();
    r.up.wheel("app", "1.0", &["leaf"]).wheel("leaf", "0.1", &[]);
    assert_eq!(r.sync().unwrap().snapshot, Some(1));
    let again = r.sync().unwrap();
    assert_eq!((again.snapshot, again.downloaded, again.reused), (None, 0, 2));

    r.up.wheel("app", "1.1", &[]); // new release that no longer needs leaf
    let rep = r.sync().unwrap();
    // Number 2 was used up by the no-change sync above: numbers are never reused.
    assert_eq!((rep.snapshot, rep.downloaded), (Some(3), 1));
    assert_eq!(r.packages(), ["packages/app/app-1.1-py3-none-any.whl"]);
    assert!(!r.view().contains_key("simple/leaf/index.html"));
    // Snapshot 1 is intact for rollback.
    assert!(r.st.get_at("pypi", 1, "packages/leaf/leaf-0.1-py3-none-any.whl").unwrap().is_some());
}

#[test]
fn only_pypi_repos_and_json_indexes_are_accepted() {
    let mut r = rig();
    r.up.wheel("app", "1.0", &[]);
    r.st.create_repo("files", "raw").unwrap();
    let client = HttpClient::new(&[host_of(&r.up.base).unwrap()]);
    assert!(matches!(sync(&r.st, "files", &client, &r.cfg), Err(PyError::Config(_))));
    // An index that answers with HTML only (no PEP 691 support).
    r.up.routes.lock().unwrap().insert("/simple/app/".into(), b"<html><a href=x>x</a></html>".to_vec());
    assert!(matches!(r.sync(), Err(PyError::Index(m)) if m.contains("PEP 691")));
}
