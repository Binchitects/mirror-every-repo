//! Composer versions and constraints, as `composer/semver` (the library
//! Composer itself uses) defines them: `VersionParser::normalize`,
//! `parseConstraints`, `parseStability`, and PHP's `version_compare` on the
//! normalized forms. Checked against composer/semver with the truth table
//! in `tests/composer_truth.json`.
use std::cmp::Ordering;

// ------------------------------------------------------- version_compare

/// PHP's `php_canonicalize_version`: `-`, `_`, `+` become `.`, and a `.` goes
/// between digits and letters.
fn canonicalize(v: &str) -> String {
    let b = v.as_bytes();
    let Some(&first) = b.first() else { return String::new() };
    let mut out = vec![first];
    let mut lp = first;
    let isdig = |c: u8| c.is_ascii_digit();
    let isndig = |c: u8| !c.is_ascii_digit() && c != b'.';
    for &c in &b[1..] {
        let last = *out.last().expect("not empty");
        if matches!(c, b'-' | b'_' | b'+') {
            if last != b'.' {
                out.push(b'.');
            }
        } else if (isndig(lp) && isdig(c)) || (isdig(lp) && isndig(c)) {
            if last != b'.' {
                out.push(b'.');
            }
            out.push(c);
        } else if !c.is_ascii_alphanumeric() {
            if last != b'.' {
                out.push(b'.');
            }
        } else {
            out.push(c);
        }
        lp = c;
    }
    String::from_utf8(out).unwrap_or_default()
}

fn special_order(form: &str) -> i32 {
    const FORMS: &[(&str, i32)] = &[("dev", 0), ("alpha", 1), ("a", 1), ("beta", 2), ("b", 2), ("RC", 3), ("rc", 3), ("#", 4), ("pl", 5), ("p", 5)];
    FORMS.iter().find(|(name, _)| form.starts_with(name)).map_or(-6, |(_, o)| *o)
}

fn compare_special(a: &str, b: &str) -> Ordering {
    special_order(a).cmp(&special_order(b))
}

/// PHP's `version_compare($a, $b)`.
pub fn php_version_compare(a: &str, b: &str) -> Ordering {
    let (ca, cb) = (canonicalize(a), canonicalize(b));
    if ca.is_empty() || cb.is_empty() {
        return match (ca.is_empty(), cb.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Less,
            _ => Ordering::Greater,
        };
    }
    let pa: Vec<&str> = ca.split('.').collect();
    let pb: Vec<&str> = cb.split('.').collect();
    let num = |s: &str| s.bytes().take_while(u8::is_ascii_digit).fold(0i64, |acc, d| acc.saturating_mul(10).saturating_add(i64::from(d - b'0')));
    let starts_digit = |s: &str| s.as_bytes().first().is_some_and(u8::is_ascii_digit);
    let mut i = 0;
    while i < pa.len() && i < pb.len() {
        let (x, y) = (pa[i], pb[i]);
        let c = match (starts_digit(x), starts_digit(y)) {
            (true, true) => num(x).cmp(&num(y)),
            (false, false) => compare_special(x, y),
            (true, false) => compare_special("#N#", y),
            (false, true) => compare_special(x, "#N#"),
        };
        if c != Ordering::Equal {
            return c;
        }
        i += 1;
    }
    if i < pa.len() {
        return if starts_digit(pa[i]) { Ordering::Greater } else { php_version_compare(&pa[i..].join("."), "#N#") };
    }
    if i < pb.len() {
        return if starts_digit(pb[i]) { Ordering::Less } else { php_version_compare("#N#", &pb[i..].join(".")) };
    }
    Ordering::Equal
}

// --------------------------------------------------------------- normalize

/// What `[._-]?(?:(stable|beta|b|RC|alpha|a|patch|pl|p)((?:[.-]?\d+)*+)?)?([.-]?dev)?`
/// captured: (stability word, its number part, whether `dev` follows).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Modifier {
    word: Option<String>,
    number: String,
    dev: bool,
}

const WORDS: &[&str] = &["stable", "beta", "b", "rc", "alpha", "a", "patch", "pl", "p"];

