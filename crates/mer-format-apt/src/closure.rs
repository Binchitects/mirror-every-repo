//! Selective mirroring: the wanted packages plus everything they need.
use crate::{compare_versions, AptError, Stanza};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Parse a dependency field into "all of (any of names)".
/// `a (>= 1), b | c [amd64] <!nocheck>, d:any` -> [[a], [b, c], [d]]
pub fn parse_depends(field: &str) -> Vec<Vec<String>> {
    field
        .split(',')
        .map(|group| {
            group
                .split('|')
                .filter_map(|alt| {
                    let name = alt.trim().split(|c: char| c.is_whitespace() || "([<".contains(c)).next()?;
                    let name = name.split(':').next()?; // drop the :any / :amd64 qualifier
                    (!name.is_empty()).then(|| name.to_string())
                })
                .collect::<Vec<_>>()
        })
        .filter(|group| !group.is_empty())
        .collect()
}

/// The packages of one upstream index: newest version of each name, plus who
/// provides which virtual package.
pub struct Index {
    packages: BTreeMap<String, Stanza>,
    provides: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Closure {
    /// Selected package names, sorted.
    pub selected: BTreeSet<String>,
    /// Dependencies nothing in the index satisfies: (package, dependency group).
    pub unresolved: Vec<(String, String)>,
}

impl Index {
    pub fn new(stanzas: Vec<Stanza>) -> Result<Self, AptError> {
        let mut packages: BTreeMap<String, Stanza> = BTreeMap::new();
        for s in stanzas {
            let (Some(name), Some(version)) = (s.get("Package"), s.get("Version")) else {
                return Err(AptError::Metadata("index entry without Package or Version".into()));
            };
            let name = name.to_string();
            let newer = match packages.get(&name) {
                Some(old) => compare_versions(version, old.get("Version").unwrap_or("")) == Ordering::Greater,
                None => true,
            };
            if newer {
                packages.insert(name, s);
            }
        }
        let mut provides: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (name, s) in &packages {
            for group in parse_depends(s.get("Provides").unwrap_or("")) {
                for virt in group {
                    provides.entry(virt).or_default().insert(name.clone());
                }
            }
        }
        Ok(Index { packages, provides })
    }

