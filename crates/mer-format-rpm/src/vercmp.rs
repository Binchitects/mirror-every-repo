//! RPM version comparison (`rpmvercmp`) and epoch:version-release.
//!
//! Checked against rpm itself: see `tests/gen_fixture.py`.
use std::cmp::Ordering;

/// Compare two version or release strings the way rpm does.
pub fn rpmvercmp(a: &str, b: &str) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    let part = |c: u8| c.is_ascii_alphanumeric() || c == b'~' || c == b'^';
    loop {
        while let [c, rest @ ..] = a {
            if part(*c) {
                break;
            }
            a = rest;
        }
        while let [c, rest @ ..] = b {
            if part(*c) {
                break;
            }
            b = rest;
        }
        // A tilde sorts before everything, even the end of the string.
        if a.first() == Some(&b'~') || b.first() == Some(&b'~') {
            if a.first() != Some(&b'~') {
                return Ordering::Greater;
            }
            if b.first() != Some(&b'~') {
                return Ordering::Less;
            }
            a = &a[1..];
            b = &b[1..];
            continue;
        }
        // A caret sorts after the end of the string, before anything else.
        if a.first() == Some(&b'^') || b.first() == Some(&b'^') {
            if a.is_empty() {
                return Ordering::Less;
            }
            if b.is_empty() {
                return Ordering::Greater;
            }
            if a.first() != Some(&b'^') {
                return Ordering::Greater;
            }
            if b.first() != Some(&b'^') {
                return Ordering::Less;
            }
            a = &a[1..];
            b = &b[1..];
            continue;
        }
        if a.is_empty() || b.is_empty() {
            break;
        }
        // Take a run of digits, or a run of letters, off the front of each.
        let numeric = a[0].is_ascii_digit();
        let run = |s: &[u8]| s.iter().take_while(|c| if numeric { c.is_ascii_digit() } else { c.is_ascii_alphabetic() }).count();
        let (la, lb) = (run(a), run(b));
        if lb == 0 {
            // Different kinds: a number is newer than letters.
            return if numeric { Ordering::Greater } else { Ordering::Less };
        }
        let (sa, sb) = (&a[..la], &b[..lb]);
        let order = if numeric {
            let strip = |s: &[u8]| s.iter().position(|c| *c != b'0').map_or(&s[s.len()..], |i| &s[i..]).to_vec();
            let (na, nb) = (strip(sa), strip(sb));
            na.len().cmp(&nb.len()).then_with(|| na.cmp(&nb))
        } else {
            sa.cmp(sb)
        };
        if order.is_ne() {
            return order;
        }
        a = &a[la..];
        b = &b[lb..];
    }
    match (a.is_empty(), b.is_empty()) {
        (true, true) => Ordering::Equal,
        (false, _) => Ordering::Greater,
        _ => Ordering::Less,
    }
}

/// Epoch, version and release of a package or of a dependency.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Evr {
    pub epoch: u64,
    pub version: String,
    /// Empty when a dependency names no release: it then matches any.
    pub release: String,
}

impl Evr {
    pub fn new(epoch: Option<&str>, version: &str, release: Option<&str>) -> Evr {
        Evr { epoch: epoch.and_then(|e| e.parse().ok()).unwrap_or(0), version: version.to_string(), release: release.unwrap_or("").to_string() }
    }

    /// `[epoch:]version[-release]`, as rpm writes it.
    pub fn parse(text: &str) -> Evr {
        let (epoch, rest) = match text.split_once(':') {
            Some((e, r)) if !e.is_empty() && e.bytes().all(|c| c.is_ascii_digit()) => (Some(e), r),
            _ => (None, text),
        };
        match rest.rsplit_once('-') {
            Some((v, r)) => Evr::new(epoch, v, Some(r)),
            None => Evr::new(epoch, rest, None),
        }
    }

    /// Order of two packages.
    pub fn cmp_full(&self, other: &Evr) -> Ordering {
        self.epoch.cmp(&other.epoch).then_with(|| rpmvercmp(&self.version, &other.version)).then_with(|| rpmvercmp(&self.release, &other.release))
    }

    /// Compare what a package provides with what a dependency asks for: a
    /// side without a release matches any release.
    pub fn cmp_dep(&self, wanted: &Evr) -> Ordering {
        let base = self.epoch.cmp(&wanted.epoch).then_with(|| rpmvercmp(&self.version, &wanted.version));
        if base.is_ne() || self.release.is_empty() || wanted.release.is_empty() {
            return base;
        }
        rpmvercmp(&self.release, &wanted.release)
    }
}

impl std::fmt::Display for Evr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.epoch > 0 {
            write!(f, "{}:", self.epoch)?;
        }
        write!(f, "{}", self.version)?;
        if !self.release.is_empty() {
            write!(f, "-{}", self.release)?;
        }
        Ok(())
    }
}

/// The comparison a dependency makes: `EQ`, `LT`, `LE`, `GT`, `GE`.
pub fn flag_allows(flag: &str, provided_vs_wanted: Ordering) -> bool {
    match flag {
        "EQ" => provided_vs_wanted.is_eq(),
        "LT" => provided_vs_wanted.is_lt(),
        "LE" => provided_vs_wanted.is_le(),
        "GT" => provided_vs_wanted.is_gt(),
        "GE" => provided_vs_wanted.is_ge(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_equals_rpm_on_every_pair() {
        let truth: serde_json::Value = serde_json::from_str(include_str!("../tests/rpm_truth.json")).unwrap();
        let versions: Vec<&str> = truth["versions"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        let mut wrong = Vec::new();
        for (i, a) in versions.iter().enumerate() {
            for (j, b) in versions.iter().enumerate() {
                let want = truth["cmp"][i][j].as_i64().unwrap().cmp(&0);
                // rpm reads "1.0-1" as version 1.0, release 1; so does Evr::parse.
                // In this full comparison, no release is older than any release.
                let got = Evr::parse(a).cmp_full(&Evr::parse(b));
                if got != want {
                    wrong.push(format!("{a:?} vs {b:?}: rpm says {want:?}, got {got:?}"));
                }
            }
        }
        assert!(wrong.is_empty(), "{} of {} pairs differ:\n{}", wrong.len(), versions.len() * versions.len(), wrong[..wrong.len().min(25)].join("\n"));
        assert!(versions.len() >= 60);
    }

    #[test]
    fn epoch_version_release() {
        let e = |ep: Option<&str>, v: &str, r: Option<&str>| Evr::new(ep, v, r);
        assert!(e(Some("1"), "1.0", Some("1")).cmp_full(&e(None, "9.0", Some("1"))).is_gt(), "the epoch decides first");
        assert!(e(None, "5.1.8", Some("9.el9")).cmp_full(&e(None, "5.1.8", Some("10.el9"))).is_lt());
        // A dependency without a release matches any release of that version.
        assert!(e(None, "2.34", Some("60.el9")).cmp_dep(&e(None, "2.34", None)).is_eq());
        assert!(e(None, "2.34", Some("60.el9")).cmp_dep(&e(None, "2.34", Some("61.el9"))).is_lt());
        assert!(flag_allows("GE", Ordering::Equal) && flag_allows("LT", Ordering::Less) && !flag_allows("GT", Ordering::Equal) && flag_allows("", Ordering::Less));
        assert_eq!(e(Some("2"), "1.0", Some("3")).to_string(), "2:1.0-3");
        assert_eq!(e(None, "1.0", None).to_string(), "1.0");
    }
}
