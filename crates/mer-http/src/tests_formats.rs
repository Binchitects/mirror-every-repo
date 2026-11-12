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

/// A registry repo holding one image (index + one platform manifest + two blobs).
fn oci_rig() -> (Rig, String, String, String) {
    use mer_core::Digest;
    let config = br#"{"architecture":"amd64"}"#.to_vec();
    let layer = vec![9u8; 50_000];
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{{"digest":"sha256:{}","size":{}}},"layers":[{{"digest":"sha256:{}","size":{}}}]}}"#,
        Digest::of(&config), config.len(), Digest::of(&layer), layer.len()
    )
    .into_bytes();
    let m = Digest::of(&manifest).to_hex();
    // An index without a mediaType field, as some builders write them.
    let index = format!(r#"{{"schemaVersion":2,"manifests":[{{"digest":"sha256:{m}","platform":{{"os":"linux","architecture":"amd64"}}}}]}}"#).into_bytes();
    let i = Digest::of(&index).to_hex();
    let tag = format!("sha256:{i}\n").into_bytes();
    let (c, l) = (Digest::of(&config).to_hex(), Digest::of(&layer).to_hex());
    let r = rig(
        "hub",
        "oci",
        &[
            (&format!("manifests/library/app/sha256/{i}"), &index),
            (&format!("manifests/library/app/sha256/{m}"), &manifest),
            ("tags/library/app/1.0", &tag),
            ("tags/library/app/stable", &tag),
            (&format!("blobs/sha256/{c}"), &config),
            (&format!("blobs/sha256/{l}"), &layer),
        ],
    );
    (r, i, m, l)
}

fn full(r: &Rig, method: &str, path: &str, headers: &[(&str, &str)]) -> (u16, Vec<u8>, Vec<(String, String)>) {
    let mut req = ureq::AgentBuilder::new().redirects(0).build().request(method, &format!("{}{path}", r.base));
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("{path}: {e}"),
    };
    let status = resp.status();
    let hs = resp.headers_names().into_iter().map(|n| (n.to_ascii_lowercase(), resp.header(&n).unwrap_or("").to_string())).collect();
    let mut body = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader(), &mut body).unwrap();
    (status, body, hs)
}

fn hdr<'a>(hs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    hs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

#[test]
fn registry_api_serves_an_image_the_way_container_clients_pull_it() {
    let (r, index, manifest, layer) = oci_rig();
    // 1. Ping.
    let (status, _, hs) = full(&r, "GET", "/v2/", &[]);
    assert_eq!((status, hdr(&hs, "docker-distribution-api-version")), (200, Some("registry/2.0")));

    // 2. Resolve the tag: a HEAD is enough to learn the digest.
    let (status, body, hs) = full(&r, "HEAD", "/v2/hub/library/app/manifests/1.0", &[]);
    assert_eq!((status, body.len()), (200, 0));
    assert_eq!(hdr(&hs, "docker-content-digest"), Some(format!("sha256:{index}").as_str()));
    assert_eq!(hdr(&hs, "content-type"), Some("application/vnd.oci.image.index.v1+json"), "inferred for an index without mediaType");

    // 3. Fetch the index by tag and by digest: identical bytes that hash to the digest.
    let (_, by_tag, _) = full(&r, "GET", "/v2/hub/library/app/manifests/1.0", &[]);
    let (_, by_digest, _) = full(&r, "GET", &format!("/v2/hub/library/app/manifests/sha256:{index}"), &[]);
    assert_eq!(by_tag, by_digest);
    assert_eq!(mer_core::Digest::of(&by_tag).to_hex(), index);

    // 4. The platform manifest, with the media type it declares.
    let (status, body, hs) = full(&r, "GET", &format!("/v2/hub/library/app/manifests/sha256:{manifest}"), &[]);
    assert_eq!(status, 200);
    assert_eq!(hdr(&hs, "content-type"), Some("application/vnd.docker.distribution.manifest.v2+json"));
    assert_eq!(mer_core::Digest::of(&body).to_hex(), manifest);

    // 5. Layers, including a resumed download.
    let (status, body, hs) = full(&r, "GET", &format!("/v2/hub/library/app/blobs/sha256:{layer}"), &[]);
    assert_eq!((status, body.len()), (200, 50_000));
    assert_eq!(hdr(&hs, "content-length"), Some("50000"));
    assert_eq!(mer_core::Digest::of(&body).to_hex(), layer);
    let (status, body, _) = full(&r, "GET", &format!("/v2/hub/library/app/blobs/sha256:{layer}"), &[("Range", "bytes=49990-")]);
    assert_eq!((status, body.len()), (206, 10));

    // 6. Tag listing.
    let (_, body, _) = full(&r, "GET", "/v2/hub/library/app/tags/list", &[]);
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&body).unwrap(), serde_json::json!({ "name": "library/app", "tags": ["1.0", "stable"] }));
}

