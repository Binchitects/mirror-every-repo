//! Reading stored paths as packages, for the panel.
//!
//! Each format keeps its files under a layout of its own. This module knows
//! those layouts well enough to say "this file is version 1.2.3 of package
//! foo", so the panel can list packages instead of paths, summarise a
//! snapshot by package, and say what to add on the collector for a wish.

/// (package, version) of a stored file. `None` for index files, signatures
/// and anything else that is not itself a package.
pub(crate) fn package_of(format: &str, path: &str) -> Option<(String, String)> {
    let own = |name: &str, version: &str| (!name.is_empty() && !version.is_empty()).then(|| (name.to_string(), version.to_string()));
    match format {
        "apt" => {
            // pool/main/c/curl/curl_8.5.0-2_amd64.deb
            let file = path.strip_prefix("pool/")?.rsplit('/').next()?;
            let stem = file.strip_suffix(".deb").or_else(|| file.strip_suffix(".udeb"))?;
            let mut parts = stem.split('_');
            let (name, version) = (parts.next()?, parts.next()?);
            own(name, &version.replace("%3a", ":").replace("%3A", ":"))
        }
        "pypi" => {
            // packages/requests/requests-2.32.3-py3-none-any.whl, .../requests-2.32.3.tar.gz
            let (name, file) = path.strip_prefix("packages/")?.split_once('/')?;
            if let Some(stem) = file.strip_suffix(".whl") {
                return own(name, stem.split('-').nth(1)?);
            }
            let stem = [".tar.gz", ".zip", ".tar.bz2", ".tgz"].iter().find_map(|ext| file.strip_suffix(ext))?;
            own(name, stem.rsplit_once('-')?.1)
        }
        "npm" => {
            // tarballs/express/express-4.19.2.tgz, tarballs/@types/node/node-20.1.0.tgz
            let (name, file) = path.strip_prefix("tarballs/")?.rsplit_once('/')?;
            let base = name.rsplit('/').next()?;
            own(name, file.strip_suffix(".tgz")?.strip_prefix(base)?.strip_prefix('-')?)
        }
        "nuget" => {
            // flat/serilog/4.0.0/serilog.4.0.0.nupkg
            let mut parts = path.strip_prefix("flat/")?.split('/');
            let (id, version, file) = (parts.next()?, parts.next()?, parts.next()?);
            file.ends_with(".nupkg").then(|| own(id, version)).flatten()
        }
        "oci" => {
            // tags/library/alpine/3.20
            let (name, tag) = path.strip_prefix("tags/")?.rsplit_once('/')?;
            own(name, tag)
        }
        "helm" => {
            // charts/cert-manager-1.16.5.tgz: the version starts at the first "-<digit>".
            let stem = path.strip_prefix("charts/")?.strip_suffix(".tgz")?;
            let at = stem.char_indices().find(|&(i, c)| c == '-' && stem[i + 1..].starts_with(|d: char| d.is_ascii_digit())).map(|(i, _)| i)?;
            own(&stem[..at], &stem[at + 1..])
        }
        "cargo" => {
            // crates/serde/serde-1.0.200.crate
            let (name, file) = path.strip_prefix("crates/")?.split_once('/')?;
            own(name, file.strip_suffix(".crate")?.strip_prefix(name)?.strip_prefix('-')?)
        }
        "go" => {
            // github.com/!azure/sdk/@v/v1.2.3.zip
            let (module, file) = path.split_once("/@v/")?;
            own(&go_unescape(module), &go_unescape(file.strip_suffix(".zip")?))
        }
        _ => None,
    }
}

fn go_unescape(s: &str) -> String {
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
}

