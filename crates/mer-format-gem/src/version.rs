//! RubyGems versions and requirements, as `Gem::Version` and
//! `Gem::Requirement` define them. Checked against Ruby with the truth table
//! in `tests/gem_truth.json`.
use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Seg {
    Num(u64),
    Str(String),
}

/// A gem version: `1.19.7`, `3.2.0.rc1`, `1.0.0.pre.1`.
#[derive(Clone, Debug)]
pub struct Version {
    text: String,
    segments: Vec<Seg>,
}

impl Version {
    /// Parse as `Gem::Version.new` does. `1.0-1` is read as `1.0.pre.1`.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let text = if s.is_empty() { "0".to_string() } else { s.replace('-', ".pre.") };
        if !correct(s) {
            return None;
        }
        let mut segments = Vec::new();
        let b = text.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i].is_ascii_digit() {
                let start = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                segments.push(Seg::Num(text[start..i].parse().ok()?));
            } else if b[i].is_ascii_alphabetic() {
                let start = i;
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                segments.push(Seg::Str(text[start..i].to_string()));
            } else {
                i += 1;
            }
        }
        Some(Version { text, segments })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn is_prerelease(&self) -> bool {
        self.text.bytes().any(|c| c.is_ascii_alphabetic())
    }

    /// Release and prerelease parts, each without trailing zeros, joined.
    fn canonical(&self) -> Vec<Seg> {
        let split = self.segments.iter().position(|s| matches!(s, Seg::Str(_))).unwrap_or(self.segments.len());
        let trim = |part: &[Seg]| {
            let mut v = part.to_vec();
            while v.last() == Some(&Seg::Num(0)) {
                v.pop();
            }
            v
        };
        let mut out = trim(&self.segments[..split]);
        out.extend(trim(&self.segments[split..]));
        out
    }

    /// The version without its prerelease part: `1.2.0.rc1` → `1.2.0`.
    fn release(&self) -> Version {
        if !self.is_prerelease() {
            return self.clone();
        }
        let nums: Vec<String> = self.segments.iter().take_while(|s| matches!(s, Seg::Num(_))).map(|s| match s {
            Seg::Num(n) => n.to_string(),
            Seg::Str(_) => unreachable!(),
        }).collect();
        Version::parse(&nums.join(".")).unwrap_or_else(|| Version::parse("0").expect("0"))
    }

    /// The upper bound of `~>`: drop prerelease parts and the last number,
    /// then increment the one before: `2.2.0` → `2.3`, `2.2` → `3`.
    fn bump(&self) -> Version {
        let mut nums: Vec<u64> = self.segments.iter().take_while(|s| matches!(s, Seg::Num(_))).map(|s| match s {
            Seg::Num(n) => *n,
            Seg::Str(_) => unreachable!(),
        }).collect();
        if nums.len() > 1 {
            nums.pop();
        }
        if let Some(last) = nums.last_mut() {
            *last += 1;
        }
        let s: Vec<String> = nums.iter().map(u64::to_string).collect();
        Version::parse(&s.join(".")).unwrap_or_else(|| Version::parse("0").expect("0"))
    }
}

/// What `Gem::Version.correct?` accepts.
fn correct(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    let (main, pre) = match s.split_once('-') {
        Some((m, p)) => (m, Some(p)),
        None => (s, None),
    };
    let mut parts = main.split('.');
    let first = parts.next().unwrap_or("");
    if first.is_empty() || !first.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if !parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric())) {
        return false;
    }
    match pre {
        None => true,
        Some(p) => p.split('.').all(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')),
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Version {}
impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.text == other.text {
            return Ordering::Equal;
        }
        let (l, r) = (self.canonical(), other.canonical());
        for i in 0..l.len().max(r.len()) {
            let a = l.get(i).cloned().unwrap_or(Seg::Num(0));
            let b = r.get(i).cloned().unwrap_or(Seg::Num(0));
            let o = match (&a, &b) {
                (Seg::Num(x), Seg::Num(y)) => x.cmp(y),
                (Seg::Str(x), Seg::Str(y)) => x.cmp(y),
                (Seg::Str(_), Seg::Num(_)) => Ordering::Less,
                (Seg::Num(_), Seg::Str(_)) => Ordering::Greater,
            };
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Eq,
    Ne,
    Gt,
    Lt,
    Ge,
    Le,
    Pessimistic,
}

/// A requirement: one or more `op version` constraints that must all hold.
#[derive(Clone, Debug)]
pub struct Requirement {
    parts: Vec<(Op, Version)>,
}

impl Requirement {
    /// Parse `~> 1.4`, `>= 3.2&< 4.1.dev` (the compact index joins with `&`),
    /// or `>= 1.0, < 2`. An empty requirement is `>= 0`.
    pub fn parse(s: &str) -> Option<Requirement> {
        let mut parts = Vec::new();
        for p in s.split(['&', ',']).map(str::trim).filter(|p| !p.is_empty()) {
            let (op, rest) = [("~>", Op::Pessimistic), (">=", Op::Ge), ("<=", Op::Le), ("!=", Op::Ne), ("=", Op::Eq), (">", Op::Gt), ("<", Op::Lt)]
                .iter()
                .find_map(|(t, op)| p.strip_prefix(t).map(|r| (*op, r)))
                .unwrap_or((Op::Eq, p));
            parts.push((op, Version::parse(rest.trim())?));
        }
        if parts.is_empty() {
            parts.push((Op::Ge, Version::parse("0")?));
        }
        Some(Requirement { parts })
    }

    pub fn any() -> Requirement {
        Requirement::parse(">= 0").expect("valid")
    }

    /// `Gem::Requirement#satisfied_by?`.
    pub fn matches(&self, v: &Version) -> bool {
        self.parts.iter().all(|(op, r)| match op {
            Op::Eq => v == r,
            Op::Ne => v != r,
            Op::Gt => v > r,
            Op::Lt => v < r,
            Op::Ge => v >= r,
            Op::Le => v <= r,
            Op::Pessimistic => v >= r && v.release() < r.bump(),
        })
    }

    /// Whether the requirement names a prerelease, which lets prereleases match.
    pub fn allows_prerelease(&self) -> bool {
        self.parts.iter().any(|(_, v)| v.is_prerelease())
    }
}
