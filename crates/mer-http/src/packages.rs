//! Serving published snapshots to package clients.
use crate::{formats, header, percent_decode, App, Reply};
use base64::Engine;
use mer_blob::StoreError;
use mer_core::{validate_path, EntryRec, EntryRole};
use mer_meta::MetaError;
use mer_storage::{Lookup, StorageError};
use std::sync::atomic::Ordering;
use tiny_http::{Method, Request};

fn content_type(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("deb") | Some("udeb") => "application/vnd.debian.binary-package",
        Some("gz") | Some("tgz") => "application/gzip",
        Some("xz") => "application/x-xz",
        Some("json") => "application/json",
        Some("html") => "text/html; charset=utf-8",
        Some("asc") | Some("gpg") | Some("txt") => "text/plain; charset=utf-8",
        Some("whl") | Some("zip") => "application/zip",
        _ if matches!(name, "Release" | "InRelease" | "Packages" | "Sources") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A single `bytes=` range, resolved against the file size, as (offset, length).
/// `Err(())` means the range cannot be satisfied (416).
fn parse_range(value: &str, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = value.trim().strip_prefix("bytes=") else { return Ok(None) };
    if spec.contains(',') {
        return Ok(None); // multiple ranges: answer with the whole file, which is allowed
    }
    let (from, to) = spec.split_once('-').ok_or(())?;
    let (from, to) = (from.trim(), to.trim());
    if from.is_empty() {
        // Suffix: the last N bytes.
        let n: u64 = to.parse().map_err(|_| ())?;
        if n == 0 || size == 0 {
            return Err(());
        }
        let n = n.min(size);
        return Ok(Some((size - n, n)));
    }
    let start: u64 = from.parse().map_err(|_| ())?;
    if start >= size {
        return Err(());
    }
    let end = if to.is_empty() { size - 1 } else { to.parse::<u64>().map_err(|_| ())?.min(size - 1) };
    if end < start {
        return Err(());
    }
    Ok(Some((start, end - start + 1)))
}

/// The secret a client presented: `Authorization: Bearer <token>`, or Basic
/// auth with the token as the password (what apt and pip can send).
pub(crate) fn presented_token(req: &Request) -> Option<String> {
    let value = header(req, "Authorization")?;
    let (scheme, rest) = value.trim().split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        return Some(rest.trim().to_string());
    }
    if scheme.eq_ignore_ascii_case("basic") {
        let decoded = base64::engine::general_purpose::STANDARD.decode(rest.trim()).ok()?;
        let text = String::from_utf8(decoded).ok()?;
        return text.split_once(':').map(|(_, password)| password.to_string());
    }
    None
}

