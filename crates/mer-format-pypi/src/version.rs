//! Python version numbers and version specifiers (PEP 440).
use std::cmp::Ordering;

/// `[N!]N(.N)*[{a|b|rc}N][.postN][.devN][+local]`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub epoch: u64,
    pub release: Vec<u64>,
    /// (0 = alpha, 1 = beta, 2 = release candidate, number)
    pub pre: Option<(u8, u64)>,
    pub post: Option<u64>,
    pub dev: Option<u64>,
    pub local: Option<String>,
}

struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    fn number(&mut self) -> Option<u64> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        if start == self.i {
            return None;
        }
        std::str::from_utf8(&self.s[start..self.i]).ok()?.parse().ok()
    }
    fn skip_sep(&mut self) -> bool {
        if self.peek().is_some_and(|c| matches!(c, b'.' | b'-' | b'_')) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    /// Consume the longest of `words` found at the cursor.
    fn word(&mut self, words: &[&str]) -> Option<usize> {
        let rest = &self.s[self.i..];
        let mut best: Option<(usize, usize)> = None;
        for (idx, w) in words.iter().enumerate() {
            if rest.starts_with(w.as_bytes()) && best.is_none_or(|(_, len)| w.len() > len) {
                best = Some((idx, w.len()));
            }
        }
        let (idx, len) = best?;
        self.i += len;
        Some(idx)
    }
}

impl Version {
    pub fn parse(text: &str) -> Option<Version> {
        let lower = text.trim().to_ascii_lowercase();
        let (public, local) = match lower.split_once('+') {
            Some((p, l)) => {
                if l.is_empty() || !l.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')) {
                    return None;
                }
                (p, Some(l.replace(['-', '_'], ".")))
            }
            None => (lower.as_str(), None),
        };
        let mut c = Cursor { s: public.strip_prefix('v').unwrap_or(public).as_bytes(), i: 0 };
        let first = c.number()?;
        let (epoch, mut release) = if c.peek() == Some(b'!') {
            c.i += 1;
            (first, vec![c.number()?])
        } else {
            (0, vec![first])
        };
        loop {
            let save = c.i;
            if c.peek() == Some(b'.') {
                c.i += 1;
                if let Some(n) = c.number() {
                    release.push(n);
                    continue;
                }
            }
            c.i = save;
            break;
        }
        let mut v = Version { epoch, release, pre: None, post: None, dev: None, local };

        // Pre-release: a, b, c, rc, alpha, beta, pre, preview.
        let save = c.i;
        c.skip_sep();
        match c.word(&["alpha", "a", "beta", "b", "preview", "pre", "rc", "c"]) {
            Some(idx) => {
                let kind = [0u8, 0, 1, 1, 2, 2, 2, 2][idx];
                c.skip_sep();
                v.pre = Some((kind, c.number().unwrap_or(0)));
            }
            None => c.i = save,
        }
        // Post-release: `-N`, or post / rev / r.
        let save = c.i;
        if c.peek() == Some(b'-') && c.s.get(c.i + 1).is_some_and(|d| d.is_ascii_digit()) {
            c.i += 1;
            v.post = c.number();
        } else {
            c.skip_sep();
            match c.word(&["post", "rev", "r"]) {
                Some(_) => {
                    c.skip_sep();
                    v.post = Some(c.number().unwrap_or(0));
                }
                None => c.i = save,
            }
        }
        // Development release.
        let save = c.i;
        c.skip_sep();
        match c.word(&["dev"]) {
            Some(_) => {
                c.skip_sep();
                v.dev = Some(c.number().unwrap_or(0));
            }
            None => c.i = save,
        }
        (c.i == c.s.len()).then_some(v)
    }

    pub fn is_prerelease(&self) -> bool {
        self.pre.is_some() || self.dev.is_some()
    }

    fn trimmed(&self) -> &[u64] {
        let mut n = self.release.len();
        while n > 1 && self.release[n - 1] == 0 {
            n -= 1;
        }
        &self.release[..n]
    }

    /// Compare the public parts (everything except the local label).
    fn cmp_public(&self, other: &Version) -> Ordering {
        // A dev release of the bare version sorts before any pre-release;
        // a version without a pre-release sorts after all its pre-releases.
        let pre_key = |v: &Version| match (v.pre, v.post, v.dev) {
            (None, None, Some(_)) => (0u8, 0u8, 0u64),
            (Some((k, n)), _, _) => (1, k, n),
            (None, _, _) => (2, 0, 0),
        };
        let post_key = |v: &Version| v.post.map_or((0u8, 0u64), |n| (1, n));
        let dev_key = |v: &Version| v.dev.map_or((1u8, 0u64), |n| (0, n));
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| self.trimmed().cmp(other.trimmed()))
            .then_with(|| pre_key(self).cmp(&pre_key(other)))
            .then_with(|| post_key(self).cmp(&post_key(other)))
            .then_with(|| dev_key(self).cmp(&dev_key(other)))
    }
}

