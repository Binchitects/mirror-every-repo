//! Format-specific client conventions on the serving side.
use crate::*;
use mer_core::EntryRole::Artifact;

struct Rig {
    _dir: tempfile::TempDir,
    st: Arc<Storage>,
    base: String,
    server: Option<HttpServer>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(s) = self.server.take() {
            s.shutdown();
        }
    }
}

fn rig(repo: &str, format: &str, files: &[(&str, &[u8])]) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let st = Arc::new(Storage::open(dir.path()).unwrap());
    st.create_repo(repo, format).unwrap();
    let mut w = st.begin(repo).unwrap();
    for (path, body) in files {
        let (d, _) = st.ingest_bytes(body).unwrap();
        w.put(path, Artifact, &d).unwrap();
    }
    let seq = w.seal(None).unwrap().unwrap().seq;
    st.decide(repo, seq, true, "t").unwrap();
    st.publish(repo, seq, "t").unwrap();
    let cfg = ServerConfig { listen: "127.0.0.1:0".into(), threads: 2, access_log: false, admin: false, ..Default::default() };
    let server = HttpServer::start(App::new(st.clone()), cfg).unwrap();
    let base = format!("http://{}", server.addr());
    Rig { _dir: dir, st, base, server: Some(server) }
}

/// (status, body, Location, Content-Type), without following redirects.
fn get(r: &Rig, path: &str) -> (u16, String, Option<String>, Option<String>) {
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let resp = match agent.get(&format!("{}{path}", r.base)).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("{path}: {e}"),
    };
    let (status, loc, ct) = (resp.status(), resp.header("Location").map(str::to_string), resp.header("Content-Type").map(str::to_string));
    (status, resp.into_string().unwrap_or_default(), loc, ct)
}

fn wishes(r: &Rig) -> Vec<(String, u64)> {
    r.st.meta().read(|t| t.miss_list(50)).unwrap().into_iter().map(|m| (m.path, m.count)).collect()
}

#[test]
fn pypi_simple_pages_are_found_the_way_pip_asks_for_them() {
    let r = rig(
        "py",
        "pypi",
        &[
            ("simple/index.html", b"<a href=\"zope-interface/\">zope-interface</a>"),
            ("simple/zope-interface/index.html", b"<a href=\"../../packages/zope-interface/z-1.0-py3-none-any.whl#sha256=ab\">z</a>"),
            ("packages/zope-interface/z-1.0-py3-none-any.whl", b"wheel bytes"),
        ],
    );
    let (status, body, _, ct) = get(&r, "/py/simple/zope-interface/");
    assert_eq!(status, 200);
    assert!(body.contains("z-1.0-py3-none-any.whl"));
    assert_eq!(ct.as_deref(), Some("text/html; charset=utf-8"));
    assert_eq!(get(&r, "/py/simple/").0, 200);

    // Any spelling of the name, with or without the slash, leads to the one page.
    for asked in ["/py/simple/zope-interface", "/py/simple/Zope.Interface/", "/py/simple/zope_interface", "/py/simple/ZOPE--interface/"] {
        let (status, _, location, _) = get(&r, asked);
        assert_eq!((status, location.as_deref()), (301, Some("/py/simple/zope-interface/")), "{asked}");
    }
    assert_eq!(get(&r, "/py/simple").2.as_deref(), Some("/py/simple/"));

    // The relative link on the page resolves to the file.
    assert_eq!(get(&r, "/py/packages/zope-interface/z-1.0-py3-none-any.whl").1, "wheel bytes");
    // The stored page itself is still reachable by its real path.
    assert_eq!(get(&r, "/py/simple/zope-interface/index.html").0, 200);
}

#[test]
fn asking_pip_for_a_project_that_is_not_mirrored_becomes_a_wish() {
    let r = rig("py", "pypi", &[("simple/index.html", b"")]);
    assert_eq!(get(&r, "/py/simple/numpy/").0, 404);
    assert_eq!(get(&r, "/py/simple/numpy/").0, 404);
    assert_eq!(get(&r, "/py/simple/Scikit_Learn/").0, 301);
    assert_eq!(get(&r, "/py/simple/scikit-learn/").0, 404);
    // Nonsense names are not recorded.
    assert_eq!(get(&r, "/py/simple/..%2f..%2fetc/").0, 404);
    assert_eq!(get(&r, "/py/simple/a%00b/").0, 400);
    assert_eq!(wishes(&r), vec![("simple/numpy".to_string(), 2), ("simple/scikit-learn".to_string(), 1)]);
}

#[test]
fn other_formats_are_not_affected_by_pypi_rules() {
    let r = rig("files", "raw", &[("simple/index.html", b"plain file")]);
    assert_eq!(get(&r, "/files/simple/index.html").1, "plain file");
    assert_eq!(get(&r, "/files/simple/").0, 404, "no directory indexes for raw repos");
    assert_eq!(get(&r, "/files/simple").0, 404);
}
