//! A small HTTP client for upstream registries. It only ever sends a request
//! to a host on its allowlist, and that includes every redirect it follows.
use crate::FetchError;
use std::io::Read;
use std::time::Duration;

/// Redirects followed for one request, as browsers and curl allow.
const MAX_REDIRECTS: usize = 5;

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

pub struct HttpClient {
    agent: ureq::Agent,
    allowed: Vec<String>,
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
    pub fn new(allowed: &[String]) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(30))
            .timeout_read(Duration::from_secs(120))
            .user_agent("mer-collector")
            // Redirects are followed here, one by one, against the allowlist.
            .redirects(0)
            .build();
        HttpClient { agent, allowed: allowed.iter().map(|h| h.to_ascii_lowercase()).collect() }
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
            let mut req = self.agent.get(&current);
            for (k, v) in headers {
                if same_host || !k.eq_ignore_ascii_case("authorization") {
                    req = req.set(k, v);
                }
            }
            let resp = match req.call() {
                Ok(r) => r,
                Err(ureq::Error::Status(_, r)) => r,
                Err(e) => return Err(FetchError::Transport { path: current, reason: e.to_string() }),
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

    /// GET that must succeed: 404 and other error statuses become errors.
    pub fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, FetchError> {
        let resp = self.get_any(url, headers)?;
        match resp.status {
            200..=299 => Ok(resp),
            404 => Err(FetchError::NotFound(url.into())),
            status => Err(FetchError::Http { path: url.into(), status }),
        }
    }
}