/// Match the modifier against all of `s` (case-insensitively), trying the
/// alternatives in the order the regex does.
fn modifier(s: &str) -> Option<Modifier> {
    let lower = s.to_ascii_lowercase();
    let starts: Vec<usize> = if lower.starts_with(['.', '_', '-']) { vec![1, 0] } else { vec![0] };
    for start in starts {
        let rest = &lower[start..];
        // With a stability word first, as the regex prefers.
        for w in WORDS {
            if let Some(after) = rest.strip_prefix(w) {
                // Possessive: take every [.-]?digits group there is.
                let b = after.as_bytes();
                let mut i = 0;
                loop {
                    let mut j = i;
                    if j < b.len() && matches!(b[j], b'.' | b'-') {
                        j += 1;
                    }
                    let d = b[j..].iter().take_while(|c| c.is_ascii_digit()).count();
                    if d == 0 {
                        break;
                    }
                    i = j + d;
                }
                let number = after[..i].to_string();
                if let Some(dev) = dev_tail(&after[i..]) {
                    let word = s[start..start + w.len()].to_string();
                    return Some(Modifier { word: Some(word), number, dev });
                }
            }
        }
        if let Some(dev) = dev_tail(rest) {
            return Some(Modifier { word: None, number: String::new(), dev });
        }
    }
    None
}

/// `([.-]?dev)?$`
fn dev_tail(s: &str) -> Option<bool> {
    match s {
        "" => Some(false),
        "dev" | ".dev" | "-dev" => Some(true),
        _ => None,
    }
}

fn expand_stability(s: &str) -> String {
    match s.to_ascii_lowercase().as_str() {
        "a" => "alpha".into(),
        "b" => "beta".into(),
        "p" | "pl" => "patch".into(),
        "rc" => "RC".into(),
        other => other.to_string(),
    }
}

/// The plain-version form: `v?(\d{1,5})(\.\d+)?(\.\d+)?(\.\d+)?` then a modifier.
fn numeric_form(s: &str) -> Option<(Vec<String>, Modifier)> {
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let b = s.as_bytes();
    let first = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if first == 0 {
        return None;
    }
    // \d{1,5}+ is possessive: at most five digits, and never fewer.
    let first = first.min(5);
    let mut ends = vec![first];
    let mut i = first;
    for _ in 0..3 {
        if i < b.len() && b[i] == b'.' {
            let d = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
            if d > 0 {
                i += 1 + d;
                ends.push(i);
                continue;
            }
        }
        break;
    }
    // The optional groups are greedy: the most groups first.
    for n in (1..=ends.len()).rev() {
        let end = ends[n - 1];
        if let Some(m) = modifier(&s[end..]) {
            let mut parts = vec![s[..first].to_string()];
            let mut at = first;
            for &e in &ends[1..n] {
                parts.push(s[at + 1..e].to_string());
                at = e;
            }
            return Some((parts, m));
        }
    }
    None
}

