//! Go module versions, ordered as `golang.org/x/mod/semver` orders them.
use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Parsed<'a> {
    major: &'a str,
    minor: &'a str,
    patch: &'a str,
    pre: &'a str,
}

fn number(v: &str) -> Option<(&str, &str)> {
    let end = v.bytes().position(|b| !b.is_ascii_digit()).unwrap_or(v.len());
    let (n, rest) = v.split_at(end);
    (!n.is_empty() && !(n.len() > 1 && n.starts_with('0'))).then_some((n, rest))
}

/// Dot-separated identifiers of `[0-9A-Za-z-]`; numeric ones without leading zeros when `numeric_rule`.
fn idents(s: &str, numeric_rule: bool) -> bool {
    s.split('.').all(|id| {
        !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !(numeric_rule && id.len() > 1 && id.bytes().all(|b| b.is_ascii_digit()) && id.starts_with('0'))
    })
}

fn parse(v: &str) -> Option<Parsed<'_>> {
    let v = v.strip_prefix('v')?;
    let (major, v) = number(v)?;
    if v.is_empty() {
        return Some(Parsed { major, minor: "0", patch: "0", pre: "" });
    }
    let (minor, v) = number(v.strip_prefix('.')?)?;
    if v.is_empty() {
        return Some(Parsed { major, minor, patch: "0", pre: "" });
    }
    let (patch, mut v) = number(v.strip_prefix('.')?)?;
    let mut pre = "";
    if let Some(rest) = v.strip_prefix('-') {
        let end = rest.find('+').unwrap_or(rest.len());
        pre = &v[..end + 1];
        if !idents(&rest[..end], true) {
            return None;
        }
        v = &rest[end..];
    }
    if let Some(build) = v.strip_prefix('+') {
        if !idents(build, false) {
            return None;
        }
        v = "";
    }
    v.is_empty().then_some(Parsed { major, minor, patch, pre })
}

/// `vMAJOR[.MINOR[.PATCH[-PRE][+BUILD]]]`, as Go accepts it.
pub fn is_valid(v: &str) -> bool {
    parse(v).is_some()
}

/// A full `vX.Y.Z[-pre][+build]` version, the only form go.mod files and proxies use.
pub fn is_canonical(v: &str) -> bool {
    parse(v).is_some() && v[1..].split(['-', '+']).next().is_some_and(|core| core.matches('.').count() == 2)
}

pub fn is_prerelease(v: &str) -> bool {
    parse(v).is_some_and(|p| !p.pre.is_empty())
}

fn cmp_num(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn cmp_pre(a: &str, b: &str) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    let (mut x, mut y) = (a[1..].split('.'), b[1..].split('.'));
    loop {
        match (x.next(), y.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(p), Some(q)) => {
                let (pn, qn) = (p.bytes().all(|c| c.is_ascii_digit()), q.bytes().all(|c| c.is_ascii_digit()));
                let o = match (pn, qn) {
                    (true, true) => cmp_num(p, q),
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (false, false) => p.cmp(q),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
        }
    }
}

/// Go's `semver.Compare`: invalid versions are equal to each other and below all valid ones.
pub fn compare(a: &str, b: &str) -> Ordering {
    match (parse(a), parse(b)) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(p), Some(q)) => cmp_num(p.major, q.major)
            .then_with(|| cmp_num(p.minor, q.minor))
            .then_with(|| cmp_num(p.patch, q.patch))
            .then_with(|| cmp_pre(p.pre, q.pre)),
    }
}

/// `1.17`, `1.21.0`, `1.21rc1` from a `go` directive -> (1, 17).
pub fn go_release(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor: String = parts.next().unwrap_or("0").chars().take_while(char::is_ascii_digit).collect();
    Some((major, minor.parse().ok()?))
}
