//! Where the collector reads upstream repositories from: an HTTP(S) mirror
//! or a directory holding a copy of one.
//!
//! Nothing read through here is trusted. Callers verify signatures and
//! hashes; this crate only bounds what can be asked for and how much is read.
mod client;

pub use client::{host_of, HttpClient, HttpResponse, Proxies};

use mer_core::validate_path;
use std::io::Read;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("{0}: not found upstream")]
    NotFound(String),
    #[error("{path}: upstream answered HTTP {status}")]
    Http { path: String, status: u16 },
    #[error("{path}: {reason}")]
    Transport { path: String, reason: String },
    #[error("{path}: larger than the {limit} bytes allowed")]
    TooLarge { path: String, limit: u64 },
    #[error("refusing upstream path {0:?}")]
    BadPath(String),
    #[error("{0}")]
    BadUpstream(String),
}

pub trait Upstream: Send + Sync {
    /// Open `path`, relative to the upstream root.
    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>, FetchError>;
    /// For log lines and audit entries.
    fn describe(&self) -> String;

    /// Read a whole file, refusing anything longer than `limit`.
    fn read(&self, path: &str, limit: u64) -> Result<Vec<u8>, FetchError> {
        let mut out = Vec::new();
        self.open(path)?
            .take(limit + 1)
            .read_to_end(&mut out)
            .map_err(|e| FetchError::Transport { path: path.into(), reason: e.to_string() })?;
        if out.len() as u64 > limit {
            return Err(FetchError::TooLarge { path: path.into(), limit });
        }
        Ok(out)
    }
}

fn check(path: &str) -> Result<(), FetchError> {
    validate_path(path).map_err(|_| FetchError::BadPath(path.to_string()))
}

/// A mirror on local disk (for example one synced by another tool).
pub struct DirUpstream {
    root: PathBuf,
}

impl DirUpstream {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, FetchError> {
        let root = root.into();
        if !root.is_dir() {
            return Err(FetchError::BadUpstream(format!("{} is not a directory", root.display())));
        }
        Ok(DirUpstream { root })
    }
}

impl Upstream for DirUpstream {
    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>, FetchError> {
        check(path)?;
        match std::fs::File::open(self.root.join(path)) {
            Ok(f) => Ok(Box::new(f)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(FetchError::NotFound(path.into())),
            Err(e) => Err(FetchError::Transport { path: path.into(), reason: e.to_string() }),
        }
    }
    fn describe(&self) -> String {
        self.root.display().to_string()
    }
}

/// A remote mirror. Redirects are followed within its own host, and to the
/// extra hosts it is given.
pub struct HttpUpstream {
    base: String,
    client: HttpClient,
}

impl HttpUpstream {
    pub fn new(base: &str) -> Result<Self, FetchError> {
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(FetchError::BadUpstream(format!("{base:?} is not an http(s) URL")));
        }
        Self::with_hosts(base, &[])
    }

    pub fn with_hosts(base: &str, extra: &[String]) -> Result<Self, FetchError> {
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(FetchError::BadUpstream(format!("{base:?} is not an http(s) URL")));
        }
        let host = host_of(base).ok_or_else(|| FetchError::BadUpstream(format!("{base:?} is not an http(s) URL")))?;
        let mut hosts = extra.to_vec();
        hosts.push(host);
        Ok(HttpUpstream { base: base.trim_end_matches('/').to_string(), client: HttpClient::new(&hosts) })
    }
}

impl Upstream for HttpUpstream {
    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>, FetchError> {
        check(path)?;
        // Only characters that are safe in a URL path are sent as they are.
        if !path.bytes().all(|b| b.is_ascii_alphanumeric() || b"/-._~+%=@:,".contains(&b)) {
            return Err(FetchError::BadPath(path.to_string()));
        }
        match self.client.get(&format!("{}/{}", self.base, path), &[]) {
            Ok(resp) => Ok(resp.into_reader()),
            Err(FetchError::NotFound(_)) => Err(FetchError::NotFound(path.into())),
            Err(FetchError::Http { status, .. }) => Err(FetchError::Http { path: path.into(), status }),
            Err(e) => Err(e),
        }
    }
    fn describe(&self) -> String {
        self.base.clone()
    }
}

/// `http://...` or `https://...` is a remote mirror; anything else is a directory.
pub fn upstream_from(spec: &str) -> Result<Box<dyn Upstream>, FetchError> {
    upstream_with_hosts(spec, &[])
}