/// `via_v2` is true for requests that came in under `/v2/`, the container
/// registry API. Image repositories answer only there, and nothing else does.
pub(crate) fn serve(app: &App, req: &Request, raw: &str, tls: bool, via_v2: bool) -> Reply {
    let st = &app.st;
    let Some((repo, path)) = raw.split_once('/') else { return Reply::text(404, "not found\n") };

    // Try the path exactly as sent, then with %XX escapes decoded.
    let mut candidates = vec![path.to_string()];
    match percent_decode(path) {
        Some(decoded) if decoded != path => candidates.push(decoded),
        Some(_) => {}
        None => return Reply::text(400, "bad request\n"),
    }
    let repo_row = match st.repo(repo) {
        Ok(r) => r,
        Err(StorageError::Meta(MetaError::NotFound(_))) => return Reply::text(404, "not found\n"),
        Err(_) => return Reply::text(500, "internal error\n"),
    };
    if (repo_row.format == "oci") != via_v2 {
        return Reply::text(404, "not found\n");
    }
    if repo_row.private {
        let allowed = presented_token(req).and_then(|secret| app.auth.check_token(&secret, repo));
        if allowed.is_none() {
            return Reply::text(401, "this repository needs an access token\n")
                .with("WWW-Authenticate", "Basic realm=\"mirror-every-repo\"");
        }
    }
    // Formats whose clients ask for something other than a stored file path.
    let mut wish_as: Option<String> = None;
    let mut record_wish = true;
    match formats::route(app, req, tls, &repo_row, path) {
        formats::Route::Unhandled => {}
        formats::Route::Done(reply) => return reply,
        formats::Route::Redirect(location) => return Reply::empty(301, vec![("Location", location)]),
        formats::Route::Mapped { path, wish } => {
            candidates = vec![path];
            record_wish = wish.is_some();
            wish_as = wish;
        }
    }
    let mut entry: Option<EntryRec> = None;
    let mut published = true;
    for candidate in &candidates {
        if validate_path(candidate).is_err() {
            continue;
        }
        match st.lookup_published(repo, candidate, false) {
            Ok(Lookup::Found(e)) => {
                entry = Some(e);
                break;
            }
            Ok(Lookup::Miss) => {}
            Ok(Lookup::NotPublished) => published = false,
            Err(_) => return Reply::text(500, "internal error\n"),
        }
    }
    // Metadata kept for the mirror's own use is not served.
    let entry = entry.filter(|e| e.role == EntryRole::Artifact);
    let Some(entry) = entry else {
        // A miss is a wish only where the client asked for real content. apt
        // probes for optional index files; those are not recorded.
        let wish = wish_as.as_ref().or(candidates.last()).filter(|p| validate_path(p).is_ok());
        if let (true, Some(path)) = (published && record_wish, wish) {
            if repo_row.format != "apt" || path.starts_with("pool/") {
                app.metrics.misses.fetch_add(1, Ordering::Relaxed);
                let _ = st.lookup_published(repo, path, true);
            }
        }
        return Reply::text(404, "not found\n");
    };

    let etag = format!("\"sha256:{}\"", entry.digest);
    let mut headers = vec![
        ("Content-Type", content_type(&entry.path).to_string()),
        ("ETag", etag.clone()),
        ("Accept-Ranges", "bytes".to_string()),
    ];
    if header(req, "If-None-Match").is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*")) {
        return Reply::empty(304, headers);
    }
    let range = match header(req, "Range").map(|v| parse_range(v, entry.size)) {
        Some(Err(())) => {
            return Reply::empty(416, vec![("Content-Range", format!("bytes */{}", entry.size))]);
        }
        Some(Ok(r)) => r,
        None => None,
    };
    let (status, len) = match range {
        Some((off, len)) => {
            headers.push(("Content-Range", format!("bytes {off}-{}/{}", off + len - 1, entry.size)));
            (206, len)
        }
        None => (200, entry.size),
    };
    match st.open_blob(&entry.digest, range) {
        Ok(body) => {
            if *req.method() != Method::Head {
                app.record_download(repo, &entry.path, len, req.remote_addr().map(|a| a.ip()));
            }
            Reply { status, headers, body, len }
        }
        Err(StorageError::Store(StoreError::NotFound(_))) => {
            eprintln!("ERROR: blob {} of {repo}/{} is missing from the store; run `mer fsck`", entry.digest, entry.path);
            Reply::text(500, "internal error\n")
        }
        Err(e) => {
            eprintln!("ERROR: {repo}/{}: {e}", entry.path);
            Reply::text(500, "internal error\n")
        }
    }
}

/// Plain-text list of repositories, for a listener without the web panel.
pub(crate) fn index(app: &App) -> Reply {
    let mut out = String::from("mirror-every-repo vault\n\n");
    let repos = app.st.meta().read(|t| {
        let mut rows = Vec::new();
        for r in t.repo_list()? {
            let live = t.live(r.id)?;
            rows.push((r, live));
        }
        Ok(rows)
    });
    match repos {
        Ok(rows) => {
            for (r, live) in rows {
                match live {
                    Some(l) => out.push_str(&format!("/{}/  {}  snapshot {}\n", r.name, r.format, l.seq)),
                    None => out.push_str(&format!("/{}/  {}  nothing published\n", r.name, r.format)),
                }
            }
            Reply::text(200, out)
        }
        Err(_) => Reply::text(500, "internal error\n"),
    }
}
