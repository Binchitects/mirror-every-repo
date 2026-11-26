//! A small HTTP client for upstream registries. It only ever sends a request
//! to a host on its allowlist, and that includes every redirect it follows.
use crate::FetchError;
use std::io::Read;
use std::time::{Duration, Instant};

/// Redirects followed for one request, as browsers and curl allow.
const MAX_REDIRECTS: usize = 5;

/// Waits before retrying a request that failed in transit (connection or TLS
/// trouble) or that the server turned away for now (429, 502, 503, 504).
/// A failure while reading the body is not retried here.
const RETRY_DELAYS_MS: [u64; 3] = [500, 2_000, 6_000];

/// Resolve a `Location` header against the URL that answered with it.
pub(crate) fn resolve(base: &str, location: &str) -> Option<String> {
    let location = location.trim();
    if location.is_empty() {
        return None;
    }
    if location.starts_with("https://") || location.starts_with("http://") {
        return Some(location.split('#').next()?.to_string());
    }
    let (scheme, rest) = base.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let target = if let Some(net) = location.strip_prefix("//") {
        format!("{scheme}://{net}")
    } else if location.starts_with('/') {
        format!("{scheme}://{authority}{location}")
    } else {
        let path = rest[authority.len()..].split(['?', '#']).next().unwrap_or("");
        let dir = &path[..path.rfind('/').map_or(0, |i| i + 1)];
        let dir = if dir.is_empty() { "/" } else { dir };
        format!("{scheme}://{authority}{dir}{location}")
    };
    Some(target.split('#').next()?.to_string())
}

#[derive(Clone)]
pub struct HttpClient {
    direct: ureq::Agent,
    /// Agents through the configured proxies, for https and http URLs.
    via_https: Option<(String, ureq::Agent)>,
    via_http: Option<(String, ureq::Agent)>,
    no_proxy: Vec<String>,
    allowed: Vec<String>,
    /// Multiplies the retry waits (0 in tests).
    retry_scale: u64,
}

/// Proxy settings, as `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY` give them.
#[derive(Clone, Debug, Default)]
pub struct Proxies {
    pub https: Option<String>,
    pub http: Option<String>,
    /// Host names (and `.domain` suffixes) reached directly; `*` for all.
    pub no_proxy: Vec<String>,
}

impl Proxies {
    /// From the environment, upper- or lower-case names; empty values are unset.
    pub fn from_env() -> Proxies {
        let var = |names: [&str; 2]| names.iter().find_map(|n| std::env::var(n).ok()).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        Proxies {
            https: var(["HTTPS_PROXY", "https_proxy"]),
            http: var(["HTTP_PROXY", "http_proxy"]),
            no_proxy: var(["NO_PROXY", "no_proxy"]).map(|v| v.split(',').map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect()).unwrap_or_default(),
        }
    }
}

fn agent(proxy: Option<&str>) -> Result<ureq::Agent, FetchError> {
    let mut b = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(120))
        .user_agent("mer-collector")
        // Redirects are followed here, one by one, against the allowlist.
        .redirects(0)
        // No connection reuse: ureq 2 clears a connection's timeouts when it
        // goes back to the pool and does not set them again when it is
        // reused, so a reused connection that stalls would block forever.
        // A fresh connection per request always has its timeouts.
        .max_idle_connections(0)
        .max_idle_connections_per_host(0);
    if let Some(p) = proxy {
        let authority = p.strip_prefix("http://").or_else(|| p.strip_prefix("https://")).map(|r| r.trim_end_matches('/')).unwrap_or("");
        let hostport = authority.rsplit('@').next().unwrap_or("");
        if hostport.is_empty() || hostport.contains('/') || hostport.bytes().any(|c| c <= 0x20) {
            return Err(FetchError::BadUpstream(format!("proxy {p:?} is not an http://host:port URL")));
        }
        b = b.proxy(ureq::Proxy::new(p).map_err(|e| FetchError::BadUpstream(format!("proxy {p:?}: {e}")))?);
    }
    Ok(b.build())
}

pub struct HttpResponse {
    pub status: u16,
    headers: Vec<(String, String)>,
    body: Box<dyn Read + Send>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn into_reader(self) -> Box<dyn Read + Send> {
        self.body
    }

    /// Read the whole body, refusing anything longer than `limit`.
    pub fn bytes(self, what: &str, limit: u64) -> Result<Vec<u8>, FetchError> {
        let mut out = Vec::new();
        self.body
            .take(limit + 1)
            .read_to_end(&mut out)
            .map_err(|e| FetchError::Transport { path: what.into(), reason: e.to_string() })?;
        if out.len() as u64 > limit {
            return Err(FetchError::TooLarge { path: what.into(), limit });
        }
        Ok(out)
    }
}