/// As [`upstream_from`], allowing redirects to `extra` hosts too.
pub fn upstream_with_hosts(spec: &str, extra: &[String]) -> Result<Box<dyn Upstream>, FetchError> {
    if spec.starts_with("http://") || spec.starts_with("https://") {
        Ok(Box::new(HttpUpstream::with_hosts(spec, extra)?))
    } else {
        Ok(Box::new(DirUpstream::new(spec)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::resolve;

    fn mirror() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("dists/stable")).unwrap();
        std::fs::write(dir.path().join("dists/stable/Release"), b"Suite: stable\n").unwrap();
        std::fs::write(dir.path().join("big"), vec![7u8; 5000]).unwrap();
        dir
    }

    /// Serve a directory over HTTP on a random local port.
    fn serve(root: PathBuf) -> String {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let rel = req.url().trim_start_matches('/').to_string();
                let resp = match rel.as_str() {
                    "boom" => tiny_http::Response::from_string("no").with_status_code(503).boxed(),
                    _ => match std::fs::File::open(root.join(&rel)) {
                        Ok(f) => tiny_http::Response::from_file(f).boxed(),
                        Err(_) => tiny_http::Response::from_string("nope").with_status_code(404).boxed(),
                    },
                };
                let _ = req.respond(resp);
            }
        });
        format!("http://{addr}")
    }

    fn exercise(up: &dyn Upstream) {
        assert_eq!(up.read("dists/stable/Release", 1000).unwrap(), b"Suite: stable\n");
        assert!(matches!(up.read("dists/stable/Missing", 1000), Err(FetchError::NotFound(_))));
        assert_eq!(up.read("big", 5000).unwrap().len(), 5000);
        assert!(matches!(up.read("big", 4999), Err(FetchError::TooLarge { limit: 4999, .. })));
        for bad in ["../secret", "/etc/passwd", "a//b", "dists/../../x", ""] {
            assert!(matches!(up.open(bad), Err(FetchError::BadPath(_))), "{bad:?}");
        }
    }

    #[test]
    fn directory_upstream() {
        let dir = mirror();
        std::fs::write(dir.path().parent().unwrap().join("secret"), b"x").ok();
        exercise(&DirUpstream::new(dir.path()).unwrap());
        assert!(DirUpstream::new(dir.path().join("nope")).is_err());
    }

    #[test]
    fn http_upstream() {
        let dir = mirror();
        let base = serve(dir.path().to_path_buf());
        let up = HttpUpstream::new(&format!("{base}/")).unwrap();
        exercise(&up);
        assert!(matches!(up.read("boom", 10), Err(FetchError::Http { status: 503, .. })));
        assert!(matches!(up.open("a b"), Err(FetchError::BadPath(_))), "no raw spaces in URLs");
        assert!(matches!(up.open("x?y=1"), Err(FetchError::BadPath(_))), "no query strings");
        assert_eq!(up.describe(), base);
    }

    #[test]
    fn unreachable_host_is_an_error_not_a_hang() {
        let up = HttpUpstream::new("http://127.0.0.1:1").unwrap();
        assert!(matches!(up.read("x", 10), Err(FetchError::Transport { .. })));
    }

    #[test]
    fn client_only_contacts_allowed_hosts() {
        let dir = mirror();
        let base = serve(dir.path().to_path_buf());
        let host = host_of(&base).unwrap();
        let client = HttpClient::new(std::slice::from_ref(&host));
        let resp = client.get(&format!("{base}/dists/stable/Release"), &[("Accept", "text/plain")]).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.bytes("Release", 100).unwrap(), b"Suite: stable
");
        assert!(matches!(client.get(&format!("{base}/missing"), &[]), Err(FetchError::NotFound(_))));
        assert!(matches!(client.get(&format!("{base}/boom"), &[]), Err(FetchError::Http { status: 503, .. })));
        assert_eq!(client.get_any(&format!("{base}/boom"), &[]).unwrap().status, 503);
        assert!(matches!(client.get(&format!("{base}/big"), &[]).unwrap().bytes("big", 10), Err(FetchError::TooLarge { .. })));

        // Same server, but reached under a name that is not on the list.
        let other = base.replace("127.0.0.1", "localhost");
        assert!(matches!(client.get(&format!("{other}/big"), &[]), Err(FetchError::BadUpstream(_))));
        for bad in ["ftp://x/y", "http://user:pw@127.0.0.1/x", "http:///x", "not a url", "http://evil.example/x"] {
            assert!(matches!(client.get(bad, &[]), Err(FetchError::BadUpstream(_))), "{bad}");
        }
        assert!(matches!(client.get(&format!("{base}/a b"), &[]), Err(FetchError::BadPath(_))));
        assert_eq!(host_of("HTTPS://PyPI.org/simple/x/").as_deref(), None, "scheme must be lower-case");
        assert_eq!(host_of("https://PyPI.org:443/simple/x/?q#f").as_deref(), Some("pypi.org:443"));
    }

    /// A server whose answers are redirects: `/to?<location>` answers 302 to
    /// `<location>`, `/loop` redirects to itself, `/auth` echoes the
    /// Authorization header it got.
    fn redirector() -> String {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let url = req.url().to_string();
                let redirect = |loc: &str| {
                    tiny_http::Response::from_string("").with_status_code(302).with_header(tiny_http::Header::from_bytes("Location", loc).unwrap()).boxed()
                };
                let resp = if let Some(loc) = url.strip_prefix("/to?") {
                    redirect(loc)
                } else if url == "/nolocation" {
                    tiny_http::Response::from_string("").with_status_code(302).boxed()
                } else if url == "/loop" {
                    redirect("/loop")
                } else if url == "/auth" {
                    let got = req.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.to_string()).unwrap_or_default();
                    tiny_http::Response::from_string(format!("auth={got}")).boxed()
                } else {
                    tiny_http::Response::from_string("plain").boxed()
                };
                let _ = req.respond(resp);
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn redirects_are_followed_only_to_allowed_hosts() {
        let dir = mirror();
        let files = serve(dir.path().to_path_buf());
        let hops = redirector();
        let client = HttpClient::new(&[host_of(&files).unwrap(), host_of(&hops).unwrap()]);
        let read = |url: &str| client.get(url, &[]).and_then(|r| r.bytes(url, 1000));

        // Absolute, root-relative and relative locations, within the allowlist.
        assert_eq!(read(&format!("{hops}/to?{files}/dists/stable/Release")).unwrap(), b"Suite: stable\n");
        assert_eq!(read(&format!("{hops}/to?/plain")).unwrap(), b"plain");
        assert_eq!(read(&format!("{hops}/a/b/to?c")).unwrap(), b"plain", "relative to /a/b/");

        // To a host that is not on the list: refused before it is contacted, naming the host.
        let elsewhere = files.replace("127.0.0.1", "localhost");
        match read(&format!("{hops}/to?{elsewhere}/big")) {
            Err(FetchError::BadUpstream(m)) => assert!(m.contains("--allow-host localhost"), "{m}"),
            other => panic!("{other:?}"),
        }
        // A loop ends.
        assert!(matches!(read(&format!("{hops}/loop")), Err(FetchError::BadUpstream(m)) if m.contains("redirects")));
        // No location, or a downgrade from https, is refused.
        assert!(matches!(read(&format!("{hops}/nolocation")), Err(FetchError::BadUpstream(m)) if m.contains("Location")));
    assert_eq!(resolve("https://a.example/x", "  "), None);
        assert_eq!(resolve("https://a.example/x/y", "http://b.example/z").as_deref(), Some("http://b.example/z"));
        assert_eq!(resolve("https://a.example/x/y?q", "//b.example/z#f").as_deref(), Some("https://b.example/z"));
        assert_eq!(resolve("https://a.example", "z").as_deref(), Some("https://a.example/z"));

        // Credentials stay with the host they were meant for.
        let auth = [("Authorization", "Bearer secret")];
        let same = client.get(&format!("{hops}/to?/auth"), &auth).unwrap().bytes("auth", 100).unwrap();
        assert_eq!(same, b"auth=Bearer secret");
        let both = HttpClient::new(&[host_of(&hops).unwrap(), host_of(&hops.replace("127.0.0.1", "localhost")).unwrap()]);
        let other = hops.replace("127.0.0.1", "localhost");
        let crossed = both.get(&format!("{hops}/to?{other}/auth"), &auth).unwrap().bytes("auth", 100).unwrap();
        assert_eq!(crossed, b"auth=", "not sent to another host");

        // An apt-style upstream follows redirects within its own host only.
        let up = HttpUpstream::new(&hops).unwrap();
        assert_eq!(up.read("to?/plain", 100).ok(), None, "query strings are refused before any request");
        assert!(matches!(up.read("loop", 100), Err(FetchError::BadUpstream(_))));
    }

    #[test]
    fn requests_go_through_the_configured_proxy_and_the_allowlist_still_applies() {
        // A plain HTTP proxy: it receives the absolute URL and answers itself.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let proxy = format!("http://{}", server.server_addr().to_ip().unwrap());
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let body = format!("proxied {}", req.url());
                let _ = req.respond(tiny_http::Response::from_string(body));
            }
        });
        let hosts = ["upstream.example".to_string(), "direct.example".to_string()];
        let p = Proxies { http: Some(proxy.clone()), https: None, no_proxy: vec![".direct.example".into(), "direct.example".into()] };
        let client = HttpClient::with_proxies(&hosts, &p).unwrap();
        let got = client.get("http://upstream.example/x/y", &[]).unwrap().bytes("x", 100).unwrap();
        assert_eq!(String::from_utf8(got).unwrap(), "proxied http://upstream.example/x/y");
        assert_eq!(client.proxy_for("http://upstream.example/a"), Some(proxy.as_str()));
        assert_eq!(client.proxy_for("http://direct.example/a"), None, "NO_PROXY");
        assert_eq!(client.proxy_for("http://a.direct.example:8080/a"), None, "NO_PROXY suffix");
        assert_eq!(client.proxy_for("https://upstream.example/a"), None, "no https proxy set");
        // The destination host must still be allowed, proxy or not.
        assert!(matches!(client.get("http://elsewhere.example/x", &[]), Err(FetchError::BadUpstream(_))));
        assert!(HttpClient::with_proxies(&hosts, &Proxies { https: Some("::not a url".into()), ..Default::default() }).is_err());
    }

    #[test]
    fn transient_failures_are_retried() {
        // Answers 503 twice, then 200; and 502 once for /flaky.
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        std::thread::spawn(move || {
            let (mut busy, mut dropped) = (0, 0);
            for req in server.incoming_requests() {
                match req.url() {
                    "/busy" if busy < 2 => {
                        busy += 1;
                        let _ = req.respond(tiny_http::Response::from_string("later").with_status_code(503));
                    }
                    "/flaky" if dropped < 1 => {
                        dropped += 1;
                        let _ = req.respond(tiny_http::Response::from_string("bad gateway").with_status_code(502));
                    }
                    "/gone" => {
                        let _ = req.respond(tiny_http::Response::from_string("no").with_status_code(404));
                    }
                    _ => {
                        let _ = req.respond(tiny_http::Response::from_string("ok"));
                    }
                }
            }
        });
        let client = HttpClient::new(&[host_of(&base).unwrap()]).without_retry_delay();
        assert_eq!(client.get(&format!("{base}/busy"), &[]).unwrap().bytes("busy", 10).unwrap(), b"ok");
        assert_eq!(client.get(&format!("{base}/flaky"), &[]).unwrap().bytes("flaky", 10).unwrap(), b"ok");
        assert!(matches!(client.get(&format!("{base}/gone"), &[]), Err(FetchError::NotFound(_))), "a 404 is an answer, not retried");
        assert!(matches!(HttpClient::new(&["127.0.0.1:1".into()]).without_retry_delay().get("http://127.0.0.1:1/x", &[]), Err(FetchError::Transport { .. })));
    }

    #[test]
    fn a_server_that_stops_answering_on_a_second_request_does_not_hang_the_collector() {
        // Answers the first request, then on the same connection reads the
        // next request and never replies. A reused connection without its
        // read timeout would wait here forever.
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for conn in listener.incoming().flatten() {
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let mut w = conn;
                let _ = w.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok");
                held.push(w); // keep it open and silent
            }
        });
        let client = HttpClient::new(&[addr.to_string()]);
        let started = std::time::Instant::now();
        for _ in 0..2 {
            let body = client.get(&format!("http://{addr}/x"), &[]).unwrap().bytes("x", 10).unwrap();
            assert_eq!(body, b"ok");
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "each request used a fresh connection");
    }

    #[test]
    fn spec_selects_the_kind() {
        let dir = mirror();
        assert_eq!(upstream_from(dir.path().to_str().unwrap()).unwrap().describe(), dir.path().display().to_string());
        assert_eq!(upstream_from("https://deb.debian.org/debian/").unwrap().describe(), "https://deb.debian.org/debian");
        assert!(HttpUpstream::new("ftp://x").is_err());
    }
}