#[test]
fn registry_api_refuses_what_it_should() {
    let (r, index, _, layer) = oci_rig();
    for path in [
        "/v2/hub/library/app/manifests/2.0".to_string(),
        "/v2/hub/library/other/manifests/1.0".to_string(),
        format!("/v2/hub/library/app/manifests/sha256:{}", "0".repeat(64)),
        format!("/v2/hub/library/app/blobs/sha256:{}", "0".repeat(64)),
        "/v2/hub/library/app/blobs/md5:abc".to_string(),
        "/v2/hub/library/app/manifests/sha256:short".to_string(),
        "/v2/hub/Library/App/manifests/1.0".to_string(),
        "/v2/hub/library/../../meta.db/manifests/1.0".to_string(),
        "/v2/hub/library/app/tags/../../blobs".to_string(),
        "/v2/hub/nothing/tags/list".to_string(),
        "/v2/nosuchrepo/library/app/manifests/1.0".to_string(),
        "/v2/hub".to_string(),
        "/v2/_catalog".to_string(),
    ] {
        let (status, body, _) = full(&r, "GET", &path, &[]);
        assert_eq!(status, 404, "{path}");
        assert!(!body.windows(6).any(|w| w == b"SQLite"), "{path}");
    }
    // Images are not reachable outside the registry API, and the internal
    // layout is not a public path.
    assert_eq!(full(&r, "GET", "/hub/library/app/manifests/1.0", &[]).0, 404);
    assert_eq!(full(&r, "GET", &format!("/hub/blobs/sha256/{layer}"), &[]).0, 404);
    assert_eq!(full(&r, "GET", &format!("/v2/hub/manifests/library/app/sha256/{index}"), &[]).0, 404);
    assert_eq!(full(&r, "GET", "/v2/hub/tags/library/app/1.0", &[]).0, 404);
    // Only a tag that is not mirrored is a wish; unknown digests and blobs are not.
    assert_eq!(wishes(&r), vec![("images/library/app:2.0".to_string(), 1), ("images/library/other:1.0".to_string(), 1)]);
    // Pushing is not offered.
    assert_eq!(full(&r, "POST", "/v2/hub/library/app/blobs/uploads/", &[]).0, 405);
    assert_eq!(full(&r, "PUT", "/v2/hub/library/app/manifests/9.9", &[]).0, 405);
}

#[test]
fn other_repositories_are_not_reachable_through_the_registry_api() {
    let r = rig("files", "raw", &[("a.txt", b"plain")]);
    assert_eq!(full(&r, "GET", "/files/a.txt", &[]).0, 200);
    assert_eq!(full(&r, "GET", "/v2/files/a.txt", &[]).0, 404);
    assert!(r.st.create_repo("v2", "raw").is_err(), "the name is reserved");
}

