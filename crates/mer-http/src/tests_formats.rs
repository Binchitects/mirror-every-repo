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

#[test]
fn npm_package_documents_point_at_this_server() {
    let doc = br#"{"name":"left","dist-tags":{"latest":"1.5.0"},"versions":{"1.5.0":{"dist":{"tarball":"mer://base/left/-/left-1.5.0.tgz"}}}}"#;
    let scoped = br#"{"name":"@types/node","versions":{"20.1.0":{"dist":{"tarball":"mer://base/@types/node/-/node-20.1.0.tgz"}}}}"#;
    let r = rig(
        "js",
        "npm",
        &[
            ("packuments/left.json", doc),
            ("tarballs/left/left-1.5.0.tgz", b"left tarball"),
            ("packuments/@types/node.json", scoped),
            ("tarballs/@types/node/node-20.1.0.tgz", b"types tarball"),
        ],
    );
    let host = r.base.trim_start_matches("http://").to_string();

    let (status, body, _, ct) = get(&r, "/js/left");
    assert_eq!((status, ct.as_deref()), (200, Some("application/json")));
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let url = parsed["versions"]["1.5.0"]["dist"]["tarball"].as_str().unwrap();
    assert_eq!(url, format!("http://{host}/js/left/-/left-1.5.0.tgz"));
    assert!(!body.contains("mer://"), "no placeholder reaches the client");
    // The URL npm was just given works.
    assert_eq!(get(&r, url.trim_start_matches(&r.base)).1, "left tarball");

    // Scoped names arrive percent-encoded from npm, or plain from other tools.
    for asked in ["/js/@types%2fnode", "/js/@types%2Fnode", "/js/@types/node"] {
        let (status, body, _, _) = get(&r, asked);
        assert_eq!(status, 200, "{asked}");
        assert!(body.contains(&format!("http://{host}/js/@types/node/-/node-20.1.0.tgz")), "{asked}");
    }
    assert_eq!(get(&r, "/js/@types/node/-/node-20.1.0.tgz").1, "types tarball");
    assert_eq!(get(&r, "/js/-/ping").0, 200);
}

#[test]
fn npm_requests_for_unmirrored_packages_become_wishes_and_services_do_not() {
    let r = rig("js", "npm", &[("packuments/left.json", b"{}")]);
    assert_eq!(get(&r, "/js/express").0, 404);
    assert_eq!(get(&r, "/js/express").0, 404);
    assert_eq!(get(&r, "/js/@babel%2fcore").0, 404);
    assert_eq!(get(&r, "/js/left/-/left-9.9.9.tgz").0, 404);
    // Registry services npm probes, nonsense names and traversal: refused, not recorded.
    for path in ["/js/-/npm/v1/security/advisories/bulk", "/js/-/v1/search", "/js/..%2f..%2fmeta.db", "/js/.hidden", "/js/a%20b", "/js/x/-/..%2f..%2fx.tgz", "/js/x/-/evil.sh"] {
        assert_eq!(get(&r, path).0, 404, "{path}");
    }
    assert_eq!(get(&r, "/js/%zz").0, 400);
    assert_eq!(
        wishes(&r),
        vec![("express".to_string(), 2), ("@babel/core".to_string(), 1), ("tarballs/left/left-9.9.9.tgz".to_string(), 1)]
    );
    // The stored documents are not reachable by their internal path.
    assert_eq!(get(&r, "/js/packuments/left.json").0, 404);
}

#[test]
fn a_forged_host_header_cannot_inject_into_the_document() {
    use std::io::{Read as _, Write as _};
    let doc = br#"{"versions":{"1.0.0":{"dist":{"tarball":"mer://base/left/-/left-1.0.0.tgz"}}}}"#;
    let r = rig("js", "npm", &[("packuments/left.json", doc)]);
    let addr = r.base.trim_start_matches("http://");
    for host in ["evil\",\"x\":\"y", "a b", "<script>", ""] {
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        write!(s, "GET /js/left HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        assert!(out.starts_with("HTTP/1.1 400"), "Host {host:?} gave: {}", out.lines().next().unwrap_or(""));
    }
}