/// The date form: `v?(\d{4}(?:[.:-]?\d{2}){1,6}(?:[.:-]?\d{1,3}){0,2})` then a modifier.
fn date_form(s: &str) -> Option<(String, Modifier)> {
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let b = s.as_bytes();
    if b.len() < 4 || !b[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    // Groups of (separator?, digits) after the year; greedy, with backtracking.
    fn go(s: &str, b: &[u8], at: usize, twos: usize, rest: usize) -> Option<(usize, Modifier)> {
        let sep = |i: usize| if i < b.len() && matches!(b[i], b'.' | b':' | b'-') { vec![i + 1, i] } else { vec![i] };
        if twos < 6 {
            for i in sep(at) {
                if i + 2 <= b.len() && b[i..i + 2].iter().all(u8::is_ascii_digit) && (i + 2 > at) {
                    if let Some(r) = go(s, b, i + 2, twos + 1, rest) {
                        return Some(r);
                    }
                }
            }
        }
        if twos >= 1 && rest == 0 {
            // Now the up-to-three-digit groups.
            fn tail(s: &str, b: &[u8], at: usize, n: usize) -> Option<(usize, Modifier)> {
                if n < 2 {
                    for i in if at < b.len() && matches!(b[at], b'.' | b':' | b'-') { vec![at + 1, at] } else { vec![at] } {
                        let d = b[i.min(b.len())..].iter().take_while(|c| c.is_ascii_digit()).count();
                        for k in (1..=d.min(3)).rev() {
                            if let Some(r) = tail(s, b, i + k, n + 1) {
                                return Some(r);
                            }
                        }
                    }
                }
                modifier(&s[at..]).map(|m| (at, m))
            }
            return tail(s, b, at, 0);
        }
        None
    }
    let (end, m) = go(s, b, 4, 0, 0)?;
    Some((s[..end].chars().map(|c| if c.is_ascii_digit() { c } else { '.' }).collect(), m))
}

/// `VersionParser::normalizeBranch`.
fn normalize_branch(name: &str) -> String {
    let n = name.trim();
    let s = n.strip_prefix(['v', 'V']).unwrap_or(n);
    let parts: Vec<&str> = s.split('.').collect();
    let num_ok = |p: &str| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit());
    let part_ok = |p: &str| num_ok(p) || matches!(p, "x" | "X" | "*");
    if !parts.is_empty() && parts.len() <= 4 && num_ok(parts[0]) && parts[1..].iter().all(|p| part_ok(p)) {
        let mut v = parts[0].to_string();
        for i in 1..4 {
            v.push('.');
            v.push_str(parts.get(i).map_or("x", |p| if matches!(*p, "*" | "X") { "x" } else { p }));
        }
        return v.replace('x', "9999999") + "-dev";
    }
    format!("dev-{n}")
}

/// `VersionParser::normalize`: `v1.2` → `1.2.0.0`, `1.0.0-b2` → `1.0.0.0-beta2`.
pub fn normalize(version: &str) -> Option<String> {
    let mut v = version.trim().to_string();
    if let Some((a, _)) = v.split_once(" as ") {
        v = a.trim().to_string();
    }
    let lower = v.to_ascii_lowercase();
    for s in ["stable", "rc", "beta", "alpha", "dev"] {
        if lower.ends_with(&format!("@{s}")) {
            v.truncate(v.len() - s.len() - 1);
            break;
        }
    }
    if matches!(v.as_str(), "master" | "trunk" | "default") {
        v = format!("dev-{v}");
    }
    if v.len() >= 4 && v[..4].eq_ignore_ascii_case("dev-") {
        return Some(format!("dev-{}", &v[4..]));
    }
    if let Some((a, b)) = v.split_once('+') {
        if !a.is_empty() && !a.contains([',', ' ']) && !b.is_empty() && !b.contains(char::is_whitespace) {
            v = a.to_string();
        }
    }
    let finish = |base: String, m: Modifier| -> String {
        let mut out = base;
        if let Some(w) = &m.word {
            if w == "stable" {
                return out;
            }
            out.push('-');
            out.push_str(&expand_stability(w));
            out.push_str(m.number.trim_start_matches(['.', '-']));
        }
        if m.dev {
            out.push_str("-dev");
        }
        out
    };
    if let Some((parts, m)) = numeric_form(&v) {
        let mut base = parts[0].clone();
        for i in 1..4 {
            base.push('.');
            base.push_str(parts.get(i).map_or("0", String::as_str));
        }
        return Some(finish(base, m));
    }
    if let Some((base, m)) = date_form(&v) {
        return Some(finish(base, m));
    }
    let lower = v.to_ascii_lowercase();
    // `(.*?)[.-]?dev$`: a branch alias such as 2.x-dev.
    if let Some(stem) = lower.strip_suffix("dev") {
        let cut = if stem.ends_with(['.', '-']) { stem.len() - 1 } else { stem.len() };
        let n = normalize_branch(&v[..cut]);
        if !n.contains("dev-") {
            return Some(n);
        }
    }
    None
}