/// What to add to the sync command on the collector so that a wish is
/// fulfilled. `None` where the wish is a plain file.
pub(crate) fn wish_hint(format: &str, wish: &str) -> Option<String> {
    match format {
        "pypi" => Some(format!("-r {}", wish.strip_prefix("simple/")?)),
        "npm" => match wish.strip_prefix("tarballs/") {
            Some(t) => package_of("npm", &format!("tarballs/{t}")).map(|(n, v)| format!("-p {n}@{v}")),
            None => Some(format!("-p {wish}")),
        },
        "nuget" => Some(match wish.split_once('/') {
            Some((id, version)) => format!("-p {id}@{version}"),
            None => format!("-p {wish}"),
        }),
        "oci" => Some(format!("-i {}", wish.strip_prefix("images/")?)),
        "helm" => package_of("helm", &format!("{wish}.tgz")).map(|(n, v)| format!("-c {n}@{v}")),
        "cargo" => Some(format!("-p {wish}")),
        "go" => Some(format!("-m {wish}")),
        "apt" => package_of("apt", wish).map(|(n, _)| format!("--package {n}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_paths_are_read_as_packages() {
        let cases = [
            ("apt", "pool/main/c/curl/curl_8.5.0-2_amd64.deb", Some(("curl", "8.5.0-2"))),
            ("apt", "pool/main/g/git/git_1%3a2.39.5-0+deb12u2_amd64.deb", Some(("git", "1:2.39.5-0+deb12u2"))),
            ("apt", "dists/bookworm/InRelease", None),
            ("pypi", "packages/requests/requests-2.32.3-py3-none-any.whl", Some(("requests", "2.32.3"))),
            ("pypi", "packages/charset-normalizer/charset_normalizer-3.4.0.tar.gz", Some(("charset-normalizer", "3.4.0"))),
            ("pypi", "packages/requests/requests-2.32.3-py3-none-any.whl.metadata", None),
            ("pypi", "simple/requests/index.html", None),
            ("npm", "tarballs/express/express-4.19.2.tgz", Some(("express", "4.19.2"))),
            ("npm", "tarballs/@types/node/node-20.1.0.tgz", Some(("@types/node", "20.1.0"))),
            ("npm", "tarballs/left-pad/left-pad-1.3.0-beta.1.tgz", Some(("left-pad", "1.3.0-beta.1"))),
            ("npm", "packuments/express.json", None),
            ("nuget", "flat/serilog/4.0.0/serilog.4.0.0.nupkg", Some(("serilog", "4.0.0"))),
            ("nuget", "flat/serilog/index.json", None),
            ("oci", "tags/library/alpine/3.20", Some(("library/alpine", "3.20"))),
            ("oci", "blobs/sha256/abcd", None),
            ("helm", "charts/cert-manager-1.16.5.tgz", Some(("cert-manager", "1.16.5"))),
            ("helm", "charts/ingress-nginx-4.15.1.tgz", Some(("ingress-nginx", "4.15.1"))),
            ("helm", "index.yaml", None),
            ("cargo", "crates/serde_json/serde_json-1.0.140.crate", Some(("serde_json", "1.0.140"))),
            ("cargo", "index/se/rd/serde", None),
            ("go", "github.com/!azure/sdk/@v/v1.2.3.zip", Some(("github.com/Azure/sdk", "v1.2.3"))),
            ("go", "github.com/gin-gonic/gin/@v/v1.10.0.mod", None),
            ("raw", "tools/a.txt", None),
        ];
        for (format, path, want) in cases {
            assert_eq!(package_of(format, path), want.map(|(n, v)| (n.to_string(), v.to_string())), "{format} {path}");
        }
    }

    #[test]
    fn wishes_are_turned_into_what_to_add_on_the_collector() {
        let cases = [
            ("pypi", "simple/flask", Some("-r flask")),
            ("npm", "left-pad", Some("-p left-pad")),
            ("npm", "tarballs/left-pad/left-pad-1.3.0.tgz", Some("-p left-pad@1.3.0")),
            ("nuget", "dapper", Some("-p dapper")),
            ("nuget", "dapper/2.1.35", Some("-p dapper@2.1.35")),
            ("oci", "images/library/busybox:latest", Some("-i library/busybox:latest")),
            ("helm", "charts/cert-manager-1.12.0", Some("-c cert-manager@1.12.0")),
            ("cargo", "rand", Some("-p rand")),
            ("go", "github.com/google/uuid", Some("-m github.com/google/uuid")),
            ("go", "github.com/google/uuid@v1.6.0", Some("-m github.com/google/uuid@v1.6.0")),
            ("apt", "pool/main/h/htop/htop_3.2.2-2_amd64.deb", Some("--package htop")),
            ("raw", "tools/missing.tgz", None),
        ];
        for (format, wish, want) in cases {
            assert_eq!(wish_hint(format, wish).as_deref(), want, "{format} {wish}");
        }
    }
}