/// `scheme://host[:port]/...` -> (`host[:port]` lower-cased).
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None; // no credentials in URLs
    }
    Some(authority.to_ascii_lowercase())
}

impl HttpClient {
    /// `allowed` are `host` or `host:port` values the collector may contact.
    /// Proxies come from the environment (`HTTPS_PROXY`, `HTTP_PROXY`,
    /// `NO_PROXY`); a proxy setting that cannot be used is ignored with a
    /// message, so a typo never silently sends traffic another way.
    pub fn new(allowed: &[String]) -> Self {
        match Self::with_proxies(allowed, &Proxies::from_env()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("WARNING: {e}; connecting directly");
                Self::with_proxies(allowed, &Proxies::default()).expect("no proxy")
            }
        }
    }

    /// Like [`HttpClient::new`], with explicit proxy settings.
    pub fn with_proxies(allowed: &[String], p: &Proxies) -> Result<Self, FetchError> {
        Ok(HttpClient {
            direct: agent(None)?,
            via_https: p.https.as_ref().map(|u| agent(Some(u)).map(|a| (u.clone(), a))).transpose()?,
            via_http: p.http.as_ref().map(|u| agent(Some(u)).map(|a| (u.clone(), a))).transpose()?,
            no_proxy: p.no_proxy.clone(),
            allowed: allowed.iter().map(|h| h.to_ascii_lowercase()).collect(),
            retry_scale: 1,
        })
    }

    /// Retry without waiting (for tests).
    pub fn without_retry_delay(mut self) -> Self {
        self.retry_scale = 0;
        self
    }

    /// The proxy a request to `url` goes through, if any.
    pub fn proxy_for(&self, url: &str) -> Option<&str> {
        self.route(url).0
    }

    fn route(&self, url: &str) -> (Option<&str>, &ureq::Agent) {
        let host = host_of(url).unwrap_or_default();
        let name = host.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(host.as_str(), |(h, _)| h);
        let bypass = self.no_proxy.iter().any(|n| n == "*" || n == name || n.strip_prefix('.').is_some_and(|d| name.ends_with(&format!(".{d}"))) || name.ends_with(&format!(".{n}")));
        let via = if bypass { None } else if url.starts_with("https://") { self.via_https.as_ref() } else { self.via_http.as_ref() };
        match via {
            Some((u, a)) => (Some(u.as_str()), a),
            None => (None, &self.direct),
        }
    }

    pub fn allows(&self, url: &str) -> bool {
        host_of(url).is_some_and(|h| self.allowed.contains(&h))
    }

    /// GET returning whatever status the server gave (the caller decides what
    /// a 401 or 404 means). Transport failures and disallowed hosts are errors.
    ///
    /// Redirects are followed only to allowlisted hosts and never from https
    /// to http. Credentials (`Authorization`) are not carried to another host.
    pub fn get_any(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, FetchError> {
        if !self.allows(url) {
            return Err(FetchError::BadUpstream(format!(
                "{url}: host is not on the allowlist ({})",
                self.allowed.join(", ")
            )));
        }
        let first_host = host_of(url);
        let mut current = url.to_string();
        for hop in 0..=MAX_REDIRECTS {
            if current.bytes().any(|b| b <= 0x20 || b == 0x7f) {
                return Err(FetchError::BadPath(current));
            }
            let same_host = host_of(&current) == first_host;
            let mut attempt = 0;
            let resp = loop {
                let mut req = self.route(&current).1.get(&current);
                for (k, v) in headers {
                    if same_host || !k.eq_ignore_ascii_case("authorization") {
                        req = req.set(k, v);
                    }
                }
                let outcome = match req.call() {
                    Ok(r) => Ok(r),
                    Err(ureq::Error::Status(_, r)) => Ok(r),
                    Err(e) => Err(e.to_string()),
                };
                let again = match &outcome {
                    Ok(r) => matches!(r.status(), 429 | 502 | 503 | 504),
                    Err(_) => true,
                };
                if !again || attempt == RETRY_DELAYS_MS.len() {
                    match outcome {
                        Ok(r) => break r,
                        Err(reason) => return Err(FetchError::Transport { path: current, reason }),
                    }
                }
                std::thread::sleep(Duration::from_millis(self.retry_scale * RETRY_DELAYS_MS[attempt]));
                attempt += 1;
            };
            let status = resp.status();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if hop == MAX_REDIRECTS {
                    return Err(FetchError::BadUpstream(format!("{url}: more than {MAX_REDIRECTS} redirects")));
                }
                let next = resp
                    .header("Location")
                    .and_then(|l| resolve(&current, l))
                    .ok_or_else(|| FetchError::BadUpstream(format!("{current}: redirect without a usable Location")))?;
                if current.starts_with("https://") && !next.starts_with("https://") {
                    return Err(FetchError::BadUpstream(format!("{current}: refusing a redirect from https to {next}")));
                }
                if !self.allows(&next) {
                    let host = host_of(&next).unwrap_or_default();
                    return Err(FetchError::BadUpstream(format!(
                        "{current} redirects to host {host}, which is not on the allowlist ({}); if it is expected, allow it with --allow-host {host}",
                        self.allowed.join(", ")
                    )));
                }
                current = next;
                continue;
            }
            let headers =
                resp.headers_names().into_iter().map(|n| (n.clone(), resp.header(&n).unwrap_or("").to_string())).collect();
            return Ok(HttpResponse { status, headers, body: Box::new(resp.into_reader()) });
        }
        unreachable!("the loop returns on its last hop")
    }

    /// GET a whole body. A body that breaks off or stalls mid-way is fetched
    /// again (the request itself is already retried by `get_any`).
    pub fn get_bytes(&self, url: &str, headers: &[(&str, &str)], limit: u64) -> Result<Vec<u8>, FetchError> {
        let mut attempt = 0;
        loop {
            match self.get(url, headers)?.bytes(url, limit) {
                Err(FetchError::Transport { .. }) if attempt < RETRY_DELAYS_MS.len() => {
                    std::thread::sleep(Duration::from_millis(self.retry_scale * RETRY_DELAYS_MS[attempt]));
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    /// GET that must succeed: 404 and other error statuses become errors.
    ///
    /// The body resumes by itself: when it breaks off, or stalls (less than
    /// 16 KiB in 30 seconds), the rest is requested again with a `Range`
    /// header, or the start is skipped when the server ignores ranges.
    /// Callers verify content by hash, so a resumed body is checked like any.
    pub fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, FetchError> {
        let mut resp = self.get_any(url, headers)?;
        match resp.status {
            200..=299 => {
                let inner = std::mem::replace(&mut resp.body, Box::new(std::io::empty()));
                resp.body = Box::new(Resuming {
                    client: self.clone(),
                    url: url.to_string(),
                    headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                    inner,
                    offset: 0,
                    resumes: 0,
                    window: (Instant::now(), 0),
                });
                Ok(resp)
            }
            404 => Err(FetchError::NotFound(url.into())),
            status => Err(FetchError::Http { path: url.into(), status }),
        }
    }
}

/// How slow a body may be before it counts as stalled.
const STALL_WINDOW: Duration = Duration::from_secs(30);
const STALL_BYTES: u64 = 16 << 10;

/// A response body that picks up where it broke off.
struct Resuming {
    client: HttpClient,
    url: String,
    headers: Vec<(String, String)>,
    inner: Box<dyn Read + Send>,
    offset: u64,
    resumes: usize,
    /// Start of the current measuring window and bytes read in it.
    window: (Instant, u64),
}

impl Resuming {
    fn resume(&mut self, why: std::io::Error) -> std::io::Result<()> {
        if self.resumes >= RETRY_DELAYS_MS.len() {
            return Err(why);
        }
        std::thread::sleep(Duration::from_millis(self.client.retry_scale * RETRY_DELAYS_MS[self.resumes]));
        self.resumes += 1;
        let range = format!("bytes={}-", self.offset);
        let mut headers: Vec<(&str, &str)> = self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        headers.push(("Range", &range));
        let resp = self.client.get_any(&self.url, &headers).map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut body = resp.body;
        match resp.status {
            206 => {
                let start = resp.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-range")).and_then(|(_, v)| v.strip_prefix("bytes ")?.split('-').next()?.parse::<u64>().ok());
                if start != Some(self.offset) {
                    return Err(std::io::Error::other(format!("{}: the server resumed at the wrong place", self.url)));
                }
            }
            // No range support: read past what we already have.
            200 => {
                let skipped = std::io::copy(&mut (&mut body).take(self.offset), &mut std::io::sink())?;
                if skipped != self.offset {
                    return Err(std::io::Error::other(format!("{}: the body is shorter on the second try", self.url)));
                }
            }
            s => return Err(std::io::Error::other(format!("{}: answered HTTP {s} when resuming", self.url))),
        }
        self.inner = body;
        self.window = (Instant::now(), 0);
        Ok(())
    }
}

impl Read for Resuming {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.inner.read(buf) {
                Ok(n) => {
                    self.offset += n as u64;
                    self.window.1 += n as u64;
                    if self.window.0.elapsed() >= STALL_WINDOW {
                        if self.window.1 < STALL_BYTES && n > 0 {
                            let stalled = std::io::Error::new(std::io::ErrorKind::TimedOut, "stalled");
                            // What was read stays read; continue from after it.
                            self.resume(stalled)?;
                            return Ok(n);
                        }
                        self.window = (Instant::now(), 0);
                    }
                    return Ok(n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => self.resume(e)?,
            }
        }
    }
}
