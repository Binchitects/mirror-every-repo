//! Requests that package clients make which are not plain file paths.
//!
//! Most formats are served as the files of the snapshot. A few client
//! conventions need translating first; that translation lives here so the
//! snapshot stays a plain tree of files.
use crate::{header, percent_decode, App, Reply};
use mer_meta::Repo;
use mer_storage::Lookup;
use serde_json::json;
use std::io::Read;
use std::sync::atomic::Ordering;
use tiny_http::Request;

pub(crate) enum Route {
    /// Serve the path as it was asked for.
    Unhandled,
    /// The answer is not a stored file; here it is.
    Done(Reply),
    /// Send the client to the canonical URL.
    Redirect(String),
    /// Serve `path` instead. If it is missing, record `wish` on the wishlist.
    Mapped { path: String, wish: String },
}

/// PEP 503 name normalisation: lower-case, runs of `-`, `_`, `.` become `-`.
fn pypi_name(name: &str) -> Option<String> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !ok {
        return None;
    }
    let mut out = String::new();
    let mut sep = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            sep = true;
        } else {
            if sep && !out.is_empty() {
                out.push('-');
            }
            sep = false;
            out.push(c.to_ascii_lowercase());
        }
    }
    (!out.is_empty()).then_some(out)
}

/// pip asks for `/simple/<project>/` and expects an HTML page of links.
fn pypi(repo: &Repo, path: &str) -> Route {
    if path == "simple" {
        return Route::Redirect(format!("/{}/simple/", repo.name));
    }
    let Some(rest) = path.strip_prefix("simple/") else { return Route::Unhandled };
    if rest.is_empty() {
        return Route::Mapped { path: "simple/index.html".into(), wish: "simple".into() };
    }
    let (name, slash) = match rest.strip_suffix('/') {
        Some(n) => (n, true),
        None => (rest, false),
    };
    let Some(norm) = pypi_name(name) else { return Route::Unhandled };
    if !slash || norm != name {
        // Links on the page are relative, so the URL must end in a slash; and
        // "Foo_Bar" must lead to the same page as "foo-bar".
        return Route::Redirect(format!("/{}/simple/{norm}/", repo.name));
    }
    Route::Mapped { path: format!("simple/{norm}/index.html"), wish: format!("simple/{norm}") }
}

/// npm package names: an optional `@scope/`, then URL-safe characters.
fn npm_name(name: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty()
            && !p.starts_with(['.', '_'])
            && p.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'))
    };
    name.len() <= 214
        && match name.strip_prefix('@') {
            Some(rest) => rest.split_once('/').is_some_and(|(scope, pkg)| part(scope) && part(pkg)),
            None => part(name),
        }
}

/// The address the client used to reach this server, from its Host header.
fn own_base(req: &Request, tls: bool, repo: &Repo) -> Option<String> {
    let host = header(req, "Host")?.trim();
    let ok = !host.is_empty()
        && host.len() <= 255
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'));
    ok.then(|| format!("{}://{host}/{}/", if tls { "https" } else { "http" }, repo.name))
}

const NPM_PLACEHOLDER: &str = "mer://base/";
const MAX_PACKUMENT: u64 = 512 << 20;

/// npm asks for `/<name>` (a JSON document listing versions) and then for
/// the tarball URLs that document names. Those URLs must be absolute, so the
/// stored document carries a placeholder that is replaced here with the
/// address the client used to reach this server.
fn npm(app: &App, req: &Request, tls: bool, repo: &Repo, raw: &str) -> Route {
    let not_found = || Route::Done(Reply::json(404, json!({ "error": "not found" })));
    let Some(path) = percent_decode(raw) else { return Route::Done(Reply::json(400, json!({ "error": "bad request" }))) };
    if path == "-/ping" {
        return Route::Done(Reply::json(200, json!({})));
    }
    if path.starts_with("-/") {
        return not_found(); // search, audit and similar registry services are not offered
    }
    if let Some((name, file)) = path.split_once("/-/") {
        let safe = npm_name(name)
            && file.ends_with(".tgz")
            && !file.starts_with('.')
            && file.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'));
        if !safe {
            return not_found();
        }
        let stored = format!("tarballs/{name}/{file}");
        return Route::Mapped { wish: stored.clone(), path: stored };
    }
    if !npm_name(&path) {
        return not_found();
    }
    match app.st.lookup_published(&repo.name, &format!("packuments/{path}.json"), false) {
        Ok(Lookup::Found(entry)) => {
            let Some(base) = own_base(req, tls, repo) else {
                return Route::Done(Reply::json(400, json!({ "error": "a Host header is required" })));
            };
            let mut text = String::new();
            let read = app.st.open_blob(&entry.digest, None).map(|r| r.take(MAX_PACKUMENT).read_to_string(&mut text));
            if !matches!(read, Ok(Ok(_))) {
                eprintln!("ERROR: package document of {}/{path} cannot be read", repo.name);
                return Route::Done(Reply::json(500, json!({ "error": "internal error" })));
            }
            // The placeholder only ever appears inside JSON strings, and the
            // base was checked to contain nothing that needs escaping.
            let body = text.replace(NPM_PLACEHOLDER, &base);
            Route::Done(Reply::bytes(200, "application/json", body.into_bytes()))
        }
        Ok(Lookup::Miss) => {
            // A package that is not mirrored: exactly what the wishlist is for.
            app.metrics.misses.fetch_add(1, Ordering::Relaxed);
            let _ = app.st.lookup_published(&repo.name, &path, true);
            not_found()
        }
        Ok(Lookup::NotPublished) => not_found(),
        Err(_) => Route::Done(Reply::json(500, json!({ "error": "internal error" }))),
    }
}

pub(crate) fn route(app: &App, req: &Request, tls: bool, repo: &Repo, path: &str) -> Route {
    match repo.format.as_str() {
        "pypi" => pypi(repo, path),
        "npm" => npm(app, req, tls, repo, path),
        _ => Route::Unhandled,
    }
}
