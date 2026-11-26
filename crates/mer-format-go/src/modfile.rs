//! Module paths, the proxy's case escaping, and the parts of `go.mod` and
//! `go.sum` the mirror needs.
use crate::version;

/// A module path as Go accepts it in a proxy request: slash-separated
/// elements of letters, digits and `-._~`, no empty, `.` or `..` element.
pub fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 512
        && path.split('/').all(|e| {
            !e.is_empty() && e != "." && e != ".." && !e.starts_with('.') && !e.ends_with('.') && e.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
        })
        && path.split('/').next().is_some_and(|first| first.contains('.') || !path.contains('/'))
}

/// The proxy protocol's case escaping: `Azure` -> `!azure`.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('!');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The reverse of [`escape`]; `None` for text that is not an escaped form
/// (upper case, or `!` not followed by a lower-case letter).
pub fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut bang = false;
    for c in s.chars() {
        if bang {
            if !c.is_ascii_lowercase() {
                return None;
            }
            out.push(c.to_ascii_uppercase());
            bang = false;
        } else if c == '!' {
            bang = true;
        } else if c.is_ascii_uppercase() {
            return None;
        } else {
            out.push(c);
        }
    }
    (!bang).then_some(out)
}

/// What a `go.mod` file says that matters here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoMod {
    pub module: Option<String>,
    pub go: Option<String>,
    pub requires: Vec<(String, String)>,
    /// Retracted versions and `[low, high]` intervals.
    pub retract: Vec<(String, String)>,
    pub replaces: usize,
}

impl GoMod {
    pub fn retracted(&self, v: &str) -> bool {
        self.retract.iter().any(|(lo, hi)| version::compare(v, lo).is_ge() && version::compare(v, hi).is_le())
    }
}

fn unquote(t: &str) -> String {
    t.trim_matches(|c| c == '"' || c == '`').to_string()
}

fn tokens(line: &str) -> Vec<String> {
    let line = line.split("//").next().unwrap_or("");
    line.split_whitespace().map(unquote).collect()
}

pub fn parse_gomod(text: &str) -> GoMod {
    let mut m = GoMod::default();
    let mut block: Option<String> = None;
    let add = |m: &mut GoMod, verb: &str, args: &[String]| match verb {
        "module" => m.module = args.first().cloned(),
        "go" => m.go = args.first().cloned(),
        "require" if args.len() >= 2 => m.requires.push((args[0].clone(), args[1].clone())),
        "replace" => m.replaces += 1,
        "retract" => {
            let joined = args.join(" ");
            if let Some(inner) = joined.strip_prefix('[').and_then(|r| r.split(']').next()) {
                if let Some((lo, hi)) = inner.split_once(',') {
                    m.retract.push((lo.trim().to_string(), hi.trim().to_string()));
                }
            } else if let Some(v) = args.first() {
                m.retract.push((v.clone(), v.clone()));
            }
        }
        _ => {}
    };
    for raw in text.lines() {
        let t = tokens(raw);
        if let Some(verb) = &block {
            if t.first().map(String::as_str) == Some(")") {
                block = None;
            } else if !t.is_empty() {
                let verb = verb.clone();
                add(&mut m, &verb, &t);
            }
            continue;
        }
        let Some(verb) = t.first() else { continue };
        if t.get(1).map(String::as_str) == Some("(") {
            block = Some(verb.clone());
        } else {
            let verb = verb.clone();
            add(&mut m, &verb, &t[1..]);
        }
    }
    m
}

/// One `go.sum` line: module, version, whether it is the `/go.mod` hash, the hash.
pub fn parse_gosum(text: &str) -> Result<Vec<(String, String, bool, String)>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.is_empty() {
            continue;
        }
        let [path, ver, hash] = f[..] else { return Err(format!("go.sum line {} is not understood", n + 1)) };
        let (ver, gomod) = match ver.strip_suffix("/go.mod") {
            Some(v) => (v, true),
            None => (ver, false),
        };
        if !hash.starts_with("h1:") || !valid_path(path) || !version::is_canonical(ver) {
            return Err(format!("go.sum line {} is not understood", n + 1));
        }
        out.push((path.to_string(), ver.to_string(), gomod, hash.to_string()));
    }
    Ok(out)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn paths_and_escaping() {
        for ok in ["rsc.io/quote", "github.com/BurntSushi/toml", "golang.org/x/text", "gopkg.in/yaml.v3", "github.com/a/b/v2", "example.com"] {
            assert!(valid_path(ok), "{ok}");
        }
        for bad in ["", "/x", "x/", "a//b", "a/../b", "x.com/.git", "x.com/a b", "x.com/a!b", "nodots/pkg", "x.com/\u{e9}"] {
            assert!(!valid_path(bad), "{bad:?}");
        }
        assert_eq!(escape("github.com/BurntSushi/toml"), "github.com/!burnt!sushi/toml");
        assert_eq!(unescape("github.com/!burnt!sushi/toml").as_deref(), Some("github.com/BurntSushi/toml"));
        assert_eq!(unescape("github.com/Burnt"), None);
        assert_eq!(unescape("x!"), None);
        assert_eq!(unescape("x!1"), None);
    }

    #[test]
    fn gomod_and_gosum() {
        let m = parse_gomod(
            "module example.com/app // the app\n\ngo 1.21\n\nrequire (\n\tgithub.com/spf13/cobra v1.8.1\n\tgolang.org/x/text v0.3.0 // indirect\n)\nrequire \"rsc.io/quote\" v1.5.2\nreplace a.com/b => ../b\nretract v1.0.1\nretract (\n\t[v1.1.0, v1.2.0] // broken\n\tv1.3.0\n)\n",
        );
        assert_eq!(m.module.as_deref(), Some("example.com/app"));
        assert_eq!(m.go.as_deref(), Some("1.21"));
        assert_eq!(m.requires, [("github.com/spf13/cobra".into(), "v1.8.1".into()), ("golang.org/x/text".into(), "v0.3.0".into()), ("rsc.io/quote".into(), "v1.5.2".into())]);
        assert_eq!(m.replaces, 1);
        for (v, r) in [("v1.0.1", true), ("v1.0.2", false), ("v1.1.5", true), ("v1.2.0", true), ("v1.3.0", true), ("v1.4.0", false)] {
            assert_eq!(m.retracted(v), r, "{v}");
        }
        let s = parse_gosum("rsc.io/quote v1.5.2 h1:abc=\nrsc.io/quote v1.5.2/go.mod h1:def=\n\n").unwrap();
        assert_eq!(s, [("rsc.io/quote".into(), "v1.5.2".into(), false, "h1:abc=".into()), ("rsc.io/quote".into(), "v1.5.2".into(), true, "h1:def=".into())]);
        assert!(parse_gosum("rsc.io/quote v1.5.2").is_err());
        assert!(parse_gosum("rsc.io/quote v1.5.2 md5:x").is_err());
    }
}
