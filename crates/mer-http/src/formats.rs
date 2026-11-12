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
    /// Serve `path` instead. If it is missing, record `wish` on the wishlist
    /// (`None`: a miss here is not a wish).
    Mapped { path: String, wish: Option<String> },
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
        return Route::Mapped { path: "simple/index.html".into(), wish: None };
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
    Route::Mapped { path: format!("simple/{norm}/index.html"), wish: Some(format!("simple/{norm}")) }
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
        return Route::Mapped { wish: Some(stored.clone()), path: stored };
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

// ----------------------------------------------------------------- nuget

fn nuget_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 100
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && !id.contains("..")
}

/// A NuGet client starts at `/v3/index.json`, which tells it where packages
/// are. That address must be absolute, so the stored file carries a
/// placeholder that is replaced with the address the client used. Package
/// ids and versions are case-insensitive; the feed is stored lower-case.
fn nuget(app: &App, req: &Request, tls: bool, repo: &Repo, raw: &str) -> Route {
    let Some(path) = percent_decode(raw) else { return Route::Done(Reply::json(400, json!({ "error": "bad request" }))) };
    let path = path.to_ascii_lowercase();
    if path == "v3/index.json" {
        let Some(base) = own_base(req, tls, repo) else {
            return Route::Done(Reply::json(400, json!({ "error": "a Host header is required" })));
        };
        return match small_blob(app, repo, "v3/index.json") {
            Ok(Some(body)) => {
                let text = String::from_utf8_lossy(&body).replace(NPM_PLACEHOLDER, &base);
                Route::Done(Reply::bytes(200, "application/json", text.into_bytes()))
            }
            Ok(None) => Route::Done(Reply::json(404, json!({ "error": "not found" }))),
            Err(()) => Route::Done(Reply::json(500, json!({ "error": "internal error" }))),
        };
    }
    // Nothing else exists in a feed. Probes for services this vault does not
    // offer (search, metadata) and malformed names are refused, not wished for.
    let not_found = || Route::Done(Reply::json(404, json!({ "error": "not found" })));
    let Some(rest) = path.strip_prefix("flat/") else { return not_found() };
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        // The version list of a package: asking for one that is not mirrored is a wish.
        [id, "index.json"] if nuget_id(id) => Route::Mapped { path: path.clone(), wish: Some((*id).to_string()) },
        [id, version, _file] if nuget_id(id) => Route::Mapped { path: path.clone(), wish: Some(format!("{id}/{version}")) },
        _ => not_found(),
    }
}

// ------------------------------------------------------------ containers

const API_VERSION: (&str, &str) = ("Docker-Distribution-API-Version", "registry/2.0");
const MAX_MANIFEST: u64 = 4 << 20;

fn oci_error(status: u16, code: &str, message: &str) -> Reply {
    Reply::json(status, json!({ "errors": [{ "code": code, "message": message }] })).with(API_VERSION.0, API_VERSION.1)
}

fn oci_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name.split('/').all(|part| {
            let b = part.as_bytes();
            !b.is_empty()
                && b[0].is_ascii_alphanumeric()
                && b[b.len() - 1].is_ascii_alphanumeric()
                && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-'))
        })
}

