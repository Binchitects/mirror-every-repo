//! Where the collector reads upstream repositories from: an HTTP(S) mirror
//! or a directory holding a copy of one.
//!
//! Nothing read through here is trusted. Callers verify signatures and
//! hashes; this crate only bounds what can be asked for and how much is read.
mod client;

pub use client::{host_of, HttpClient, HttpResponse};

use mer_core::validate_path;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

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

pub struct HttpUpstream {
    base: String,
    agent: ureq::Agent,
}

impl HttpUpstream {
    pub fn new(base: &str) -> Result<Self, FetchError> {
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(FetchError::BadUpstream(format!("{base:?} is not an http(s) URL")));
        }
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(30))
            .timeout_read(Duration::from_secs(120))
            .user_agent("mer-collector")
            .build();
        Ok(HttpUpstream { base: base.trim_end_matches('/').to_string(), agent })
    }
}

impl Upstream for HttpUpstream {
    fn open(&self, path: &str) -> Result<Box<dyn Read + Send>, FetchError> {
        check(path)?;
        // Only characters that are safe in a URL path are sent as they are.
        if !path.bytes().all(|b| b.is_ascii_alphanumeric() || b"/-._~+%=@:,".contains(&b)) {
            return Err(FetchError::BadPath(path.to_string()));
        }
        match self.agent.get(&format!("{}/{}", self.base, path)).call() {
            Ok(resp) => Ok(Box::new(resp.into_reader())),
            Err(ureq::Error::Status(404, _)) => Err(FetchError::NotFound(path.into())),
            Err(ureq::Error::Status(status, _)) => Err(FetchError::Http { path: path.into(), status }),
            Err(e) => Err(FetchError::Transport { path: path.into(), reason: e.to_string() }),
        }
    }
    fn describe(&self) -> String {
        self.base.clone()
    }
}

/// `http://...` or `https://...` is a remote mirror; anything else is a directory.
pub fn upstream_from(spec: &str) -> Result<Box<dyn Upstream>, FetchError> {
    if spec.starts_with("http://") || spec.starts_with("https://") {
        Ok(Box::new(HttpUpstream::new(spec)?))
    } else {
        Ok(Box::new(DirUpstream::new(spec)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn spec_selects_the_kind() {
        let dir = mirror();
        assert_eq!(upstream_from(dir.path().to_str().unwrap()).unwrap().describe(), dir.path().display().to_string());
        assert_eq!(upstream_from("https://deb.debian.org/debian/").unwrap().describe(), "https://deb.debian.org/debian");
        assert!(HttpUpstream::new("ftp://x").is_err());
    }
}
