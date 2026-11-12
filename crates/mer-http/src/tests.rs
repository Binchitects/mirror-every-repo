use crate::*;
use mer_core::{Digest, EntryRole};
use mer_storage::Storage;
use std::io::Write;
use std::net::TcpStream;

struct Rig {
    dir: tempfile::TempDir,
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

fn big() -> Vec<u8> {
    (0..100_000u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect()
}

/// Seal a snapshot of `repo` from (path, role, body) and return its seq.
fn snapshot(st: &Storage, repo: &str, files: &[(&str, EntryRole, &[u8])]) -> u64 {
    let mut w = st.begin(repo).unwrap();
    for (path, role, body) in files {
        let (d, _) = st.ingest_bytes(body).unwrap();
        w.put(path, *role, &d).unwrap();
    }
    w.seal(None).unwrap().unwrap().seq
}

fn publish(st: &Storage, repo: &str, seq: u64) {
    st.decide(repo, seq, true, "test").unwrap();
    st.publish(repo, seq, "test").unwrap();
}

fn rig() -> Rig {
    use EntryRole::*;
    let dir = tempfile::tempdir().unwrap();
    let st = Arc::new(Storage::open(dir.path()).unwrap());
    st.create_repo("files", "raw").unwrap();
    let s = snapshot(
        &st,
        "files",
        &[
            ("a.txt", Artifact, b"hello\n"),
            ("pool/big.bin", Artifact, &big()),
            ("dir/with space.txt", Artifact, b"spaced"),
            ("literal%20name", Artifact, b"literal"),
        ],
    );
    publish(&st, "files", s);

    st.create_repo("debian", "apt").unwrap();
    let s = snapshot(
        &st,
        "debian",
        &[
            ("dists/bookworm/InRelease", Artifact, b"signed release"),
            ("pool/main/h/hello/hello_1.0_amd64.deb", Artifact, b"deb bytes"),
            ("evidence/bookworm/InRelease", Evidence, b"upstream evidence"),
        ],
    );
    publish(&st, "debian", s);

    st.create_repo("pending", "raw").unwrap();
    snapshot(&st, "pending", &[("secret.txt", Artifact, b"not approved yet")]);

    let cfg = ServerConfig { listen: "127.0.0.1:0".into(), threads: 4, access_log: false, admin: false, ..Default::default() };
    let server = HttpServer::start(App::new(st.clone()), cfg).unwrap();
    let base = format!("http://{}", server.addr());
    Rig { dir, st, base, server: Some(server) }
}

struct Resp {
    status: u16,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn request(method: &str, url: &str, headers: &[(&str, &str)]) -> Resp {
    let mut req = ureq::request(method, url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let resp = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => panic!("{method} {url}: {e}"),
    };
    let status = resp.status();
    let headers = resp
        .headers_names()
        .into_iter()
        .map(|n| {
            let v = resp.header(&n).unwrap_or("").to_string();
            (n, v)
        })
        .collect();
    let mut body = Vec::new();
    resp.into_reader().read_to_end(&mut body).unwrap();
    Resp { status, body, headers }
}

fn get(r: &Rig, path: &str) -> Resp {
    request("GET", &format!("{}{path}", r.base), &[])
}

/// Send a request line exactly as written (HTTP clients normalise paths).
fn raw_status(r: &Rig, target: &str) -> u16 {
    let addr = r.base.trim_start_matches("http://");
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "GET {target} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0)
}

fn misses(r: &Rig) -> Vec<(String, String, u64)> {
    r.st.meta().read(|t| t.miss_list(100)).unwrap().into_iter().map(|m| (m.repo, m.path, m.count)).collect()
}

#[test]
fn serves_published_files_with_useful_headers() {
    let r = rig();
    let resp = get(&r, "/files/a.txt");
    assert_eq!((resp.status, resp.body.as_slice()), (200, &b"hello\n"[..]));
    assert_eq!(resp.header("Content-Length"), Some("6"));
    assert_eq!(resp.header("Content-Type"), Some("text/plain; charset=utf-8"));
    assert_eq!(resp.header("ETag").unwrap(), format!("\"sha256:{}\"", Digest::of(b"hello\n")));
    assert_eq!(resp.header("Accept-Ranges"), Some("bytes"));
    assert_eq!(resp.header("X-Content-Type-Options"), Some("nosniff"));

    let resp = get(&r, "/files/pool/big.bin");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, big());
    assert_eq!(resp.header("Content-Length"), Some("100000"), "large files carry their size too");
    assert_eq!(resp.header("Transfer-Encoding"), None);
    assert_eq!(get(&r, "/debian/pool/main/h/hello/hello_1.0_amd64.deb").header("Content-Type"), Some("application/vnd.debian.binary-package"));
    assert_eq!(get(&r, "/files/a.txt?ignored=1").status, 200, "query strings are ignored");
}

#[test]
fn head_gives_headers_without_a_body() {
    let r = rig();
    let resp = request("HEAD", &format!("{}/files/pool/big.bin", r.base), &[]);
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("Content-Length"), Some("100000"));
    assert!(resp.body.is_empty());
}

#[test]
fn range_requests_allow_resuming_a_download() {
    let r = rig();
    let url = format!("{}/files/pool/big.bin", r.base);
    let data = big();
    let range = |spec: &str| request("GET", &url, &[("Range", spec)]);

    let resp = range("bytes=10-19");
    assert_eq!((resp.status, resp.body.as_slice()), (206, &data[10..20]));
    assert_eq!(resp.header("Content-Range"), Some("bytes 10-19/100000"));
    assert_eq!(resp.header("Content-Length"), Some("10"));

    let resp = range("bytes=99990-");
    assert_eq!((resp.status, resp.body.as_slice()), (206, &data[99_990..]));
    let resp = range("bytes=-5");
    assert_eq!((resp.status, resp.body.as_slice()), (206, &data[99_995..]));
    assert_eq!(resp.header("Content-Range"), Some("bytes 99995-99999/100000"));
    let resp = range("bytes=50000-999999999");
    assert_eq!((resp.status, resp.body.len()), (206, 50_000), "end is clamped to the file");

    for bad in ["bytes=100000-", "bytes=5-2", "bytes=abc-", "bytes=-0"] {
        let resp = range(bad);
        assert_eq!(resp.status, 416, "{bad}");
        assert_eq!(resp.header("Content-Range"), Some("bytes */100000"));
    }
    // Forms this server does not implement fall back to the whole file.
    assert_eq!(range("bytes=0-1,5-6").status, 200);
    assert_eq!(range("lines=1-2").status, 200);
}

#[test]
fn conditional_request_avoids_resending() {
    let r = rig();
    let url = format!("{}/files/a.txt", r.base);
    let etag = get(&r, "/files/a.txt").header("ETag").unwrap().to_string();
    let resp = request("GET", &url, &[("If-None-Match", &etag)]);
    assert_eq!((resp.status, resp.body.len()), (304, 0));
    assert_eq!(request("GET", &url, &[("If-None-Match", "\"sha256:other\"")]).status, 200);
}

#[test]
fn only_the_published_snapshot_is_reachable() {
    use EntryRole::Artifact;
    let r = rig();
    assert_eq!(get(&r, "/pending/secret.txt").status, 404, "sealed but never published");
    assert_eq!(get(&r, "/nosuchrepo/a.txt").status, 404);

    // A newer snapshot waits in quarantine: clients keep seeing the old one.
    let s2 = snapshot(&r.st, "files", &[("a.txt", Artifact, b"version two\n"), ("new.txt", Artifact, b"new")]);
    assert_eq!(get(&r, "/files/a.txt").body, b"hello\n");
    assert_eq!(get(&r, "/files/new.txt").status, 404);

    publish(&r.st, "files", s2);
    assert_eq!(get(&r, "/files/a.txt").body, b"version two\n");
    assert_eq!(get(&r, "/files/new.txt").status, 200);

    r.st.rollback("files", 1, "test").unwrap();
    assert_eq!(get(&r, "/files/a.txt").body, b"hello\n");
    assert_eq!(get(&r, "/files/new.txt").status, 404, "rolled-back content is withdrawn");
}

#[test]
fn path_tricks_never_escape_the_snapshot() {
    let r = rig();
    // The data directory holds meta.db and the blob store next to nothing else;
    // none of these may ever return content.
    for target in [
        "/files/../meta.db",
        "/files/../../etc/passwd",
        "/files/%2e%2e/meta.db",
        "/files/%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        "/files/..%2fmeta.db",
        "/files//a.txt",
        "/files/./a.txt",
        "/files/a.txt/",
        "/files/dir\\a.txt",
        "/files/%00",
        "/files/%zz",
        "//files/a.txt",
        "/../files/a.txt",
        "/files",
        "files/a.txt",
    ] {
        let status = raw_status(&r, target);
        assert!(matches!(status, 400 | 404), "{target} gave {status}");
    }
    assert!(r.dir.path().join("meta.db").exists());
    assert_eq!(raw_status(&r, "/files/a.txt"), 200, "the server is still healthy");
}

#[test]
fn encoded_and_literal_names_both_resolve() {
    let r = rig();
    assert_eq!(get(&r, "/files/dir/with%20space.txt").body, b"spaced");
    // A stored name that itself contains "%20" is found as sent.
    assert_eq!(raw_status(&r, "/files/literal%20name"), 200);
}

#[test]
fn internal_metadata_is_not_served() {
    let r = rig();
    assert_eq!(get(&r, "/debian/evidence/bookworm/InRelease").status, 404);
    assert_eq!(get(&r, "/debian/dists/bookworm/InRelease").status, 200);
}

#[test]
fn misses_become_wishes_only_for_real_content() {
    let r = rig();
    assert_eq!(get(&r, "/files/wanted-2.0.tar.gz").status, 404);
    assert_eq!(get(&r, "/files/wanted-2.0.tar.gz").status, 404);
    // apt probes for optional indices all the time; those are not wishes.
    assert_eq!(get(&r, "/debian/dists/bookworm/main/i18n/Translation-en.xz").status, 404);
    assert_eq!(get(&r, "/debian/dists/bookworm/main/binary-amd64/Packages.xz").status, 404);
    assert_eq!(get(&r, "/debian/pool/main/c/curl/curl_8.0_amd64.deb").status, 404);
    // Not published, unknown repo, or unsafe path: nothing recorded.
    assert_eq!(get(&r, "/pending/x").status, 404);
    assert_eq!(get(&r, "/nosuchrepo/x").status, 404);
    raw_status(&r, "/files/../x");
    assert_eq!(
        misses(&r),
        vec![
            ("files".to_string(), "wanted-2.0.tar.gz".to_string(), 2),
            ("debian".to_string(), "pool/main/c/curl/curl_8.0_amd64.deb".to_string(), 1),
        ]
    );
}

#[test]
fn other_methods_and_service_pages() {
    let r = rig();
    let resp = request("POST", &format!("{}/files/a.txt", r.base), &[]);
    assert_eq!((resp.status, resp.header("Allow")), (405, Some("GET, HEAD")));
    assert_eq!(request("DELETE", &format!("{}/files/a.txt", r.base), &[]).status, 405);

    assert_eq!(get(&r, "/healthz").body, b"ok\n");
    let index = String::from_utf8(get(&r, "/").body).unwrap();
    assert!(index.contains("/files/  raw  snapshot 1") && index.contains("/pending/  raw  nothing published"), "{index}");

    get(&r, "/files/a.txt");
    get(&r, "/files/missing");
    let m = String::from_utf8(get(&r, "/metrics").body).unwrap();
    for needle in [
        "mer_http_requests_total{code=\"200\"}",
        "mer_http_requests_total{code=\"404\"} 1",
        "mer_http_requests_total{code=\"405\"} 2",
        "mer_http_misses_total 1",
        "mer_repo_live_snapshot{repo=\"files\"} 1",
        "mer_repo_live_snapshot{repo=\"pending\"} 0",
        "mer_store_blobs ",
    ] {
        assert!(m.contains(needle), "metrics lack {needle}:\n{m}");
    }
}

#[test]
fn many_clients_at_once() {
    let r = rig();
    let want = big();
    std::thread::scope(|scope| {
        for t in 0..16 {
            let (base, want) = (&r.base, &want);
            scope.spawn(move || {
                for i in 0..10 {
                    if (t + i) % 3 == 0 {
                        let resp = request("GET", &format!("{base}/files/pool/big.bin"), &[("Range", "bytes=1000-1999")]);
                        assert_eq!((resp.status, resp.body.as_slice()), (206, &want[1000..2000]));
                    } else {
                        let resp = request("GET", &format!("{base}/files/pool/big.bin"), &[]);
                        assert_eq!(resp.status, 200);
                        assert!(resp.body == *want, "body differs under concurrency");
                    }
                }
            });
        }
    });
}

#[test]
fn a_lost_blob_is_an_error_response_not_a_crash() {
    let r = rig();
    let hex = Digest::of(b"hello\n").to_hex();
    let path = r.dir.path().join("store/blobs/sha256").join(&hex[0..2]).join(&hex[2..4]).join(&hex);
    std::fs::remove_file(path).unwrap();
    assert_eq!(get(&r, "/files/a.txt").status, 500);
    assert_eq!(get(&r, "/files/pool/big.bin").status, 200, "other content is unaffected");
}
