//! HTTP for the vault (and the collector's panel).
//!
//! Two surfaces, which can share one listener or have one each:
//!
//! - **packages**: `GET /<repo>/<path>` returns the file at `<path>` in the
//!   snapshot that is currently live for `<repo>`. Nothing else in the store
//!   is reachable: unpublished, quarantined, rejected and retired content has
//!   no URL. Private repos need a client token.
//! - **admin**: the web panel under `/ui/` and its JSON API under `/api/`,
//!   for signed-in users only.
mod api;
mod formats;
mod packages;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_admin;
#[cfg(test)]
mod tests_formats;
mod ui;

use mer_auth::Auth;
use mer_storage::{now_ms, Storage};
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response, StatusCode};

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("cannot listen on {addr}: {reason}")]
    Bind { addr: String, reason: String },
    #[error("TLS: {0}")]
    Tls(String),
}

#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Address to listen on, for example `0.0.0.0:8080`.
    pub listen: String,
    pub threads: usize,
    /// Print one line per request to standard output.
    pub access_log: bool,
    /// Serve repositories to package clients on this listener.
    pub packages: bool,
    /// Serve the web panel and its API on this listener.
    pub admin: bool,
    /// PEM certificate chain and private key. `None` = plain HTTP.
    pub tls: Option<(Vec<u8>, Vec<u8>)>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig { listen: "0.0.0.0:8080".into(), threads: 8, access_log: true, packages: true, admin: true, tls: None }
    }
}

#[derive(Default)]
pub(crate) struct Metrics {
    by_status: Mutex<BTreeMap<u16, u64>>,
    bytes_sent: AtomicU64,
    misses: AtomicU64,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ClientStat {
    pub hits: u64,
    pub bytes: u64,
    pub last_at: i64,
}

/// Download statistics. Counters are kept in memory and written to the
/// database in batches, so serving a file never waits for a disk sync.
pub(crate) struct Traffic {
    pending: Mutex<HashMap<(String, String), (u64, u64)>>,
    clients: Mutex<HashMap<IpAddr, ClientStat>>,
    last_flush: Mutex<Instant>,
}

/// State shared by every listener of one process.
pub struct App {
    pub(crate) st: Arc<Storage>,
    pub(crate) auth: Auth,
    pub(crate) metrics: Metrics,
    pub(crate) traffic: Traffic,
}

impl App {
    pub fn new(st: Arc<Storage>) -> Arc<App> {
        Arc::new(App {
            auth: Auth::new(st.clone()),
            st,
            metrics: Metrics::default(),
            traffic: Traffic {
                pending: Mutex::default(),
                clients: Mutex::default(),
                last_flush: Mutex::new(Instant::now()),
            },
        })
    }

    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    pub(crate) fn record_download(&self, repo: &str, path: &str, bytes: u64, peer: Option<IpAddr>) {
        {
            let mut pending = lock(&self.traffic.pending);
            let e = pending.entry((repo.to_string(), path.to_string())).or_insert((0, 0));
            e.0 += 1;
            e.1 += bytes;
        }
        if let Some(ip) = peer {
            let mut clients = lock(&self.traffic.clients);
            if clients.len() >= 10_000 && !clients.contains_key(&ip) {
                clients.clear(); // bounded; this view is "recent clients", not an archive
            }
            let c = clients.entry(ip).or_default();
            c.hits += 1;
            c.bytes += bytes;
            c.last_at = now_ms();
        }
        let due = {
            let last = lock(&self.traffic.last_flush);
            last.elapsed() >= Duration::from_secs(5) || lock(&self.traffic.pending).len() >= 500
        };
        if due {
            self.flush_traffic();
        }
    }

