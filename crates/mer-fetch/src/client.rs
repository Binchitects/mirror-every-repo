//! A small HTTP client for upstream registries that are not plain file trees
//! (PyPI, npm, OCI). It only ever starts a request to a host on its allowlist.
use crate::FetchError;
use std::io::Read;
use std::time::Duration;

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
            .build();
        HttpClient { agent, allowed: allowed.iter().map(|h| h.to_ascii_lowercase()).collect() }
    }

    pub fn allows(&self, url: &str) -> bool {
        host_of(url).is_some_and(|h| self.allowed.contains(&h))
    }

    /// GET returning whatever status the server gave (the caller decides what
    /// a 401 or 404 means). Transport failures and disallowed hosts are errors.
    pub fn get_any(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, FetchError> {
        if !self.allows(url) {
            return Err(FetchError::BadUpstream(format!(
                "{url}: host is not on the allowlist ({})",
                self.allowed.join(", ")
            )));
        }
        if url.bytes().any(|b| b <= 0x20 || b == 0x7f) {
            return Err(FetchError::BadPath(url.to_string()));
        }
        let mut req = self.agent.get(url);
        for (k, v) in headers {
            req = req.set(k, v);
        }
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => return Err(FetchError::Transport { path: url.into(), reason: e.to_string() }),
        };
        let status = resp.status();
        let headers =
            resp.headers_names().into_iter().map(|n| (n.clone(), resp.header(&n).unwrap_or("").to_string())).collect();
        Ok(HttpResponse { status, headers, body: Box::new(resp.into_reader()) })
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
