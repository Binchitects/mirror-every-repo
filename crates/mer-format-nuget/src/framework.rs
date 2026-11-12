//! Target frameworks, and which of a package's dependency groups a project uses.
//!
//! A package lists its dependencies per target framework. A project restores
//! the group of the "nearest" framework it is compatible with. This module
//! covers the frameworks in practical use: modern .NET (net5.0 and later),
//! .NET Core, .NET Standard and .NET Framework.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Framework {
    /// A group without a target framework: applies to everything.
    Any,
    /// .NET 5 and later, optionally for one platform and platform version
    /// (`net8.0-windows10.0` is windows, (10, 0); `net8.0-windows` is (0, 0)).
    Net { major: u32, minor: u32, platform: Option<(String, (u32, u32))> },
    /// .NET Core 1.0 to 3.1.
    NetCoreApp { major: u32, minor: u32 },
    NetStandard { major: u32, minor: u32 },
    /// .NET Framework, as (4, 6, 2) for 4.6.2.
    NetFramework(u32, u32, u32),
    /// Something this mirror does not reason about (portable profiles, UAP, ...).
    Other,
}

fn two(text: &str) -> Option<(u32, u32)> {
    let mut it = text.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().map_or(Some(0), |m| m.parse().ok())?;
    // A third component only if it is zero ("3.1.0").
    it.all(|x| x == "0").then_some((major, minor))
}

fn three(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.split('.').map(|p| p.parse::<u32>().ok());
    let a = parts.next()??;
    let b = parts.next().unwrap_or(Some(0))?;
    let c = parts.next().unwrap_or(Some(0))?;
    parts.next().is_none().then_some((a, b, c))
}

impl Framework {
    /// Accepts both spellings found in feeds: `.NETStandard2.0` and `netstandard2.0`.
    pub fn parse(text: &str) -> Framework {
        let t = text.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("any") {
            return Framework::Any;
        }
        let lower = t.to_ascii_lowercase();
        let modern = |major: u32, minor: u32, platform: Option<(String, (u32, u32))>| {
            if major >= 5 {
                Framework::Net { major, minor, platform }
            } else {
                Framework::NetCoreApp { major, minor }
            }
        };
        if let Some(v) = lower.strip_prefix(".netstandard").or_else(|| lower.strip_prefix("netstandard")) {
            return two(v).map_or(Framework::Other, |(major, minor)| Framework::NetStandard { major, minor });
        }
        if let Some(v) = lower.strip_prefix(".netcoreapp").or_else(|| lower.strip_prefix("netcoreapp")) {
            return two(v).map_or(Framework::Other, |(a, b)| modern(a, b, None));
        }
        if let Some(v) = lower.strip_prefix(".netframework") {
            return three(v).map_or(Framework::Other, |(a, b, c)| Framework::NetFramework(a, b, c));
        }
        if let Some(v) = lower.strip_prefix("net") {
            let (version, platform) = match v.split_once('-') {
                Some((ver, p)) => {
                    let name = p.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
                    let version = if name.len() == p.len() { Some((0, 0)) } else { two(&p[name.len()..]) };
                    match version {
                        Some(pv) if !name.is_empty() => (ver, Some((name.to_string(), pv))),
                        _ => return Framework::Other,
                    }
                }
                None => (v, None),
            };
            if version.contains('.') {
                // net5.0, net8.0-windows7.0
                return match two(version) {
                    Some((a, b)) if a >= 5 => Framework::Net { major: a, minor: b, platform },
                    _ => Framework::Other,
                };
            }
            // net48, net472, net20: digits without dots are .NET Framework.
            let digits: Vec<u32> = version.chars().filter_map(|c| c.to_digit(10)).collect();
            if platform.is_none() && digits.len() == version.len() && (2..=3).contains(&digits.len()) {
                return Framework::NetFramework(digits[0], digits[1], digits.get(2).copied().unwrap_or(0));
            }
        }
        Framework::Other
    }