/// `VersionParser::parseStability`: `stable`, `RC`, `beta`, `alpha` or `dev`.
pub fn stability(version: &str) -> &'static str {
    let v = version.split('#').next().unwrap_or("");
    if v.starts_with("dev-") || v.ends_with("-dev") {
        return "dev";
    }
    let lower = v.to_ascii_lowercase();
    // The leftmost place from which the rest is a modifier (and a +build).
    for i in 0..=lower.len() {
        if !lower.is_char_boundary(i) {
            continue;
        }
        let rest = &lower[i..];
        let core = rest.split_once('+').map_or(rest, |(a, _)| a);
        for candidate in [rest, core] {
            if let Some(m) = modifier(candidate) {
                if m.dev {
                    return "dev";
                }
                return match m.word.as_deref() {
                    Some("beta" | "b") => "beta",
                    Some("alpha" | "a") => "alpha",
                    Some("rc") => "RC",
                    _ => "stable",
                };
            }
        }
    }
    "stable"
}

// ------------------------------------------------------------- constraints

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A parsed constraint: any of the groups, each of which needs all its parts.
#[derive(Clone, Debug)]
pub struct Constraint {
    any: Vec<Vec<(Op, String)>>,
}

/// The groups of `v?(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:\.(\d+))?(?:MOD|\.([xX*][.-]?dev))(?:\+\S+)?`.
struct VersionMatch {
    nums: [Option<String>; 4],
    word: Option<String>,
    dev: bool,
    xdev: bool,
}

fn version_match(s: &str) -> Option<VersionMatch> {
    let s = match s.split_once('+') {
        Some((a, b)) if !b.is_empty() && !b.contains(char::is_whitespace) => a,
        _ => s,
    };
    let body = s.strip_prefix(['v', 'V']).unwrap_or(s);
    let b = body.as_bytes();
    let mut ends = Vec::new();
    let mut i = 0;
    let d = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if d == 0 {
        return None;
    }
    i += d;
    ends.push(i);
    for _ in 0..3 {
        if i < b.len() && b[i] == b'.' {
            let d = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
            if d > 0 {
                i += 1 + d;
                ends.push(i);
                continue;
            }
        }
        break;
    }
    for n in (1..=ends.len()).rev() {
        let end = ends[n - 1];
        let mut nums: [Option<String>; 4] = Default::default();
        let mut at = 0;
        for (k, &e) in ends[..n].iter().enumerate() {
            nums[k] = Some(body[if k == 0 { 0 } else { at + 1 }..e].to_string());
            at = e;
        }
        let rest = &body[end..];
        if let Some(m) = modifier(rest) {
            return Some(VersionMatch { nums, word: m.word, dev: m.dev, xdev: false });
        }
        let r = rest.to_ascii_lowercase();
        if let Some(x) = r.strip_prefix('.') {
            if let Some(after) = x.strip_prefix(['x', '*']) {
                if matches!(after, "dev" | ".dev" | "-dev") {
                    return Some(VersionMatch { nums, word: None, dev: false, xdev: true });
                }
            }
        }
    }
    None
}

/// `manipulateVersionString`: zero what follows `position`, add `increment` there.
fn manipulate(nums: &[Option<String>; 4], position: usize, increment: i64) -> Option<String> {
    let mut parts: Vec<String> = nums.iter().map(|n| n.clone().unwrap_or_default()).collect();
    for i in (1..=4).rev() {
        if i > position {
            parts[i - 1] = "0".into();
        } else if i == position && increment != 0 {
            let n: i64 = parts[i - 1].parse().unwrap_or(0) + increment;
            if n < 0 {
                parts[i - 1] = "0".into();
                if i == 1 {
                    return None;
                }
            } else {
                parts[i - 1] = n.to_string();
            }
        }
    }
    Some(parts.join("."))
}

fn is_operator(t: &str) -> bool {
    matches!(t, "<>" | "!=" | ">" | ">=" | "<" | "<=" | "=" | "==")
}