/// Local labels: segment by segment; numbers beat strings, longer beats shorter.
fn cmp_local(a: &Option<String>, b: &Option<String>) -> Ordering {
    let (Some(a), Some(b)) = (a, b) else { return a.is_some().cmp(&b.is_some()) };
    let key = |s: &str| -> Vec<(u8, u64, String)> {
        s.split('.').map(|p| p.parse::<u64>().map_or((0, 0, p.to_string()), |n| (1, n, String::new()))).collect()
    };
    key(a).cmp(&key(b))
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cmp_public(other).then_with(|| cmp_local(&self.local, &other.local))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// One clause such as `>=1.2` or `==1.4.*`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Specifier {
    op: String,
    text: String,
}

/// Comma-separated clauses; a version must satisfy all of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpecifierSet(pub Vec<Specifier>);

impl Specifier {
    fn parse(clause: &str) -> Option<Specifier> {
        let clause = clause.trim();
        let op = ["===", "~=", "==", "!=", "<=", ">=", "<", ">"].into_iter().find(|op| clause.starts_with(op))?;
        let text = clause[op.len()..].trim().to_string();
        if text.is_empty() {
            return None;
        }
        let spec = Specifier { op: op.to_string(), text };
        // Reject what cannot be evaluated, at parse time.
        match op {
            "===" => {}
            "==" | "!=" if spec.text.ends_with(".*") => {
                Version::parse(spec.text.trim_end_matches(".*"))?;
            }
            "~=" => {
                if Version::parse(&spec.text)?.release.len() < 2 {
                    return None;
                }
            }
            _ => {
                Version::parse(&spec.text)?;
            }
        }
        Some(spec)
    }

    /// Does `v` match the release prefix `self.text` minus its `.*`?
    fn prefix_match(&self, v: &Version) -> bool {
        let Some(p) = Version::parse(self.text.trim_end_matches(".*")) else { return false };
        // Compare the first N release numbers, padding the candidate with zeros.
        let mut rel = v.release.clone();
        rel.resize(rel.len().max(p.release.len()), 0);
        v.epoch == p.epoch && rel[..p.release.len()] == p.release[..]
    }

    pub fn matches(&self, v: &Version) -> bool {
        if self.op == "===" {
            return Version::parse(&self.text).is_some_and(|s| s == *v);
        }
        if (self.op == "==" || self.op == "!=") && self.text.ends_with(".*") {
            return self.prefix_match(v) == (self.op == "==");
        }
        let Some(s) = Version::parse(&self.text) else { return false };
        // A local label on the candidate is ignored unless the clause names one.
        let public = Version { local: None, ..v.clone() };
        let cand = if s.local.is_some() { v } else { &public };
        match self.op.as_str() {
            "==" => *cand == s || cand.cmp(&s) == Ordering::Equal,
            "!=" => cand.cmp(&s) != Ordering::Equal,
            "<=" => public.cmp_public(&s) != Ordering::Greater,
            ">=" => public.cmp_public(&s) != Ordering::Less,
            // "<1.0" must not let in 1.0's own pre-releases.
            "<" => {
                public.cmp_public(&s) == Ordering::Less
                    && (s.is_prerelease() || !(v.is_prerelease() && v.epoch == s.epoch && v.trimmed() == s.trimmed()))
            }
            // ">1.0" must not let in 1.0.post1, nor any local build of the
            // same base version (1.0+local), which only sorts higher on paper.
            ">" => {
                let same_base = v.epoch == s.epoch && v.trimmed() == s.trimmed();
                v.cmp(&s) == Ordering::Greater
                    && (s.post.is_some() || !(v.post.is_some() && same_base))
                    && !(v.local.is_some() && same_base)
            }
            // "~=2.2" means ">=2.2, ==2.*".
            "~=" => {
                let mut prefix = s.release.clone();
                prefix.pop();
                let mut rel = v.release.clone();
                rel.resize(rel.len().max(prefix.len()), 0);
                public.cmp_public(&s) != Ordering::Less && v.epoch == s.epoch && rel[..prefix.len()] == prefix[..]
            }
            _ => false,
        }
    }
}

impl SpecifierSet {
    pub fn parse(text: &str) -> Option<SpecifierSet> {
        let text = text.trim();
        if text.is_empty() {
            return Some(SpecifierSet::default());
        }
        text.split(',').map(Specifier::parse).collect::<Option<Vec<_>>>().map(SpecifierSet)
    }

    /// True when every clause accepts `v`. Pre-releases are a separate policy
    /// decision made by the caller.
    pub fn matches(&self, v: &Version) -> bool {
        self.0.iter().all(|s| s.matches(v))
    }