fn oci_tag(tag: &str) -> bool {
    let b = tag.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// `sha256:<64 lower-case hex>` -> the hex part.
fn oci_digest(text: &str) -> Option<&str> {
    let hex = text.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then_some(hex)
}

fn small_blob(app: &App, repo: &Repo, path: &str) -> Result<Option<Vec<u8>>, ()> {
    match app.st.lookup_published(&repo.name, path, false) {
        Ok(Lookup::Found(entry)) => {
            let mut body = Vec::new();
            match app.st.open_blob(&entry.digest, None).map(|r| r.take(MAX_MANIFEST).read_to_end(&mut body)) {
                Ok(Ok(_)) => Ok(Some(body)),
                _ => Err(()),
            }
        }
        Ok(_) => Ok(None),
        Err(_) => Err(()),
    }
}

/// The registry API root, and the hand-over to normal serving for the rest.
pub(crate) fn oci_entry(app: &App, req: &Request, rest: &str, tls: bool) -> Reply {
    if rest.is_empty() {
        // Clients ping here first and only learn at this point that
        // credentials are needed. If any image repository is private and the
        // client sent none, ask for them now.
        let needs_login = app.st.meta().read(|t| Ok(t.repo_list()?.iter().any(|r| r.format == "oci" && r.private))).unwrap_or(false);
        if needs_login && header(req, "Authorization").is_none() {
            return oci_error(401, "UNAUTHORIZED", "authentication required")
                .with("WWW-Authenticate", "Basic realm=\"mirror-every-repo\"");
        }
        return Reply::json(200, json!({})).with(API_VERSION.0, API_VERSION.1);
    }
    let mut reply = crate::packages::serve(app, req, rest, tls, true);
    reply.headers.push((API_VERSION.0, API_VERSION.1.to_string()));
    reply
}

/// `<name>/manifests/<tag or digest>`, `<name>/blobs/<digest>`, `<name>/tags/list`.
fn oci(app: &App, repo: &Repo, path: &str) -> Route {
    let unknown = |code: &str, what: &str| Route::Done(oci_error(404, code, what));
    let internal = || Route::Done(oci_error(500, "UNKNOWN", "internal error"));

    if let Some(name) = path.strip_suffix("/tags/list") {
        if !oci_name(name) {
            return unknown("NAME_INVALID", "invalid repository name");
        }
        let prefix = format!("tags/{name}/");
        return match app.st.list_published(&repo.name, &prefix, 10_000) {
            Ok(entries) => {
                let tags: Vec<&str> = entries.iter().filter_map(|e| e.path.strip_prefix(&prefix)).filter(|t| !t.contains('/')).collect();
                if tags.is_empty() {
                    unknown("NAME_UNKNOWN", "repository name not known to registry")
                } else {
                    Route::Done(Reply::json(200, json!({ "name": name, "tags": tags })))
                }
            }
            Err(_) => internal(),
        };
    }
    if let Some((name, digest)) = path.rsplit_once("/blobs/") {
        return match (oci_name(name), oci_digest(digest)) {
            // Layers are plain files of the snapshot: range requests and
            // conditional requests work as for any other file.
            (true, Some(hex)) => Route::Mapped { path: format!("blobs/sha256/{hex}"), wish: None },
            _ => unknown("BLOB_UNKNOWN", "blob unknown to registry"),
        };
    }
    let Some((name, reference)) = path.rsplit_once("/manifests/") else {
        return unknown("UNSUPPORTED", "not a registry API path");
    };
    if !oci_name(name) {
        return unknown("NAME_INVALID", "invalid repository name");
    }
    // A tag is a small file holding the digest it points at.
    let hex = match oci_digest(reference) {
        Some(hex) => hex.to_string(),
        None if oci_tag(reference) => match small_blob(app, repo, &format!("tags/{name}/{reference}")) {
            Ok(Some(body)) => match std::str::from_utf8(&body).ok().map(str::trim).and_then(oci_digest) {
                Some(hex) => hex.to_string(),
                None => return internal(),
            },
            Ok(None) => {
                // An image or tag that is not mirrored: a wish.
                app.metrics.misses.fetch_add(1, Ordering::Relaxed);
                let _ = app.st.lookup_published(&repo.name, &format!("images/{name}:{reference}"), true);
                return unknown("MANIFEST_UNKNOWN", "manifest unknown");
            }
            Err(()) => return internal(),
        },
        None => return unknown("MANIFEST_INVALID", "invalid reference"),
    };
    match small_blob(app, repo, &format!("manifests/{name}/sha256/{hex}")) {
        Ok(Some(body)) => {
            // The media type is part of the manifest; older ones leave it out.
            let doc: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            let media_type = doc["mediaType"].as_str().filter(|m| m.is_ascii() && !m.contains(['\r', '\n'])).map(str::to_string).unwrap_or_else(|| {
                if doc["manifests"].is_array() { "application/vnd.oci.image.index.v1+json" } else { "application/vnd.oci.image.manifest.v1+json" }.to_string()
            });
            Route::Done(
                Reply::bytes(200, &media_type, body)
                    .with("Docker-Content-Digest", format!("sha256:{hex}"))
                    .with("ETag", format!("\"sha256:{hex}\"")),
            )
        }
        Ok(None) => unknown("MANIFEST_UNKNOWN", "manifest unknown"),
        Err(()) => internal(),
    }
}

// ----------------------------------------------------------------- cargo

fn cargo_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// Cargo starts at `index/config.json`, which tells it where crate files
/// are. That address must be absolute, so the file is written here from the
/// address the client used. Then it reads one index file per crate.
fn cargo(req: &Request, tls: bool, repo: &Repo, path: &str) -> Route {
    let not_found = || Route::Done(Reply::json(404, json!({ "error": "not found" })));
    if path == "index/config.json" {
        let Some(base) = own_base(req, tls, repo) else {
            return Route::Done(Reply::json(400, json!({ "error": "a Host header is required" })));
        };
        let mut config = json!({ "dl": format!("{base}crates/{{crate}}/{{crate}}-{{version}}.crate") });
        if repo.private {
            config["auth-required"] = json!(true);
        }
        return Route::Done(Reply::json(200, config));
    }
    if let Some(rest) = path.strip_prefix("index/") {
        // An index file that is missing is a crate somebody wants.
        return match rest.rsplit('/').next() {
            Some(name) if cargo_name(name) && rest.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'/')) => {
                Route::Mapped { path: path.to_ascii_lowercase(), wish: Some(name.to_ascii_lowercase()) }
            }
            _ => not_found(),
        };
    }
    if path.starts_with("crates/") && path.ends_with(".crate") {
        return Route::Mapped { path: path.to_string(), wish: None };
    }
    not_found()
}