    pub fn len(&self) -> usize {
        self.packages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&Stanza> {
        self.packages.get(name)
    }

    /// Pick the package that satisfies one dependency group. A candidate that
    /// is already selected wins. Otherwise alternatives are tried in the order
    /// written, and within one alternative a real package comes before the
    /// packages that merely provide it.
    fn choose(&self, group: &[String], selected: &BTreeSet<String>) -> Option<String> {
        let mut candidates: Vec<String> = Vec::new();
        for alt in group {
            if self.packages.contains_key(alt) {
                candidates.push(alt.clone());
            }
            if let Some(providers) = self.provides.get(alt) {
                candidates.extend(providers.iter().cloned());
            }
        }
        candidates.iter().find(|c| selected.contains(*c)).or(candidates.first()).cloned()
    }

    /// Follow Depends and Pre-Depends (and Recommends when asked) from `roots`.
    /// Version constraints are not evaluated: an index holds one version of a
    /// package, and that is the one that gets mirrored.
    pub fn closure(&self, roots: &[String], recommends: bool) -> Result<Closure, AptError> {
        let mut out = Closure::default();
        let mut queue: VecDeque<String> = VecDeque::new();
        for root in roots {
            if !self.packages.contains_key(root) {
                return Err(AptError::UnknownPackage(root.clone()));
            }
            if out.selected.insert(root.clone()) {
                queue.push_back(root.clone());
            }
        }
        while let Some(name) = queue.pop_front() {
            let stanza = &self.packages[&name];
            let mut fields = vec!["Pre-Depends", "Depends"];
            if recommends {
                fields.push("Recommends");
            }
            for field in fields {
                for group in parse_depends(stanza.get(field).unwrap_or("")) {
                    match self.choose(&group, &out.selected) {
                        Some(dep) => {
                            if out.selected.insert(dep.clone()) {
                                queue.push_back(dep);
                            }
                        }
                        // A missing Recommends is normal; a missing Depends is reported.
                        None if field != "Recommends" => out.unresolved.push((name.clone(), group.join(" | "))),
                        None => {}
                    }
                }
            }
        }
        out.unresolved.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_stanzas;

    fn index(text: &str) -> Index {
        Index::new(parse_stanzas(text).unwrap()).unwrap()
    }

    fn names(c: &Closure) -> Vec<&str> {
        c.selected.iter().map(String::as_str).collect()
    }

    #[test]
    fn dependency_syntax() {
        assert_eq!(
            parse_depends("libc6 (>= 2.34), debconf (>= 0.5) | debconf-2.0, perl:any, foo [amd64 i386] <!nocheck>"),
            vec![vec!["libc6"], vec!["debconf", "debconf-2.0"], vec!["perl"], vec!["foo"]]
        );
        assert!(parse_depends("").is_empty());
        assert!(parse_depends(" , ").is_empty());
        assert_eq!(parse_depends("a(>=1)|b"), vec![vec!["a", "b"]]);
    }

    const WORLD: &str = "\
Package: app
Version: 1.0
Depends: libfoo (>= 1), mail-transport-agent | postfix
Pre-Depends: init-system
Recommends: docs, nonexistent-extra

Package: libfoo
Version: 1.0
Depends: libc

Package: libfoo
Version: 1.2
Depends: libc, libbar

Package: libbar
Version: 0.1

Package: libc
Version: 2.0
Depends: libfoo

Package: postfix
Version: 3
Provides: mail-transport-agent (= 3)

Package: exim
Version: 4
Provides: mail-transport-agent

Package: sysv
Version: 1
Provides: init-system

Package: docs
Version: 1

Package: unrelated
Version: 9
Depends: something-missing
";

    #[test]
    fn closure_follows_depends_and_pre_depends() {
        let c = index(WORLD).closure(&["app".into()], false).unwrap();
        assert_eq!(names(&c), ["app", "exim", "libbar", "libc", "libfoo", "sysv"]);
        assert!(c.unresolved.is_empty());
    }

    #[test]
    fn newest_version_wins_and_its_dependencies_are_used() {
        let i = index(WORLD);
        assert_eq!(i.get("libfoo").unwrap().get("Version"), Some("1.2"));
        // libbar is needed only by libfoo 1.2.
        assert!(i.closure(&["libfoo".into()], false).unwrap().selected.contains("libbar"));
    }

    #[test]
    fn cycles_terminate() {
        // libfoo -> libc -> libfoo
        let c = index(WORLD).closure(&["libc".into()], false).unwrap();
        assert_eq!(names(&c), ["libbar", "libc", "libfoo"]);
    }

    #[test]
    fn an_already_selected_alternative_is_preferred() {
        // With postfix asked for explicitly, the virtual dependency is already
        // satisfied and exim is not pulled in.
        let c = index(WORLD).closure(&["postfix".into(), "app".into()], false).unwrap();
        assert!(c.selected.contains("postfix") && !c.selected.contains("exim"));
    }

    #[test]
    fn recommends_are_optional_and_their_absence_is_not_an_error() {
        let i = index(WORLD);
        assert!(!i.closure(&["app".into()], false).unwrap().selected.contains("docs"));
        let c = i.closure(&["app".into()], true).unwrap();
        assert!(c.selected.contains("docs"));
        assert!(c.unresolved.is_empty(), "missing Recommends are not reported");
    }

    #[test]
    fn missing_hard_dependency_is_reported_and_unknown_root_is_an_error() {
        let i = index(WORLD);
        let c = i.closure(&["unrelated".into()], false).unwrap();
        assert_eq!(c.unresolved, vec![("unrelated".to_string(), "something-missing".to_string())]);
        assert!(matches!(i.closure(&["nope".into()], false), Err(AptError::UnknownPackage(_))));
    }

    #[test]
    fn index_entries_need_name_and_version() {
        assert!(Index::new(parse_stanzas("Package: x\n").unwrap()).is_err());
    }
}
