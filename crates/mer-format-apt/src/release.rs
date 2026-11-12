//! The `Release` file: the signed list of index files of one suite.
use crate::parse_stanzas;
use mer_core::Digest;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseFile {
    /// Relative to `dists/<suite>/`.
    pub path: String,
    pub size: u64,
    pub sha256: Digest,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Release {
    pub suite: Option<String>,
    pub codename: Option<String>,
    pub date: Option<String>,
    pub valid_until: Option<String>,
    pub files: Vec<ReleaseFile>,
}

impl Release {
    pub fn file(&self, path: &str) -> Option<&ReleaseFile> {
        self.files.iter().find(|f| f.path == path)
    }
}

pub fn parse_release(text: &str) -> Result<Release, String> {
    let stanzas = parse_stanzas(text)?;
    let [s] = stanzas.as_slice() else {
        return Err(format!("Release must be one stanza, found {}", stanzas.len()));
    };
    let mut files = Vec::new();
    for line in s.get("SHA256").ok_or("Release has no SHA256 section")?.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }
        let [hash, size, path] = parts.as_slice() else {
            return Err(format!("bad SHA256 line {line:?}"));
        };
        files.push(ReleaseFile {
            path: path.to_string(),
            size: size.parse().map_err(|_| format!("bad size in {line:?}"))?,
            sha256: Digest::from_hex(hash).map_err(|_| format!("bad hash in {line:?}"))?,
        });
    }
    let field = |n: &str| s.get(n).map(str::to_string);
    Ok(Release {
        suite: field("Suite"),
        codename: field("Codename"),
        date: field("Date"),
        valid_until: field("Valid-Until"),
        files,
    })
}

/// Render the Release of our own subset repository. There is deliberately no
/// `Valid-Until`: an air-gapped vault must not stop working because a
/// transfer was late.
pub fn render_release(
    origin: &str,
    upstream: &Release,
    suite: &str,
    architectures: &[String],
    components: &[String],
    files: &[ReleaseFile],
) -> String {
    let mut out = format!("Origin: {origin}\nLabel: {origin}\n");
    out.push_str(&format!("Suite: {}\n", upstream.suite.as_deref().unwrap_or(suite)));
    out.push_str(&format!("Codename: {}\n", upstream.codename.as_deref().unwrap_or(suite)));
    if let Some(date) = &upstream.date {
        out.push_str(&format!("Date: {date}\n"));
    }
    out.push_str(&format!("Architectures: {}\n", architectures.join(" ")));
    out.push_str(&format!("Components: {}\n", components.join(" ")));
    out.push_str("Acquire-By-Hash: yes\nSHA256:\n");
    for f in files {
        out.push_str(&format!(" {} {:>16} {}\n", f.sha256, f.size, f.path));
    }
    out
}

/// Parse an RFC 2822 style date as used in Release files
/// (`Sat, 08 Nov 2026 14:23:05 UTC`) into unix seconds. Only UTC is accepted.
pub fn parse_date(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    let parts = if parts.first()?.ends_with(',') { &parts[1..] } else { &parts[..] };
    let [day, month, year, time, zone] = parts else { return None };
    if !matches!(*zone, "UTC" | "GMT" | "+0000" | "Z") {
        return None;
    }
    let day: i64 = day.parse().ok()?;
    let year: i64 = year.parse().ok()?;
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
        .iter()
        .position(|m| m.eq_ignore_ascii_case(month))? as i64
        + 1;
    let hms: Vec<i64> = time.split(':').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let [h, m, sec] = hms.as_slice() else { return None };
    if !(1..=31).contains(&day) || !(0..24).contains(h) || !(0..60).contains(m) || !(0..=60).contains(sec) {
        return None;
    }
    // Days since 1970-01-01 (proleptic Gregorian calendar).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + m * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE: &str = "Origin: Debian\nSuite: stable\nCodename: bookworm\nDate: Sat, 06 Sep 2025 09:41:36 UTC\nValid-Until: Sat, 13 Sep 2025 09:41:36 UTC\nMD5Sum:\n 0123456789abcdef0123456789abcdef 10 main/binary-amd64/Packages\nSHA256:\n ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  8787088 main/binary-amd64/Packages.gz\n ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad 47000000 main/binary-amd64/Packages\n";

    #[test]
    fn parses_fields_and_the_sha256_section_only() {
        let r = parse_release(RELEASE).unwrap();
        assert_eq!((r.suite.as_deref(), r.codename.as_deref()), (Some("stable"), Some("bookworm")));
        assert_eq!(r.files.len(), 2, "MD5 entries are ignored");
        let f = r.file("main/binary-amd64/Packages.gz").unwrap();
        assert_eq!(f.size, 8_787_088);
        assert_eq!(f.sha256, Digest::of(b"abc"));
        assert!(r.file("contrib/binary-amd64/Packages.gz").is_none());
    }

    #[test]
    fn rejects_broken_release_files() {
        assert!(parse_release("Suite: x\n").is_err(), "no SHA256 section");
        assert!(parse_release("Suite: x\nSHA256:\n nothex 10 a\n").is_err());
        assert!(parse_release("Suite: x\nSHA256:\n ba78 ten a\n").is_err());
        assert!(parse_release("Suite: x\nSHA256:\n a b\n").is_err());
        assert!(parse_release(&format!("{RELEASE}\nSuite: second-stanza\n")).is_err());
    }

    #[test]
    fn our_release_round_trips_and_never_expires() {
        let up = parse_release(RELEASE).unwrap();
        let files = vec![ReleaseFile { path: "main/binary-amd64/Packages".into(), size: 1234, sha256: Digest::of(b"x") }];
        let text = render_release("Site Mirror", &up, "bookworm", &["amd64".into()], &["main".into()], &files);
        let back = parse_release(&text).unwrap();
        assert_eq!(back.files, files);
        assert_eq!(back.codename.as_deref(), Some("bookworm"));
        assert_eq!(back.date, up.date);
        assert_eq!(back.valid_until, None);
        assert!(text.contains("Acquire-By-Hash: yes"));
    }

    #[test]
    fn dates() {
        assert_eq!(parse_date("Thu, 01 Jan 1970 00:00:00 UTC"), Some(0));
        assert_eq!(parse_date("Sat, 06 Sep 2025 09:41:36 UTC"), Some(1_757_151_696));
        assert_eq!(parse_date("29 Feb 2024 12:00:00 +0000"), Some(1_709_208_000));
        assert_eq!(parse_date("Fri, 31 Dec 1999 23:59:59 GMT"), Some(946_684_799));
        for bad in ["", "yesterday", "Sat, 06 Sep 2025 09:41:36 CEST", "Sat, 06 Foo 2025 09:41:36 UTC", "Sat, 40 Sep 2025 09:41:36 UTC"] {
            assert_eq!(parse_date(bad), None, "{bad:?}");
        }
    }
}
