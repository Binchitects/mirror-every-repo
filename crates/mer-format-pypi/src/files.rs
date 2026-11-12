//! Distribution file names and wheel compatibility tags.
use crate::Version;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileKind {
    Wheel { pythons: Vec<String>, abis: Vec<String>, platforms: Vec<String> },
    Sdist,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileName {
    pub version: Version,
    pub kind: FileKind,
}

/// File names end up in URLs, on disk paths and in HTML, so only a
/// conservative character set is accepted.
pub fn safe_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+' | b'!'))
}

/// Parse `name-1.0-py3-none-any.whl` or `name-1.0.tar.gz`. Other file types
/// (eggs, installers) are not mirrored.
pub fn parse_filename(name: &str) -> Option<FileName> {
    if !safe_filename(name) {
        return None;
    }
    if let Some(stem) = name.strip_suffix(".whl") {
        let parts: Vec<&str> = stem.split('-').collect();
        if parts.len() != 5 && parts.len() != 6 {
            return None;
        }
        let n = parts.len();
        let split = |s: &str| s.split('.').map(str::to_string).collect::<Vec<_>>();
        return Some(FileName {
            version: Version::parse(parts[1])?,
            kind: FileKind::Wheel { pythons: split(parts[n - 3]), abis: split(parts[n - 2]), platforms: split(parts[n - 1]) },
        });
    }
    for ext in [".tar.gz", ".zip", ".tar.bz2", ".tar.xz"] {
        if let Some(stem) = name.strip_suffix(ext) {
            let (_, version) = stem.rsplit_once('-')?;
            return Some(FileName { version: Version::parse(version)?, kind: FileKind::Sdist });
        }
    }
    None
}

/// The interpreters and platform the mirror is built for.
#[derive(Clone, Debug)]
pub struct Targets {
    /// Minor versions of CPython 3, for example `[10, 11]`.
    pub minors: Vec<u32>,
    /// CPU architecture as it appears in wheel tags, for example `x86_64`.
    pub arch: String,
    /// Accepted platform tag prefixes, for example `manylinux`, `musllinux`.
    pub libcs: Vec<String>,
}

impl Targets {
    fn platform_ok(&self, plat: &str) -> bool {
        plat == "any"
            || plat == format!("linux_{}", self.arch)
            || (plat.ends_with(&format!("_{}", self.arch)) && self.libcs.iter().any(|l| plat.starts_with(l.as_str())))
    }

    fn python_ok(minor: u32, py: &str, abi: &str) -> bool {
        // "py3", "py311", "cp311": interpreter family + optional version.
        let (family, ver) = py.split_at(py.find(|c: char| c.is_ascii_digit()).unwrap_or(py.len()));
        let tag_minor = match ver.strip_prefix('3') {
            Some("") => None,
            Some(m) => match m.parse::<u32>() {
                Ok(m) => Some(m),
                Err(_) => return false,
            },
            None => return false, // python 2 or nonsense
        };
        match (family, abi) {
            // Pure Python: any Python 3 at or above the tagged minor.
            ("py", "none") => tag_minor.is_none_or(|m| m <= minor),
            // Built for exactly this CPython.
            ("cp", abi) if tag_minor == Some(minor) => abi == format!("cp3{minor}") || abi == "abi3" || abi == "none",
            // Stable ABI or no ABI: built for an older CPython, still loadable.
            ("cp", "abi3") | ("cp", "none") => tag_minor.is_some_and(|m| m < minor),
            _ => false,
        }
    }

    /// Can at least one target interpreter install this wheel?
    pub fn wheel_ok(&self, kind: &FileKind) -> bool {
        let FileKind::Wheel { pythons, abis, platforms } = kind else { return false };
        platforms.iter().any(|p| self.platform_ok(p))
            && self.minors.iter().any(|minor| pythons.iter().any(|py| abis.iter().any(|abi| Self::python_ok(*minor, py, abi))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets() -> Targets {
        Targets { minors: vec![11], arch: "x86_64".into(), libcs: vec!["manylinux".into()] }
    }

    fn ok(name: &str) -> bool {
        targets().wheel_ok(&parse_filename(name).unwrap_or_else(|| panic!("{name} should parse")).kind)
    }

    #[test]
    fn file_names() {
        let f = parse_filename("requests-2.31.0-py3-none-any.whl").unwrap();
        assert_eq!(f.version, Version::parse("2.31.0").unwrap());
        assert_eq!(parse_filename("zope.interface-6.0.tar.gz").unwrap().kind, FileKind::Sdist);
        assert_eq!(parse_filename("my-odd-name-1.0.zip").unwrap().version, Version::parse("1.0").unwrap());
        assert!(parse_filename("pkg-1.0-1-py3-none-any.whl").is_some(), "build tag");
        for bad in ["pkg-1.0.egg", "pkg.whl", "pkg-notaversion-py3-none-any.whl", "../x-1.0.tar.gz", "a/b-1.0.tar.gz",
            ".hidden-1.0.tar.gz", "x-1.0.tar.gz\"><script>", "x 1.0.tar.gz", "pkg-1.0.exe", ""] {
            assert!(parse_filename(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn wheels_for_cpython_311_on_x86_64_linux() {
        for name in [
            "a-1-py3-none-any.whl", "a-1-py2.py3-none-any.whl", "a-1-py311-none-any.whl", "a-1-py38-none-any.whl",
            "a-1-cp311-cp311-manylinux_2_17_x86_64.manylinux2014_x86_64.whl", "a-1-cp311-cp311-manylinux1_x86_64.whl",
            "a-1-cp37-abi3-manylinux_2_28_x86_64.whl", "a-1-cp311-abi3-linux_x86_64.whl", "a-1-cp311-none-any.whl",
            "a-1-cp39-none-any.whl",
        ] {
            assert!(ok(name), "{name} should be installable");
        }
        for name in [
            "a-1-py2-none-any.whl", "a-1-py312-none-any.whl", "a-1-cp310-cp310-manylinux1_x86_64.whl",
            "a-1-cp312-cp312-manylinux_2_17_x86_64.whl", "a-1-cp312-abi3-manylinux_2_17_x86_64.whl",
            "a-1-cp311-cp311-win_amd64.whl", "a-1-cp311-cp311-macosx_11_0_arm64.whl",
            "a-1-cp311-cp311-manylinux_2_17_aarch64.whl", "a-1-cp311-cp311-musllinux_1_1_x86_64.whl",
            "a-1-pp311-pypy311_pp73-manylinux_2_17_x86_64.whl", "a-1-cp27-cp27mu-manylinux1_x86_64.whl",
        ] {
            assert!(!ok(name), "{name} should not be installable");
        }
        let musl = Targets { libcs: vec!["manylinux".into(), "musllinux".into()], ..targets() };
        assert!(musl.wheel_ok(&parse_filename("a-1-cp311-cp311-musllinux_1_1_x86_64.whl").unwrap().kind));
        let two = Targets { minors: vec![10, 12], ..targets() };
        assert!(two.wheel_ok(&parse_filename("a-1-cp312-cp312-manylinux_2_17_x86_64.whl").unwrap().kind));
        assert!(!two.wheel_ok(&parse_filename("a-1-cp311-cp311-manylinux_2_17_x86_64.whl").unwrap().kind));
    }
}
