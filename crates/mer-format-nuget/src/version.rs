//! NuGet version numbers and version ranges.
use std::cmp::Ordering;

/// `major[.minor[.patch[.revision]]][-prerelease][+metadata]`
#[derive(Clone, Debug)]
pub struct Version {
    /// Always four numbers; missing parts are zero.
    pub parts: [u64; 4],
    /// Pre-release labels, in the spelling they were written.
    pub pre: Vec<String>,
}

impl Version {
    pub fn parse(text: &str) -> Option<Version> {
        let t = text.trim();
        if t.is_empty() || t.len() > 256 {
            return None;
        }
        let t = t.split('+').next()?; // build metadata never matters
        let (core, pre) = match t.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (t, None),
        };
        let pieces: Vec<&str> = core.split('.').collect();
        if pieces.is_empty() || pieces.len() > 4 {
            return None;
        }
        let mut parts = [0u64; 4];
        for (i, piece) in pieces.iter().enumerate() {
            if piece.is_empty() || !piece.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            // NuGet stores these as 32-bit integers.
            parts[i] = piece.parse::<u64>().ok().filter(|n| *n <= i32::MAX as u64)?;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(p) => {
                let labels: Vec<String> = p.split('.').map(str::to_string).collect();
                // Numeric labels must not have leading zeros ("rc.01" is not a version).
                let ok = labels.iter().all(|l| {
                    !l.is_empty()
                        && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                        && !(l.len() > 1 && l.starts_with('0') && l.bytes().all(|b| b.is_ascii_digit()))
                });
                if !ok {
                    return None;
                }
                labels
            }
        };
        Some(Version { parts, pre })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }

    /// The canonical lower-case spelling used in feed URLs:
    /// three numbers, a fourth only when it is not zero, no build metadata.
    pub fn normalized(&self) -> String {
        let mut out = format!("{}.{}.{}", self.parts[0], self.parts[1], self.parts[2]);
        if self.parts[3] != 0 {
            out.push_str(&format!(".{}", self.parts[3]));
        }
        if !self.pre.is_empty() {
            out.push('-');
            out.push_str(&self.pre.join(".").to_ascii_lowercase());
        }
        out
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.parts.cmp(&other.parts).then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater, // a release is newer than its pre-releases
            (false, true) => Ordering::Less,
            (false, false) => {
                for (a, b) in self.pre.iter().zip(&other.pre) {
                    let num = |s: &str| s.bytes().all(|c| c.is_ascii_digit()).then(|| s.trim_start_matches('0').to_string());
                    let ord = match (num(a), num(b)) {
                        // Numbers of any length, compared as numbers.
                        (Some(x), Some(y)) => x.len().cmp(&y.len()).then_with(|| x.cmp(&y)),
                        (Some(_), None) => Ordering::Less,
                        (None, Some(_)) => Ordering::Greater,
                        // Words compare without regard to case.
                        (None, None) => a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()),
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
impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Version {}

/// A version range in NuGet's interval notation.
///
/// `1.0` means "1.0 or higher"; `[1.0]` means exactly 1.0; `[1.0,2.0)` and
/// `(,1.0]` are intervals with inclusive `[ ]` or exclusive `( )` ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Range {
    min: Option<(Version, bool)>,
    max: Option<(Version, bool)>,
}

impl Range {
    pub fn parse(text: &str) -> Option<Range> {
        let t = text.trim();
        if t.is_empty() || t.len() > 256 {
            return None;
        }
        let first = t.chars().next()?;
        if first != '[' && first != '(' {
            // A bare version is a minimum.
            return Some(Range { min: Some((Version::parse(t)?, true)), max: None });
        }
        let last = t.chars().last()?;
        if last != ']' && last != ')' {
            return None;
        }
        let (min_inclusive, max_inclusive) = (first == '[', last == ']');
        let inner = &t[1..t.len() - 1];
        let (lo, hi) = match inner.split_once(',') {
            Some((lo, hi)) => (lo.trim(), hi.trim()),
            None => {
                // "[1.0]" is exactly that version; "(1.0)" is not a range.
                if !(min_inclusive && max_inclusive) {
                    return None;
                }
                let v = Version::parse(inner)?;
                return Some(Range { min: Some((v.clone(), true)), max: Some((v, true)) });
            }
        };
        if hi.contains(',') {
            return None;
        }
        let min = if lo.is_empty() { None } else { Some((Version::parse(lo)?, min_inclusive)) };
        let max = if hi.is_empty() { None } else { Some((Version::parse(hi)?, max_inclusive)) };
        if min.is_none() && max.is_none() {
            return None;
        }
        // An interval that cannot contain anything is a mistake, not a range.
        if let (Some((lo, li)), Some((hi, hi_inc))) = (&min, &max) {
            match lo.cmp(hi) {
                Ordering::Greater => return None,
                Ordering::Equal if !(*li && *hi_inc) => return None,
                _ => {}
            }
        }
        Some(Range { min, max })
    }

    pub fn satisfied_by(&self, v: &Version) -> bool {
        let above = match &self.min {
            None => true,
            Some((min, inclusive)) => match v.cmp(min) {
                Ordering::Greater => true,
                Ordering::Equal => *inclusive,
                Ordering::Less => false,
            },
        };
        let below = match &self.max {
            None => true,
            Some((max, inclusive)) => match v.cmp(max) {
                Ordering::Less => true,
                Ordering::Equal => *inclusive,
                Ordering::Greater => false,
            },
        };
        above && below
    }

    /// The version NuGet restores for this range: the LOWEST one that fits.
    pub fn best_match<'a>(&self, versions: impl IntoIterator<Item = &'a Version>) -> Option<&'a Version> {
        versions.into_iter().filter(|v| self.satisfied_by(v)).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("{s} should parse"))
    }

    #[test]
    fn versions() {
        let order = ["1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta", "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0", "1.0.0.1", "1.0.1", "1.1", "2"];
        for pair in order.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        for (a, b) in [("1.0", "1.0.0.0"), ("1", "1.0.0"), ("01.02.03", "1.2.3"), ("1.0.0-BETA", "1.0.0-beta"), ("1.0.0+build", "1.0.0")] {
            assert_eq!(v(a), v(b), "{a} == {b}");
        }
        assert_eq!(v("1.0").normalized(), "1.0.0");
        assert_eq!(v("1.2.3.0").normalized(), "1.2.3");
        assert_eq!(v("1.2.3.4-RC.1+meta").normalized(), "1.2.3.4-rc.1");
        assert_eq!(v("01.002.0003").normalized(), "1.2.3");
        for bad in ["", "a", "1.", ".1", "1..2", "1.2.3.4.5", "1.0-", "1.0-a..b", "1.0-a_b", "-1", "1.0 beta", "99999999999", "1.0.0-rc.01"] {
            assert!(Version::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn ranges() {
        let yes = [("1.0", "1.0.0"), ("1.0", "9.0"), ("[1.0]", "1.0.0"), ("[1.0,2.0)", "1.9.9"), ("(1.0,)", "1.0.1"), ("(,1.0]", "1.0"), ("(,1.0)", "0.9"), ("[1.0,2.0]", "2.0"), ("[1.0, 2.0)", "2.0.0-beta")];
        let no = [("1.0", "0.9"), ("1.0", "1.0.0-beta"), ("[1.0]", "1.0.1"), ("[1.0,2.0)", "2.0"), ("(1.0,)", "1.0"), ("(,1.0)", "1.0"), ("[1.0,2.0]", "2.0.1")];
        for (r, ver) in yes {
            assert!(Range::parse(r).unwrap().satisfied_by(&v(ver)), "{r} should accept {ver}");
        }
        for (r, ver) in no {
            assert!(!Range::parse(r).unwrap().satisfied_by(&v(ver)), "{r} should refuse {ver}");
        }
        for bad in ["", "[1.0", "1.0]", "(1.0)", "[,]", "(,)", "[2.0,1.0]", "(1.0,1.0]", "[1.0,2.0,3.0]", "[a,b]", "latest"] {
            assert!(Range::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn the_lowest_fitting_version_wins() {
        let all: Vec<Version> = ["1.0.0", "1.5.0", "2.0.0-beta", "2.0.0", "3.1.0"].iter().map(|s| v(s)).collect();
        let pick = |r: &str| Range::parse(r).unwrap().best_match(&all).map(|v| v.normalized());
        assert_eq!(pick("1.2").as_deref(), Some("1.5.0"), "not 3.1.0");
        assert_eq!(pick("[1.0,2.0)").as_deref(), Some("1.0.0"));
        assert_eq!(pick("(1.5,)").as_deref(), Some("2.0.0-beta"));
        assert_eq!(pick("[2.0]").as_deref(), Some("2.0.0"));
        assert_eq!(pick("4.0"), None);
    }
}