    /// Does any clause itself name a pre-release? Then pre-releases are wanted.
    pub fn mentions_prerelease(&self) -> bool {
        self.0.iter().any(|s| Version::parse(s.text.trim_end_matches(".*")).is_some_and(|v| v.is_prerelease()))
    }

    pub fn and(&mut self, other: &SpecifierSet) {
        for s in &other.0 {
            if !self.0.contains(s) {
                self.0.push(s.clone());
            }
        }
    }
}

impl std::fmt::Display for SpecifierSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<String> = self.0.iter().map(|s| format!("{}{}", s.op, s.text)).collect();
        f.write_str(&parts.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("{s} should parse"))
    }

    #[test]
    fn the_ordering_example_from_pep_440() {
        // From the "Summary of permitted suffixes and relative ordering" section.
        let order = [
            "1.dev0", "1.0.dev456", "1.0a1", "1.0a2.dev456", "1.0a12.dev456", "1.0a12", "1.0b1.dev456", "1.0b2",
            "1.0b2.post345.dev456", "1.0b2.post345", "1.0rc1.dev456", "1.0rc1", "1.0", "1.0+abc.5", "1.0+abc.7",
            "1.0+5", "1.0.post456.dev34", "1.0.post456", "1.0.15", "1.1.dev1",
        ];
        for pair in order.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
    }

    #[test]
    fn normalisation_makes_spellings_equal() {
        for (a, b) in [
            ("1.0", "1"), ("1.0.0", "1"), ("v1.2", "1.2"), ("1.0alpha1", "1.0a1"), ("1.0-rc.1", "1.0rc1"),
            ("1.0c1", "1.0rc1"), ("1.0-1", "1.0.post1"), ("1.0.rev2", "1.0.post2"), ("1.0-DEV3", "1.0.dev3"),
            ("1.0a", "1.0a0"), ("0!1.0", "1.0"), ("1.0+ubuntu-1", "1.0+ubuntu.1"), ("1.0.post", "1.0.post0"),
        ] {
            assert_eq!(v(a).cmp(&v(b)), Ordering::Equal, "{a} == {b}");
        }
        assert!(v("1!0.5") > v("2.0"), "epoch wins");
        for bad in ["", "abc", "1.0.", "1..0", "1.0+", "1.0 beta", "1.0-", "1.0+a b", "!1"] {
            assert!(Version::parse(bad).is_none(), "{bad:?}");
        }
    }

    fn accepts(spec: &str, version: &str) -> bool {
        SpecifierSet::parse(spec).unwrap_or_else(|| panic!("{spec} should parse")).matches(&v(version))
    }

    #[test]
    fn specifier_rules() {
        let yes = [
            ("", "1.0"), (">=1.0", "1.0"), (">=1.0,<2", "1.9.9"), ("==1.4.*", "1.4.2"), ("==1.4.*", "1.4"),
            ("~=2.2", "2.9"), ("~=1.4.5", "1.4.9"), ("!=1.5", "1.4"), ("==1.0", "1.0.0"), ("==1.0", "1.0+local"),
            ("<2", "1.99"), (">1.7", "1.7.1"), (">1.7.post2", "1.7.post3"), ("===1.0", "1.0"), ("<=1.0", "1.0a1"),
            (">=1.0a1", "1.0a2"), ("==1.*", "1.0.post1"), ("!=1.4.*", "1.5"), ("<1.0rc1", "1.0b2"),
        ];
        let no = [
            (">=1.0,<2", "2.0"), ("==1.4.*", "1.5"), ("~=2.2", "3.0"), ("~=1.4.5", "1.5.0"), ("~=2.2", "2.1"),
            ("!=1.5", "1.5.0"), ("==1.0", "1.0.1"), ("<2", "2.0a1"), (">1.7", "1.7.post1"), (">1.7", "1.7+local"),
            ("<1.0", "1.0.dev1"), ("==1.0+a", "1.0"), ("==1.0+a", "1.0+b"), (">1.0", "1.0"), ("!=1.4.*", "1.4.9"),
        ];
        for (spec, ver) in yes {
            assert!(accepts(spec, ver), "{spec} should accept {ver}");
        }
        for (spec, ver) in no {
            assert!(!accepts(spec, ver), "{spec} should refuse {ver}");
        }
        for bad in ["1.0", "=1.0", ">=", "~=1", ">=abc", "==1.0,,<2", "<<1"] {
            assert!(SpecifierSet::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn sets_combine_and_print() {
        let mut a = SpecifierSet::parse(">=1.0").unwrap();
        a.and(&SpecifierSet::parse("<2, >=1.0").unwrap());
        assert_eq!(a.to_string(), ">=1.0,<2");
        assert!(a.matches(&v("1.5")) && !a.matches(&v("2.1")));
        assert!(SpecifierSet::parse(">=1.0rc1").unwrap().mentions_prerelease());
        assert!(!SpecifierSet::parse(">=1.0").unwrap().mentions_prerelease());
    }
}