#[test]
fn private_image_repositories_ask_for_credentials_at_the_ping() {
    use base64::Engine;
    let (r, index, _, _) = oci_rig();
    let app_auth = mer_auth::Auth::new(r.st.clone());
    let id = r.st.repo("hub").unwrap().id;
    r.st.meta().write(|t| t.repo_set_private(id, true)).unwrap();
    let secret = app_auth.create_token("puller", Some("hub"), "test").unwrap();

    // Without credentials the ping already says so, which is when clients decide to log in.
    let (status, _, hs) = full(&r, "GET", "/v2/", &[]);
    assert_eq!(status, 401);
    assert!(hdr(&hs, "www-authenticate").unwrap().starts_with("Basic"));
    assert_eq!(full(&r, "GET", "/v2/hub/library/app/manifests/1.0", &[]).0, 401);

    // What `docker login` sends: Basic with any user name and the token as password.
    let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("anything:{secret}")));
    assert_eq!(full(&r, "GET", "/v2/", &[("Authorization", &basic)]).0, 200);
    let (status, _, hs) = full(&r, "HEAD", "/v2/hub/library/app/manifests/1.0", &[("Authorization", &basic)]);
    assert_eq!((status, hdr(&hs, "docker-content-digest")), (200, Some(format!("sha256:{index}").as_str())));
    let wrong = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("anything:mer_wrong"));
    assert_eq!(full(&r, "GET", "/v2/hub/library/app/manifests/1.0", &[("Authorization", &wrong)]).0, 401);
}

#[test]
fn nuget_feed_is_found_the_way_dotnet_asks_for_it() {
    let index = br#"{"version":"3.0.0","resources":[{"@id":"mer://base/flat/","@type":"PackageBaseAddress/3.0.0"}]}"#;
    let r = rig(
        "dn",
        "nuget",
        &[
            ("v3/index.json", index),
            ("flat/newtonsoft.json/index.json", br#"{"versions":["13.0.3"]}"#),
            ("flat/newtonsoft.json/13.0.3/newtonsoft.json.13.0.3.nupkg", b"nupkg bytes"),
        ],
    );
    let host = r.base.trim_start_matches("http://").to_string();

    // The service index names the package location on this very server.
    let (status, body, _, ct) = get(&r, "/dn/v3/index.json");
    assert_eq!((status, ct.as_deref()), (200, Some("application/json")));
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    let flat = doc["resources"][0]["@id"].as_str().unwrap().to_string();
    assert_eq!(flat, format!("http://{host}/dn/flat/"));
    assert!(!body.contains("mer://"));

    // Following it: version list, then the package. Clients may send any case.
    let rel = flat.trim_start_matches(&r.base).to_string();
    assert_eq!(get(&r, &format!("{rel}newtonsoft.json/index.json")).1, r#"{"versions":["13.0.3"]}"#);
    assert_eq!(get(&r, &format!("{rel}newtonsoft.json/13.0.3/newtonsoft.json.13.0.3.nupkg")).1, "nupkg bytes");
    assert_eq!(get(&r, "/dn/flat/Newtonsoft.Json/index.json").0, 200);
    assert_eq!(get(&r, "/dn/flat/NEWTONSOFT.JSON/13.0.3/Newtonsoft.Json.13.0.3.nupkg").1, "nupkg bytes");
    assert_eq!(get(&r, "/dn/V3/index.json").0, 200);
}

#[test]
fn nuget_requests_for_unmirrored_packages_become_wishes() {
    let r = rig("dn", "nuget", &[("v3/index.json", b"{}"), ("flat/a/index.json", br#"{"versions":["1.0.0"]}"#)]);
    assert_eq!(get(&r, "/dn/flat/dapper/index.json").0, 404);
    assert_eq!(get(&r, "/dn/flat/Dapper/index.json").0, 404);
    assert_eq!(get(&r, "/dn/flat/a/2.0.0/a.2.0.0.nupkg").0, 404);
    for path in ["/dn/flat/..%2f..%2fmeta.db/index.json", "/dn/flat/a..b/index.json", "/dn/flat/index.json", "/dn/search?q=x", "/dn/flat/a/1.0.0/../../../../meta.db"] {
        assert_eq!(get(&r, path).0, 404, "{path}");
    }
    assert_eq!(wishes(&r), vec![("dapper".to_string(), 2), ("a/2.0.0".to_string(), 1)]);
}
