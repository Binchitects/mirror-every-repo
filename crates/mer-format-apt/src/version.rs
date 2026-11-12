//! Debian version ordering (Debian Policy 5.6.12), as dpkg implements it.
use std::cmp::Ordering;

/// Sort weight of one character in the non-digit part of a version:
/// `~` sorts before everything, even the end of the string; letters sort
/// before all other characters.
fn weight(c: Option<u8>) -> i32 {
    match c {
        None => 0,
        Some(b'~') => -1,
        Some(c) if c.is_ascii_digit() => 0,
        Some(c) if c.is_ascii_alphabetic() => c as i32,
        Some(c) => c as i32 + 256,
    }
}

fn compare_part(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        // Non-digit run.
        while (i < a.len() && !a[i].is_ascii_digit()) || (j < b.len() && !b[j].is_ascii_digit()) {
            let (wa, wb) = (weight(a.get(i).copied()), weight(b.get(j).copied()));
            if wa != wb {
                return wa.cmp(&wb);
            }
            i += 1;
            j += 1;
        }
        // Digit run: compare as numbers of any length.
        while i < a.len() && a[i] == b'0' {
            i += 1;
        }
        while j < b.len() && b[j] == b'0' {
            j += 1;
        }
        let (si, sj) = (i, j);
        while i < a.len() && a[i].is_ascii_digit() {
            i += 1;
        }
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        let (da, db) = (&a[si..i], &b[sj..j]);
        match da.len().cmp(&db.len()).then_with(|| da.cmp(db)) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

fn split(v: &str) -> (u64, &str, &str) {
    let (epoch, rest) = match v.split_once(':') {
        Some((e, rest)) if !e.is_empty() && e.bytes().all(|b| b.is_ascii_digit()) => {
            (e.parse().unwrap_or(u64::MAX), rest)
        }
        _ => (0, v),
    };
    match rest.rsplit_once('-') {
        Some((upstream, revision)) => (epoch, upstream, revision),
        None => (epoch, rest, ""),
    }
}

/// `[epoch:]upstream[-revision]`
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let (ea, ua, ra) = split(a.trim());
    let (eb, ub, rb) = split(b.trim());
    ea.cmp(&eb).then_with(|| compare_part(ua, ub)).then_with(|| compare_part(ra, rb))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn matches_dpkg_on_known_cases() {
        // (a, b, expected): the same answers as `dpkg --compare-versions`.
        let cases = [
            ("1.0", "1.0", Equal),
            ("1.0", "1.1", Less),
            ("1.10", "1.9", Greater),
            ("1.0-1", "1.0-2", Less),
            ("1.0-10", "1.0-9", Greater),
            ("1:0.9", "2.0", Greater),
            ("0:1.0", "1.0", Equal),
            ("1.0~rc1", "1.0", Less),
            ("1.0~rc1", "1.0~rc2", Less),
            ("1.0~~", "1.0~", Less),
            ("1.0", "1.0+b1", Less),
            ("1.0+dfsg", "1.0", Greater),
            ("1.0a", "1.0+", Less),
            ("1.0-1", "1.0-1+deb12u1", Less),
            ("2.36-9+deb12u10", "2.36-9+deb12u9", Greater),
            ("1.0-1~bpo12+1", "1.0-1", Less),
            ("1.0", "1.0-0", Equal),
            ("1.0", "1.0-1", Less),
            ("1.2.3", "1.2", Greater),
            ("1.02", "1.2", Equal),
            ("1.0-1.1", "1.0-1", Greater),
            ("12.2.0-14+deb12u1", "12.2.0-14", Greater),
            ("1.a", "1.1", Greater), // a letter outweighs "no letter here" (the other side has a digit)
            ("7.88.1-10+deb12u12", "7.88.1-10+deb12u5", Greater),
            ("3.0.0~alpha", "3.0.0~beta", Less),
            ("1-2-3", "1-2-4", Less), // revision is after the LAST hyphen
            ("1.0.", "1.0", Greater),
        ];
        for (a, b, want) in cases {
            assert_eq!(compare_versions(a, b), want, "{a} vs {b}");
            assert_eq!(compare_versions(b, a), want.reverse(), "{b} vs {a}");
        }
    }

    /// Ask the real dpkg about every pair of a mixed bag of versions.
    #[test]
    fn agrees_with_real_dpkg_on_every_pair() {
        use std::process::Command;
        if Command::new("dpkg").arg("--version").output().is_err() {
            eprintln!("skipped: dpkg not available");
            return;
        }
        let versions = [
            "1.0", "1.0-1", "1.0-1+deb12u1", "1.0~rc1", "1.0~~", "1.0+b1", "1.0+dfsg-1", "1:0.5", "2:0.1-1",
            "0.9.9", "1.a", "1.1", "1.0.", "1.02", "1.2", "1.10", "1.0-1~bpo12+1", "1.0-10", "1.0-9",
            "2.36-9+deb12u10", "2.36-9+deb12u9", "1-2-3", "1-2-4", "1.0a", "1.0+", "0", "1.0-0",
        ];
        let mut checked = 0;
        for a in versions {
            for b in versions {
                let dpkg_lt = Command::new("dpkg").args(["--compare-versions", a, "lt", b]).status().unwrap().success();
                let dpkg_eq = Command::new("dpkg").args(["--compare-versions", a, "eq", b]).status().unwrap().success();
                let want = if dpkg_lt { Less } else if dpkg_eq { Equal } else { Greater };
                assert_eq!(compare_versions(a, b), want, "{a} vs {b}");
                checked += 1;
            }
        }
        eprintln!("dpkg cross-check: {checked} pairs agree");
    }

    #[test]
    fn ordering_is_consistent() {
        let mut v = vec!["1.0", "1.0~rc1", "2:0.1", "1.0-1", "1.0+b1", "0.9.9", "1.0~~", "1.0-1+deb12u1"];
        v.sort_by(|a, b| compare_versions(a, b));
        assert_eq!(v, ["0.9.9", "1.0~~", "1.0~rc1", "1.0", "1.0-1", "1.0-1+deb12u1", "1.0+b1", "2:0.1"]);
    }
}
