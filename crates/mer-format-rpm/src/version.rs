//! RPM version comparison, as `rpmvercmp` and `rpm.labelCompare` do it.
use std::cmp::Ordering;

/// `rpmvercmp`: alternating runs of digits and letters; `~` sorts before
/// anything (even the end), `^` after the end but before anything else.
pub fn vercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    let sep = |c: u8| !c.is_ascii_alphanumeric() && c != b'~' && c != b'^';
    loop {
        while a.first().is_some_and(|&c| sep(c)) {
            a = &a[1..];
        }
        while b.first().is_some_and(|&c| sep(c)) {
            b = &b[1..];
        }
        // Tilde: before everything, including the end.
        match (a.first() == Some(&b'~'), b.first() == Some(&b'~')) {
            (true, true) => {
                a = &a[1..];
                b = &b[1..];
                continue;
            }
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        // Caret: after the end, before anything else.
        match (a.first() == Some(&b'^'), b.first() == Some(&b'^')) {
            (true, true) => {
                a = &a[1..];
                b = &b[1..];
                continue;
            }
            (true, false) => return if b.is_empty() { Ordering::Greater } else { Ordering::Less },
            (false, true) => return if a.is_empty() { Ordering::Less } else { Ordering::Greater },
            _ => {}
        }
        if a.is_empty() || b.is_empty() {
            break;
        }
        let numeric = a[0].is_ascii_digit();
        let take = |s: &[u8]| {
            let n = s.iter().position(|c| if numeric { !c.is_ascii_digit() } else { !c.is_ascii_alphabetic() }).unwrap_or(s.len());
            n
        };
        let (na, nb) = (take(a), take(b));
        let (sa, sb) = (&a[..na], &b[..nb]);
        // A run of the other kind on one side: numbers are newer than letters.
        if sb.is_empty() {
            return if numeric { Ordering::Greater } else { Ordering::Less };
        }
        let o = if numeric {
            let (ta, tb) = (trim_zeros(sa), trim_zeros(sb));
            ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb))
        } else {
            sa.cmp(sb)
        };
        if o != Ordering::Equal {
            return o;
        }
        a = &a[na..];
        b = &b[nb..];
    }
    match (a.is_empty(), b.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        _ => Ordering::Greater,
    }
}

fn trim_zeros(s: &[u8]) -> &[u8] {
    let n = s.iter().position(|&c| c != b'0').unwrap_or(s.len());
    &s[n..]
}

/// Epoch, version, release. A missing epoch is 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evr {
    pub epoch: u64,
    pub version: String,
    pub release: String,
}

impl Evr {
    pub fn new(epoch: &str, version: &str, release: &str) -> Evr {
        Evr { epoch: epoch.parse().unwrap_or(0), version: version.into(), release: release.into() }
    }
}

impl Ord for Evr {
    fn cmp(&self, o: &Self) -> Ordering {
        self.epoch.cmp(&o.epoch).then_with(|| vercmp(&self.version, &o.version)).then_with(|| vercmp(&self.release, &o.release))
    }
}

impl PartialOrd for Evr {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl std::fmt::Display for Evr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.epoch > 0 {
            write!(f, "{}:", self.epoch)?;
        }
        write!(f, "{}-{}", self.version, self.release)
    }
}