/// Split one OR-alternative into its AND parts, as composer's lookbehind
/// split does: on commas and spaces, but not after an operator, nor around
/// the `-` of a range, nor around `as`.
fn and_parts(s: &str) -> Vec<String> {
    let tokens: Vec<&str> = s.split([' ', ',']).filter(|t| !t.is_empty()).collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let mut cur = tokens[i].to_string();
        while is_operator(&cur) && i + 1 < tokens.len() {
            i += 1;
            cur.push_str(tokens[i]);
        }
        while i + 2 < tokens.len() && (tokens[i + 1] == "-" || tokens[i + 1] == "as") {
            cur = format!("{cur} {} {}", tokens[i + 1], tokens[i + 2]);
            i += 2;
        }
        out.push(cur);
        i += 1;
    }
    out
}

fn parse_one(c: &str) -> Option<Vec<(Op, String)>> {
    let mut c = c.trim().to_string();
    if let Some((a, _)) = c.split_once(" as ") {
        c = a.trim().to_string();
    }
    let mut modifier_flag: Option<String> = None;
    if let Some(at) = c.rfind('@') {
        let flag = c[at + 1..].to_ascii_lowercase();
        if ["stable", "rc", "beta", "alpha", "dev"].contains(&flag.as_str()) && !c[..at].contains([',', ' ']) {
            if flag != "stable" {
                modifier_flag = Some(c[at + 1..].to_string());
            }
            c = if at == 0 { "*".into() } else { c[..at].to_string() };
        }
    }
    if let Some(hash) = c.find('#') {
        let head = &c[..hash];
        if head.to_ascii_lowercase().starts_with("dev-") || head.to_ascii_lowercase().ends_with(".x-dev") {
            c = head.to_string();
        }
    }
    // * and x
    {
        let s = c.strip_prefix(['v', 'V']).unwrap_or(&c);
        let parts: Vec<&str> = s.split('.').collect();
        if !s.is_empty() && parts.iter().all(|p| matches!(*p, "x" | "X" | "*")) {
            if c.starts_with(['v', 'V']) || parts.len() > 1 {
                return Some(vec![(Op::Ge, "0.0.0.0-dev".into())]);
            }
            return Some(vec![]);
        }
    }
    let low_from = |c: &str, m: &VersionMatch| -> Option<String> {
        let suffix = if m.word.is_none() && !m.dev && !m.xdev { "-dev" } else { "" };
        normalize(&format!("{}{suffix}", &c[1..]))
    };
    // ~
    if let Some(rest) = c.strip_prefix('~') {
        if rest.starts_with('>') {
            return None;
        }
        let m = version_match(rest)?;
        let mut position = (1..=4).rev().find(|&i| m.nums[i - 1].as_deref().is_some_and(|n| !n.is_empty())).unwrap_or(1);
        if m.xdev {
            position += 1;
        }
        let low = low_from(&c, &m)?;
        let high = manipulate(&m.nums, position.saturating_sub(1).max(1), 1)? + "-dev";
        return Some(vec![(Op::Ge, low), (Op::Lt, high)]);
    }
    // ^
    if let Some(rest) = c.strip_prefix('^') {
        let m = version_match(rest)?;
        let n = |i: usize| m.nums[i].clone().unwrap_or_default();
        let position = if n(0) != "0" || n(1).is_empty() {
            1
        } else if n(1) != "0" || n(2).is_empty() {
            2
        } else {
            3
        };
        let low = low_from(&c, &m)?;
        let high = manipulate(&m.nums, position, 1)? + "-dev";
        return Some(vec![(Op::Ge, low), (Op::Lt, high)]);
    }
    // 1.2.* and the like
    {
        let s = c.strip_prefix(['v', 'V']).unwrap_or(&c);
        let parts: Vec<&str> = s.split('.').collect();
        let numeric = parts.iter().take_while(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())).count();
        if (1..=3).contains(&numeric) && parts.len() > numeric && parts[numeric..].iter().all(|p| matches!(*p, "x" | "X" | "*")) {
            let mut nums: [Option<String>; 4] = Default::default();
            for (k, p) in parts[..numeric].iter().enumerate() {
                nums[k] = Some(p.to_string());
            }
            let low = manipulate(&nums, numeric, 0)? + "-dev";
            let high = manipulate(&nums, numeric, 1)? + "-dev";
            if low == "0.0.0.0-dev" {
                return Some(vec![(Op::Lt, high)]);
            }
            return Some(vec![(Op::Ge, low), (Op::Lt, high)]);
        }
    }
    // from - to
    if let Some((from, to)) = c.split_once(" - ") {
        let (from, to) = (from.trim(), to.trim());
        if let (Some(f), Some(t)) = (version_match(from), version_match(to)) {
            let low_suffix = if f.word.is_none() && !f.dev && !f.xdev { "-dev" } else { "" };
            let low = normalize(from)? + low_suffix;
            let has = |i: usize| t.nums[i].as_deref().is_some_and(|n| !n.is_empty());
            if (has(1) && has(2)) || t.word.is_some() || t.dev || t.xdev {
                return Some(vec![(Op::Ge, low), (Op::Le, normalize(to)?)]);
            }
            normalize(to)?;
            let high = manipulate(&t.nums, if has(1) { 2 } else { 1 }, 1)? + "-dev";
            return Some(vec![(Op::Ge, low), (Op::Lt, high)]);
        }
    }
    // An operator and a version.
    let (op_text, rest) = ["<>", "!=", ">=", "<=", "==", ">", "<", "="].iter().find_map(|o| c.strip_prefix(o).map(|r| (*o, r))).unwrap_or(("", c.as_str()));
    let rest = rest.trim_start();
    let mut version = match normalize(rest) {
        Some(v) => v,
        None if rest.ends_with("-dev") && rest.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'/')) => normalize(&format!("dev-{}", &rest[..rest.len() - 4]))?,
        None => return None,
    };
    let op = match op_text {
        "<>" | "!=" => Op::Ne,
        ">=" => Op::Ge,
        "<=" => Op::Le,
        ">" => Op::Gt,
        "<" => Op::Lt,
        _ => Op::Eq,
    };
    if op != Op::Eq && modifier_flag.is_some() && stability(&version) == "stable" {
        version = format!("{version}-{}", modifier_flag.unwrap_or_default());
    } else if matches!(op, Op::Lt | Op::Ge) && !ends_in_dash_modifier(rest) && !rest.starts_with("dev-") {
        version.push_str("-dev");
    }
    Some(vec![(op, version)])
}

