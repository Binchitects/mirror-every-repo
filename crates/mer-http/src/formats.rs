//! Requests that package clients make which are not plain file paths.
//!
//! Most formats are served as the files of the snapshot. A few client
//! conventions need translating first; that translation lives here so the
//! snapshot stays a plain tree of files.
use mer_meta::Repo;

pub(crate) enum Route {
    /// Serve the path as it was asked for.
    Unhandled,
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

pub(crate) fn route(repo: &Repo, path: &str) -> Route {
    match repo.format.as_str() {
        "pypi" => pypi(repo, path),
        _ => Route::Unhandled,
    }
}