// -------------------------------------------------------------------- go

/// The go command asks for `<module>/@v/list`, `<module>/@v/<version>.info`,
/// `.mod`, `.zip` and `<module>/@latest`, which is how the files are stored.
/// A version that is missing is a wish, named `module@version`; a module that
/// is not mirrored at all is a wish under its own name.
///
/// To find which module a package belongs to, the go command also asks for
/// every shorter path (`github.com/owner`, `github.com`). Those are not
/// modules, and are not recorded.
fn go(raw: &str) -> Route {
    let not_found = || Route::Done(Reply::bytes(404, "text/plain", b"not found\n".to_vec()));
    let Some(path) = percent_decode(raw) else { return not_found() };
    // Upper case is written as "!" and the lower-case letter.
    let unescape = |s: &str| {
        let mut out = String::new();
        let mut bang = false;
        for c in s.chars() {
            match (bang, c) {
                (false, '!') => bang = true,
                (true, c) => {
                    out.push(c.to_ascii_uppercase());
                    bang = false;
                }
                (false, c) => out.push(c),
            }
        }
        out
    };
    let safe = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/' | b'!' | b'+'));
    let could_be_module = |module: &str| {
        let segments = module.split('/').count();
        let hosted = ["github.com/", "gitlab.com/", "bitbucket.org/", "golang.org/x/"].iter().any(|h| module.starts_with(h));
        segments >= if hosted { 3 } else { 2 } || (module.starts_with("gopkg.in/") && segments == 2)
    };
    let wish = |module: &str, version: Option<&str>| {
        could_be_module(module).then(|| match version {
            Some(v) => format!("{}@{}", unescape(module), unescape(v)),
            None => unescape(module),
        })
    };
    if let Some(module) = path.strip_suffix("/@latest") {
        return if safe(module) { Route::Mapped { wish: wish(module, None), path } } else { not_found() };
    }
    let Some((module, file)) = path.split_once("/@v/") else { return not_found() };
    if !safe(module) || !safe(file) || file.contains('/') {
        return not_found();
    }
    let version = [".info", ".mod", ".zip"].iter().find_map(|ext| file.strip_suffix(ext));
    match version {
        Some(v) => Route::Mapped { wish: wish(module, Some(v)), path },
        None if file == "list" => Route::Mapped { wish: wish(module, None), path },
        None => not_found(),
    }
}

// ------------------------------------------------------------------ helm

/// A chart repository is two kinds of file: the index and chart archives.
/// A chart archive that is missing is a wish, named `<chart>-<version>`.
fn helm(path: &str) -> Route {
    match path.strip_prefix("charts/").and_then(|f| f.strip_suffix(".tgz")) {
        Some(chart) if !chart.is_empty() && !chart.contains('/') => Route::Mapped { path: path.to_string(), wish: Some(format!("charts/{chart}")) },
        _ => Route::Mapped { path: path.to_string(), wish: None },
    }
}

pub(crate) fn route(app: &App, req: &Request, tls: bool, repo: &Repo, path: &str) -> Route {
    match repo.format.as_str() {
        "pypi" => pypi(repo, path),
        "npm" => npm(app, req, tls, repo, path),
        "oci" => oci(app, repo, path),
        "nuget" => nuget(app, req, tls, repo, path),
        "helm" => helm(path),
        "cargo" => cargo(req, tls, repo, path),
        "go" => go(path),
        _ => Route::Unhandled,
    }
}