/// Whether `s` matches `-MOD$`: a hyphen somewhere followed by a modifier to the end.
fn ends_in_dash_modifier(s: &str) -> bool {
    s.char_indices().filter(|(_, c)| *c == '-').any(|(i, _)| modifier(&s[i + 1..]).is_some())
}

impl Constraint {
    /// `VersionParser::parseConstraints`.
    pub fn parse(s: &str) -> Option<Constraint> {
        let mut any = Vec::new();
        for alt in s.trim().split("||").flat_map(|a| a.split('|')).map(str::trim) {
            let mut all = Vec::new();
            for part in and_parts(alt) {
                all.extend(parse_one(&part)?);
            }
            any.push(all);
        }
        Some(Constraint { any })
    }

    /// `Semver::satisfies`: whether a (not yet normalized) version fits.
    pub fn matches(&self, version: &str) -> bool {
        let Some(v) = normalize(version) else { return false };
        self.matches_normalized(&v)
    }

    pub fn matches_normalized(&self, v: &str) -> bool {
        self.any.iter().any(|all| all.iter().all(|(op, x)| compare(v, x, *op)))
    }
}

/// `Constraint::versionCompare` without branch comparison.
fn compare(a: &str, b: &str, op: Op) -> bool {
    let (ab, bb) = (a.starts_with("dev-"), b.starts_with("dev-"));
    if op == Op::Ne && (ab || bb) {
        return a != b;
    }
    if ab && bb {
        return op == Op::Eq && a == b;
    }
    if ab || bb {
        return false;
    }
    let o = php_version_compare(a, b);
    match op {
        Op::Eq => o == Ordering::Equal,
        Op::Ne => o != Ordering::Equal,
        Op::Lt => o == Ordering::Less,
        Op::Le => o != Ordering::Greater,
        Op::Gt => o == Ordering::Greater,
        Op::Ge => o != Ordering::Less,
    }
}