    /// Write buffered download counters to the database.
    pub fn flush_traffic(&self) {
        let batch: Vec<((String, String), (u64, u64))> = lock(&self.traffic.pending).drain().collect();
        *lock(&self.traffic.last_flush) = Instant::now();
        if batch.is_empty() {
            return;
        }
        let now = now_ms();
        let _ = self.st.meta().write(|t| {
            for ((repo, path), (hits, bytes)) in &batch {
                t.traffic_add(repo, path, *hits, *bytes, now)?;
            }
            Ok(())
        });
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What one listener offers.
#[derive(Clone, Copy)]
pub(crate) struct Surface {
    packages: bool,
    admin: bool,
    pub(crate) tls: bool,
    log: bool,
}

pub struct HttpServer {
    addr: SocketAddr,
    app: Arc<App>,
    stop: Arc<AtomicBool>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl HttpServer {
    pub fn start(app: Arc<App>, cfg: ServerConfig) -> Result<Self, HttpError> {
        let bind = |reason: String| HttpError::Bind { addr: cfg.listen.clone(), reason };
        let surface = Surface { packages: cfg.packages, admin: cfg.admin, tls: cfg.tls.is_some(), log: cfg.access_log };
        let server = match cfg.tls.clone() {
            None => tiny_http::Server::http(&cfg.listen).map_err(|e| bind(e.to_string()))?,
            Some((certificate, private_key)) => {
                tiny_http::Server::https(&cfg.listen, tiny_http::SslConfig { certificate, private_key })
                    .map_err(|e| HttpError::Tls(e.to_string()))?
            }
        };
        let server = Arc::new(server);
        let addr = server.server_addr().to_ip().ok_or_else(|| bind("not an IP address".into()))?;
        let stop = Arc::new(AtomicBool::new(false));
        let workers = (0..cfg.threads.max(1))
            .map(|_| {
                let (server, app, stop) = (server.clone(), app.clone(), stop.clone());
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match server.recv_timeout(Duration::from_millis(200)) {
                            Ok(Some(req)) => serve(&app, surface, req),
                            Ok(None) => {}
                            Err(_) => break,
                        }
                    }
                })
            })
            .collect();
        Ok(HttpServer { addr, app, stop, workers })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Serve until the process is stopped.
    pub fn wait(self) {
        for w in self.workers {
            let _ = w.join();
        }
    }

    pub fn shutdown(self) {
        self.stop.store(true, Ordering::Relaxed);
        let app = self.app.clone();
        self.wait();
        app.flush_traffic();
    }
}

/// What to send back, before it is turned into bytes on the wire.
pub(crate) struct Reply {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Box<dyn Read + Send>,
    pub len: u64,
}

impl Reply {
    pub fn bytes(status: u16, content_type: &str, body: Vec<u8>) -> Reply {
        Reply {
            status,
            headers: vec![("Content-Type", content_type.to_string())],
            len: body.len() as u64,
            body: Box::new(std::io::Cursor::new(body)),
        }
    }
    pub fn text(status: u16, body: impl Into<String>) -> Reply {
        Reply::bytes(status, "text/plain; charset=utf-8", body.into().into_bytes())
    }
    pub fn json(status: u16, value: serde_json::Value) -> Reply {
        let mut r = Reply::bytes(status, "application/json", value.to_string().into_bytes());
        r.headers.push(("Cache-Control", "no-store".into()));
        r
    }
    pub fn empty(status: u16, headers: Vec<(&'static str, String)>) -> Reply {
        Reply { status, headers, body: Box::new(std::io::empty()), len: 0 }
    }
    pub fn with(mut self, name: &'static str, value: impl Into<String>) -> Reply {
        self.headers.push((name, value.into()));
        self
    }
}

fn serve(app: &App, surface: Surface, mut req: Request) {
    let reply = route(app, surface, &mut req);
    let (status, len) = (reply.status, reply.len);
    *lock(&app.metrics.by_status).entry(status).or_insert(0) += 1;
    if *req.method() != Method::Head {
        app.metrics.bytes_sent.fetch_add(len, Ordering::Relaxed);
    }
    if surface.log {
        let peer = req.remote_addr().map(|a| a.ip().to_string()).unwrap_or_else(|| "-".into());
        // The query string is not logged: it may carry things that should not be in logs.
        let path = req.url().split(['?', '#']).next().unwrap_or("");
        println!("{peer} {} {path} {status} {len}", req.method());
    }
    let mut headers: Vec<Header> = reply
        .headers
        .iter()
        .filter_map(|(k, v)| Header::from_bytes(k.as_bytes(), v.as_bytes()).ok())
        .collect();
    headers.extend(Header::from_bytes(&b"X-Content-Type-Options"[..], &b"nosniff"[..]).ok());
    // Always send Content-Length. The library would otherwise switch to
    // chunked encoding for anything above 32 KiB, and clients would lose the
    // size they use for progress, resume and sanity checks.
    let response = Response::new(StatusCode(status), headers, reply.body, Some(len as usize), None)
        .with_chunked_threshold(usize::MAX);
    let _ = req.respond(response);
}

pub(crate) fn header<'a>(req: &'a Request, name: &'static str) -> Option<&'a str> {
    req.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str())
}

/// Decode %XX escapes. `None` for malformed escapes, NUL bytes or invalid UTF-8.
pub(crate) fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            let v = u8::from_str_radix(hex, 16).ok()?;
            if v == 0 {
                return None;
            }
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn route(app: &App, surface: Surface, req: &mut Request) -> Reply {
    let url = req.url().to_string();
    let (raw, query) = match url.split_once('?') {
        Some((p, q)) => (p, q.split('#').next().unwrap_or("")),
        None => (url.split('#').next().unwrap_or(""), ""),
    };
    let Some(raw) = raw.strip_prefix('/') else { return Reply::text(400, "bad request\n") };
    let method = req.method().clone();
    let read_only = matches!(method, Method::Get | Method::Head);

    if let Some(path) = raw.strip_prefix("api/") {
        if !surface.admin {
            return Reply::text(404, "not found\n");
        }
        return api::handle(app, req, path, query, surface);
    }
    if !read_only {
        return Reply::text(405, "method not allowed\n").with("Allow", "GET, HEAD");
    }
    // Container registry API: /v2/<repo>/<image name>/...
    if raw == "v2" || raw.starts_with("v2/") {
        return if surface.packages {
            formats::oci_entry(app, req, raw.strip_prefix("v2").unwrap_or("").trim_start_matches('/'), surface.tls)
        } else {
            Reply::text(404, "not found\n")
        };
    }
    match raw {
        "healthz" => return Reply::text(200, "ok\n"),
        "metrics" => return metrics_page(app),
        "" if surface.admin => return Reply::empty(302, vec![("Location", "/ui/".into())]),
        "" => return packages::index(app),
        "ui" if surface.admin => return Reply::empty(302, vec![("Location", "/ui/".into())]),
        _ => {}
    }
    if let Some(path) = raw.strip_prefix("ui/") {
        return if surface.admin { ui::serve(path) } else { Reply::text(404, "not found\n") };
    }
    if surface.packages {
        packages::serve(app, req, raw, surface.tls, false)
    } else {
        Reply::text(404, "not found\n")
    }
}

fn metrics_page(app: &App) -> Reply {
    let m = &app.metrics;
    let mut out = String::new();
    out.push_str("# TYPE mer_http_requests_total counter\n");
    for (code, n) in lock(&m.by_status).iter() {
        out.push_str(&format!("mer_http_requests_total{{code=\"{code}\"}} {n}\n"));
    }
    out.push_str(&format!(
        "# TYPE mer_http_response_bytes_total counter\nmer_http_response_bytes_total {}\n",
        m.bytes_sent.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "# TYPE mer_http_misses_total counter\nmer_http_misses_total {}\n",
        m.misses.load(Ordering::Relaxed)
    ));
    let gauges = app.st.meta().read(|t| {
        let mut g = String::new();
        let (blobs, bytes) = t.blob_count()?;
        g.push_str(&format!("# TYPE mer_store_blobs gauge\nmer_store_blobs {blobs}\n"));
        g.push_str(&format!("# TYPE mer_store_bytes gauge\nmer_store_bytes {bytes}\n"));
        g.push_str("# TYPE mer_repo_live_snapshot gauge\n# TYPE mer_repo_bytes gauge\n");
        for r in t.repo_list()? {
            let live = t.live(r.id)?.map_or(0, |l| l.seq);
            g.push_str(&format!("mer_repo_live_snapshot{{repo=\"{}\"}} {live}\n", r.name));
            g.push_str(&format!("mer_repo_bytes{{repo=\"{}\"}} {}\n", r.name, t.repo_usage(r.id)?));
        }
        g.push_str("# TYPE mer_channel_imported_seq gauge\n");
        for c in t.channel_list()? {
            g.push_str(&format!("mer_channel_imported_seq{{channel=\"{}\"}} {}\n", c.id, c.imported_seq));
        }
        Ok(g)
    });
    match gauges {
        Ok(g) => {
            out.push_str(&g);
            Reply::bytes(200, "text/plain; version=0.0.4", out.into_bytes())
        }
        Err(_) => Reply::text(500, "internal error\n"),
    }
}