    /// How well a dependency group for `group` fits a project targeting
    /// `self`. `None` means the project cannot use it; otherwise higher is
    /// nearer.
    fn fit(&self, group: &Framework) -> Option<(u8, u32, u32, u32, u8)> {
        use Framework::*;
        // The .NET Standard version a framework can consume.
        let standard_ceiling = match self {
            Net { .. } => Some((2, 1)),
            NetCoreApp { major: 3, .. } => Some((2, 1)),
            NetCoreApp { major: 2, .. } => Some((2, 0)),
            NetCoreApp { major: 1, .. } => Some((1, 6)),
            NetCoreApp { .. } => None,
            NetStandard { major, minor } => Some((*major, *minor)),
            NetFramework(a, b, c) => match (*a, *b, *c) {
                v if v >= (4, 6, 1) => Some((2, 0)),
                v if v >= (4, 6, 0) => Some((1, 3)),
                v if v >= (4, 5, 1) => Some((1, 2)),
                v if v >= (4, 5, 0) => Some((1, 1)),
                _ => None,
            },
            Any | Other => None,
        };
        match (self, group) {
            (_, Any) => Some((0, 0, 0, 0, 0)),
            (Net { major, minor, platform }, Net { major: gm, minor: gn, platform: gp }) => {
                if (*gm, *gn) > (*major, *minor) {
                    return None;
                }
                match (platform, gp) {
                    (_, None) => Some((4, *gm, *gn, 0, 0)),
                    // A platform-specific group needs a project for that
                    // platform, at that platform version or newer.
                    (Some((p, pv)), Some((g, gv))) if p == g && gv <= pv => Some((4, *gm, *gn, 0, 1)),
                    _ => None,
                }
            }
            (Net { .. }, NetCoreApp { major, minor }) => Some((3, *major, *minor, 0, 0)),
            (NetCoreApp { major, minor }, NetCoreApp { major: gm, minor: gn }) => ((*gm, *gn) <= (*major, *minor)).then_some((3, *gm, *gn, 0, 0)),
            (NetFramework(a, b, c), NetFramework(ga, gb, gc)) => ((*ga, *gb, *gc) <= (*a, *b, *c)).then_some((3, *ga, *gb, *gc, 0)),
            (_, NetStandard { major, minor }) => {
                let (cm, cn) = standard_ceiling?;
                ((*major, *minor) <= (cm, cn)).then_some((1, *major, *minor, 0, 0))
            }
            _ => None,
        }
    }

    /// Index of the group this project restores from, if any is compatible.
    pub fn nearest(&self, groups: &[Framework]) -> Option<usize> {
        groups.iter().enumerate().filter_map(|(i, g)| self.fit(g).map(|rank| (rank, i))).max_by(|a, b| a.0.cmp(&b.0)).map(|(_, i)| i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Framework::*;

    #[test]
    fn spellings() {
        assert_eq!(Framework::parse("net8.0"), Net { major: 8, minor: 0, platform: None });
        assert_eq!(Framework::parse("net6.0-windows7.0"), Net { major: 6, minor: 0, platform: Some(("windows".into(), (7, 0))) });
        assert_eq!(Framework::parse("net8.0-windows"), Net { major: 8, minor: 0, platform: Some(("windows".into(), (0, 0))) });
        assert_eq!(Framework::parse(".NETCoreApp3.1"), NetCoreApp { major: 3, minor: 1 });
        assert_eq!(Framework::parse("netcoreapp5.0"), Net { major: 5, minor: 0, platform: None });
        assert_eq!(Framework::parse(".NETStandard2.0"), NetStandard { major: 2, minor: 0 });
        assert_eq!(Framework::parse("netstandard2.1"), NetStandard { major: 2, minor: 1 });
        assert_eq!(Framework::parse(".NETFramework4.6.2"), NetFramework(4, 6, 2));
        assert_eq!(Framework::parse("net462"), NetFramework(4, 6, 2));
        assert_eq!(Framework::parse("net48"), NetFramework(4, 8, 0));
        assert_eq!(Framework::parse(""), Any);
        for other in [".NETPortable0.0-Profile259", "uap10.0", "monoandroid", "net", "netX", "nonsense"] {
            assert_eq!(Framework::parse(other), Other, "{other}");
        }
    }

    #[test]
    fn nearest_group() {
        fn pick<'a>(project: &str, groups: &[&'a str]) -> Option<&'a str> {
            let parsed: Vec<Framework> = groups.iter().map(|g| Framework::parse(g)).collect();
            Framework::parse(project).nearest(&parsed).map(|i| groups[i])
        }
        assert_eq!(pick("net8.0", &[".NETFramework4.5", ".NETStandard2.0", "net6.0"]), Some("net6.0"));
        assert_eq!(pick("net8.0", &[".NETStandard1.0", ".NETStandard2.0"]), Some(".NETStandard2.0"));
        assert_eq!(pick("net8.0", &[".NETFramework4.5"]), None);
        assert_eq!(pick("net8.0", &["net9.0"]), None, "a group for a newer framework is not usable");
        assert_eq!(pick("net48", &[".NETFramework4.5", ".NETStandard2.0"]), Some(".NETFramework4.5"));
        assert_eq!(pick("netstandard2.0", &[".NETStandard2.1", ""]), Some(""));
    }
}
