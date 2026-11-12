//! Semantic versions and npm's range syntax, as node-semver implements them.
use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Pre-release identifiers, for example `["beta", "2"]`.
    pub pre: Vec<String>,
}

fn numeric(s: &str) -> Option<u64> {
    // No leading zeros, and within JavaScript's safe integer range.
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) || (s.len() > 1 && s.starts_with('0')) {
        return None;
    }
    s.parse::<u64>().ok().filter(|n| *n <= 9_007_199_254_740_991)
}

fn valid_ident(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl Version {
    /// Strict `1.2.3[-pre][+build]`, with an optional leading `v`.
    pub fn parse(text: &str) -> Option<Version> {
        let t = text.trim();
        let t = t.strip_prefix('v').unwrap_or(t);
        if t.len() > 256 {
            return None;
        }
        let (core_pre, build) = match t.split_once('+') {
            Some((c, b)) => (c, Some(b)),
            None => (t, None),
        };
        if let Some(b) = build {
            if !b.split('.').all(valid_ident) {
                return None;
            }
        }
        let (core, pre) = match core_pre.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (core_pre, None),
        };
        let mut nums = core.split('.');
        let (major, minor, patch) = (numeric(nums.next()?)?, numeric(nums.next()?)?, numeric(nums.next()?)?);
        if nums.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(p) => {
                let ids: Vec<String> = p.split('.').map(str::to_string).collect();
                // Numeric identifiers must not have leading zeros.
                if !ids.iter().all(|i| valid_ident(i) && (!i.bytes().all(|b| b.is_ascii_digit()) || numeric(i).is_some())) {
                    return None;
                }
                ids
            }
        };
        Some(Version { major, minor, patch, pre })
    }

    fn tuple(&self) -> (u64, u64, u64) {
        (self.major, self.minor, self.patch)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.tuple().cmp(&other.tuple()).then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater, // a release is newer than its pre-releases
            (false, true) => Ordering::Less,
            (false, false) => {
                for (a, b) in self.pre.iter().zip(&other.pre) {
                    let ord = match (numeric(a), numeric(b)) {
                        (Some(x), Some(y)) => x.cmp(&y),
                        (Some(_), None) => Ordering::Less, // numbers sort before words
                        (None, Some(_)) => Ordering::Greater,
                        (None, None) => a.cmp(b),
                    };
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                self.pre.len().cmp(&other.pre.len())
            }
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Comparator {
    op: Op,
    v: Version,
}

impl Comparator {
    fn test(&self, v: &Version) -> bool {
        let ord = v.cmp(&self.v);
        match self.op {
            Op::Lt => ord == Ordering::Less,
            Op::Le => ord != Ordering::Greater,
            Op::Gt => ord == Ordering::Greater,
            Op::Ge => ord != Ordering::Less,
            Op::Eq => ord == Ordering::Equal,
        }
    }
}

/// A range: alternatives (`||`), each a list of comparators that must all hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Range(Vec<Vec<Comparator>>);

/// `1`, `1.2`, `1.2.3`, with `x`, `X` or `*` as wildcards, and an optional pre-release.
#[derive(Clone, Debug)]
struct Partial {
    major: Option<u64>,
    minor: Option<u64>,
    patch: Option<u64>,
    pre: Vec<String>,
}

fn parse_partial(text: &str) -> Option<Partial> {
    let t = text.strip_prefix('v').unwrap_or(text);
    let t = t.split('+').next()?; // build metadata is ignored
    let (core, pre) = match t.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (t, None),
    };
    let mut parts = [None, None, None];
    let mut wild = false;
    let pieces: Vec<&str> = if core.is_empty() { Vec::new() } else { core.split('.').collect() };
    if pieces.len() > 3 {
        return None;
    }
    for (i, piece) in pieces.iter().enumerate() {
        if matches!(*piece, "x" | "X" | "*") {
            wild = true;
        } else if wild {
            numeric(piece)?; // "1.x.3": must be a number, but is ignored
        } else {
            parts[i] = Some(numeric(piece)?);
        }
    }
    let pre = match pre {
        Some(p) if parts[2].is_some() => {
            let ids: Vec<String> = p.split('.').map(str::to_string).collect();
            if !ids.iter().all(|i| valid_ident(i) && (!i.bytes().all(|b| b.is_ascii_digit()) || numeric(i).is_some())) {
                return None;
            }
            ids
        }
        Some(_) => return None,
        None => Vec::new(),
    };
    Some(Partial { major: parts[0], minor: parts[1], patch: parts[2], pre })
}

fn ver(major: u64, minor: u64, patch: u64) -> Version {
    Version { major, minor, patch, pre: Vec::new() }
}

/// The lowest possible pre-release of a version: `<2.0.0-0` keeps out `2.0.0-alpha`.
fn floor(major: u64, minor: u64, patch: u64) -> Version {
    Version { major, minor, patch, pre: vec!["0".into()] }
}

fn ge(v: Version) -> Comparator {
    Comparator { op: Op::Ge, v }
}
fn lt(v: Version) -> Comparator {
    Comparator { op: Op::Lt, v }
}

/// Turn one token (`^1.2.3`, `>=1.2`, `1.x`, `~1`) into primitive comparators.
/// `None` in the result list position means "matches nothing".
fn desugar(token: &str) -> Option<Vec<Comparator>> {
    let (op, rest) = ["^", "~>", "~", ">=", "<=", ">", "<", "="]
        .into_iter()
        .find_map(|op| token.strip_prefix(op).map(|r| (op, r)))
        .unwrap_or(("", token));
    let p = parse_partial(rest)?;
    let (m, n, pt) = (p.major, p.minor, p.patch);
    Some(match op {
        "^" => match (m, n, pt) {
            (None, _, _) => vec![ge(ver(0, 0, 0))],
            (Some(m), None, _) => vec![ge(ver(m, 0, 0)), lt(floor(m + 1, 0, 0))],
            (Some(m), Some(n), None) => {
                if m == 0 {
                    vec![ge(ver(m, n, 0)), lt(floor(m, n + 1, 0))]
                } else {
                    vec![ge(ver(m, n, 0)), lt(floor(m + 1, 0, 0))]
                }
            }
            (Some(m), Some(n), Some(pt)) => {
                let low = Version { major: m, minor: n, patch: pt, pre: p.pre.clone() };
                let high = if m > 0 {
                    floor(m + 1, 0, 0)
                } else if n > 0 {
                    floor(0, n + 1, 0)
                } else {
                    floor(0, 0, pt + 1)
                };
                vec![ge(low), lt(high)]
            }
        },
        "~" | "~>" => match (m, n, pt) {
            (None, _, _) => vec![ge(ver(0, 0, 0))],
            (Some(m), None, _) => vec![ge(ver(m, 0, 0)), lt(floor(m + 1, 0, 0))],
            (Some(m), Some(n), None) => vec![ge(ver(m, n, 0)), lt(floor(m, n + 1, 0))],
            (Some(m), Some(n), Some(pt)) => {
                vec![ge(Version { major: m, minor: n, patch: pt, pre: p.pre.clone() }), lt(floor(m, n + 1, 0))]
            }
        },
        // Plain comparators and x-ranges. A partial version widens or
        // narrows depending on the operator.
        _ => match (m, n, pt) {
            (None, _, _) => match op {
                "<" | ">" => vec![lt(floor(0, 0, 0))], // "<*" and ">*" match nothing
                _ => vec![ge(ver(0, 0, 0))],
            },
            (Some(m), None, _) => match op {
                ">" => vec![ge(ver(m + 1, 0, 0))],
                "<=" => vec![lt(floor(m + 1, 0, 0))],
                "<" => vec![lt(floor(m, 0, 0))],
                ">=" => vec![ge(ver(m, 0, 0))],
                _ => vec![ge(ver(m, 0, 0)), lt(floor(m + 1, 0, 0))],
            },
            (Some(m), Some(n), None) => match op {
                ">" => vec![ge(ver(m, n + 1, 0))],
                "<=" => vec![lt(floor(m, n + 1, 0))],
                "<" => vec![lt(floor(m, n, 0))],
                ">=" => vec![ge(ver(m, n, 0))],
                _ => vec![ge(ver(m, n, 0)), lt(floor(m, n + 1, 0))],
            },
            (Some(m), Some(n), Some(pt)) => {
                let v = Version { major: m, minor: n, patch: pt, pre: p.pre.clone() };
                let op = match op {
                    ">" => Op::Gt,
                    ">=" => Op::Ge,
                    "<" => Op::Lt,
                    "<=" => Op::Le,
                    _ => Op::Eq,
                };
                vec![Comparator { op, v }]
            }
        },
    })
}

/// `1.2.3 - 2.3`: inclusive on both ends; a partial upper end covers its whole range.
fn hyphen(low: &str, high: &str) -> Option<Vec<Comparator>> {
    let (l, h) = (parse_partial(low)?, parse_partial(high)?);
    let mut out = Vec::new();
    match (l.major, l.minor, l.patch) {
        (None, _, _) => {}
        (Some(m), None, _) => out.push(ge(ver(m, 0, 0))),
        (Some(m), Some(n), None) => out.push(ge(ver(m, n, 0))),
        (Some(m), Some(n), Some(p)) => out.push(ge(Version { major: m, minor: n, patch: p, pre: l.pre.clone() })),
    }
    match (h.major, h.minor, h.patch) {
        (None, _, _) => {}
        (Some(m), None, _) => out.push(lt(floor(m + 1, 0, 0))),
        (Some(m), Some(n), None) => out.push(lt(floor(m, n + 1, 0))),
        (Some(m), Some(n), Some(p)) => out.push(Comparator { op: Op::Le, v: Version { major: m, minor: n, patch: p, pre: h.pre.clone() } }),
    }
    if out.is_empty() {
        out.push(ge(ver(0, 0, 0)));
    }
    Some(out)
}

impl Range {
    pub fn parse(text: &str) -> Option<Range> {
        if text.len() > 256 {
            return None;
        }
        let mut alternatives = Vec::new();
        for alt in text.split("||") {
            // "> = 1.2.3" and ">= 1.2.3" are the same as ">=1.2.3".
            let mut tokens: Vec<String> = Vec::new();
            let mut pending = String::new();
            for word in alt.split_whitespace() {
                if word.bytes().all(|b| b"<>=~^".contains(&b)) {
                    pending.push_str(word);
                } else {
                    tokens.push(format!("{pending}{word}"));
                    pending.clear();
                }
            }
            if !pending.is_empty() {
                return None;
            }
            let comparators = if tokens.len() == 3 && tokens[1] == "-" {
                hyphen(&tokens[0], &tokens[2])?
            } else if tokens.is_empty() {
                vec![ge(ver(0, 0, 0))]
            } else {
                let mut all = Vec::new();
                for t in &tokens {
                    if t == "-" {
                        return None;
                    }
                    all.extend(desugar(t)?);
                }
                all
            };
            alternatives.push(comparators);
        }
        Some(Range(alternatives))
    }

    pub fn satisfied_by(&self, v: &Version) -> bool {
        self.0.iter().any(|set| {
            if !set.iter().all(|c| c.test(v)) {
                return false;
            }
            // A pre-release only counts if the range itself names a
            // pre-release of the very same major.minor.patch.
            v.pre.is_empty() || set.iter().any(|c| !c.v.pre.is_empty() && c.v.tuple() == v.tuple())
        })
    }

    /// The highest of `versions` that satisfies this range.
    pub fn max_satisfying<'a>(&self, versions: impl IntoIterator<Item = &'a Version>) -> Option<&'a Version> {
        versions.into_iter().filter(|v| self.satisfied_by(v)).max()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("{s} should parse"))
    }
    fn sat(range: &str, version: &str) -> bool {
        Range::parse(range).unwrap_or_else(|| panic!("{range:?} should parse")).satisfied_by(&v(version))
    }

    #[test]
    fn precedence_examples_from_the_semver_spec() {
        let order = ["1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0", "2.0.0", "2.1.0", "2.1.1"];
        for pair in order.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert_eq!(v("1.0.0+build.5").cmp(&v("1.0.0")), Ordering::Equal, "build metadata is ignored");
        assert_eq!(v("v1.2.3").to_string(), "1.2.3");
        for bad in ["", "1", "1.2", "1.2.3.4", "01.2.3", "1.2.3-", "1.2.3-01", "a.b.c", "1.2.x", "1.2.3-a..b", "1.2.3+"] {
            assert!(Version::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn range_rules() {
        let yes = [
            ("^1.2.3", "1.9.0"), ("^0.2.3", "0.2.9"), ("^0.0.3", "0.0.3"), ("~1.2.3", "1.2.9"), ("~1.2", "1.2.0"), ("~1", "1.9.9"),
            ("1.x", "1.5.0"), ("1.2.x", "1.2.7"), ("*", "0.0.1"), ("", "5.0.0"), (">=1.2.3 <2", "1.5.0"), ("1.2.3 - 2.3.4", "2.3.4"),
            ("1.2.3 - 2.3", "2.3.9"), ("1 - 2", "2.9.9"), ("<1.0.0 || >=2.0.0", "2.1.0"), (">= 1.2.3", "1.2.3"), ("=1.2.3", "1.2.3"),
            ("^1.2.3-beta.2", "1.2.3-beta.4"), ("^1.2.3-beta.2", "1.3.0"), (">1.2", "1.3.0"), ("<=1.2", "1.2.9"), ("^1.x", "1.0.0"), ("~>1.2", "1.2.5"),
        ];
        let no = [
            ("^1.2.3", "2.0.0"), ("^1.2.3", "1.2.2"), ("^0.2.3", "0.3.0"), ("^0.0.3", "0.0.4"), ("~1.2.3", "1.3.0"), ("~1", "2.0.0"),
            ("1.x", "2.0.0"), ("*", "1.0.0-beta"), ("^1.2.3", "1.3.0-beta"), ("^1.2.3-beta.2", "1.2.4-beta.1"), ("<2.0.0", "2.0.0-rc.1"),
            ("1.2.3 - 2.3.4", "2.3.5"), (">1.2", "1.2.9"), ("<1.2", "1.2.0"), ("<1.0.0 || >=2.0.0", "1.5.0"), (">1.2.3 <1.2.3", "1.2.3"),
        ];
        for (r, ver) in yes {
            assert!(sat(r, ver), "{r:?} should accept {ver}");
        }
        for (r, ver) in no {
            assert!(!sat(r, ver), "{r:?} should refuse {ver}");
        }
        for bad in ["not a range", ">=", "^", "1.2.3 -", "1.2.3.4", "latest", "git+https://x/y.git", "file:../x"] {
            assert!(Range::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn max_satisfying() {
        let all: Vec<Version> = ["1.0.0", "1.2.0", "1.9.3", "2.0.0-rc.1", "2.0.0", "2.1.0"].iter().map(|s| v(s)).collect();
        let pick = |r: &str| Range::parse(r).unwrap().max_satisfying(&all).map(|v| v.to_string());
        assert_eq!(pick("^1.0.0").as_deref(), Some("1.9.3"));
        assert_eq!(pick("*").as_deref(), Some("2.1.0"));
        assert_eq!(pick("<2").as_deref(), Some("1.9.3"));
        assert_eq!(pick("^2.0.0-rc.0").as_deref(), Some("2.1.0"));
        assert_eq!(pick("^3").as_deref(), None);
    }
}
