//! Which packages an installation needs: the named ones and, recursively,
//! something that provides each thing they require.
use crate::repodata::{Cap, Package};
use crate::vercmp::{flag_allows, Evr};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

pub struct Index {
    pub packages: Vec<Package>,
    by_name: HashMap<String, Vec<usize>>,
    /// capability -> (package, what it provides it as)
    provides: HashMap<String, Vec<(usize, Option<Evr>)>>,
    files: HashMap<String, Vec<usize>>,
}

impl Index {
    pub fn new(packages: Vec<Package>) -> Index {
        let mut index = Index { packages, by_name: HashMap::new(), provides: HashMap::new(), files: HashMap::new() };
        for (i, p) in index.packages.iter().enumerate() {
            index.by_name.entry(p.name.clone()).or_default().push(i);
            for c in &p.provides {
                index.provides.entry(c.name.clone()).or_default().push((i, c.evr.clone()));
            }
            for f in &p.files {
                index.files.entry(f.clone()).or_default().push(i);
            }
        }
        index
    }

    /// File paths found in the separate file lists: (path, package ids).
    pub fn add_files(&mut self, found: &BTreeMap<String, BTreeSet<String>>) {
        let by_id: HashMap<&str, usize> = self.packages.iter().enumerate().map(|(i, p)| (p.pkgid.as_str(), i)).collect();
        for (path, ids) in found {
            let list = self.files.entry(path.clone()).or_default();
            for id in ids {
                if let Some(i) = by_id.get(id.as_str()) {
                    if !list.contains(i) {
                        list.push(*i);
                    }
                }
            }
        }
    }

    /// Packages that satisfy a requirement.
    fn providers(&self, want: &Cap) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        for (i, provided) in self.provides.get(&want.name).map(Vec::as_slice).unwrap_or_default() {
            let ok = match (&want.evr, provided) {
                // A side without a version matches any version of the other.
                (Some(w), Some(p)) => flag_allows(&want.flag, p.cmp_dep(w)),
                _ => true,
            };
            if ok && !out.contains(i) {
                out.push(*i);
            }
        }
        if want.name.starts_with('/') {
            for i in self.files.get(&want.name).map(Vec::as_slice).unwrap_or_default() {
                if !out.contains(i) {
                    out.push(*i);
                }
            }
        }
        out
    }

    /// Of several packages that would do, the one to take: the one whose
    /// name is what was asked for, else the shortest name; of that name the
    /// best architecture, then the newest.
    fn best(&self, candidates: &[usize], wanted: &str, archs: &[String]) -> Option<usize> {
        let arch_rank = |i: usize| archs.iter().position(|a| *a == self.packages[i].arch).unwrap_or(archs.len());
        let name = candidates
            .iter()
            .map(|i| self.packages[*i].name.as_str())
            .min_by(|a, b| (*a != wanted).cmp(&(*b != wanted)).then(a.len().cmp(&b.len())).then(a.cmp(b)))?;
        candidates
            .iter()
            .copied()
            .filter(|i| self.packages[*i].name == name)
            .min_by(|a, b| arch_rank(*a).cmp(&arch_rank(*b)).then_with(|| self.packages[*b].evr.cmp_full(&self.packages[*a].evr)).then(a.cmp(b)))
    }
}

/// A requirement, possibly a boolean one: `(a or b)`, `(a if b)`.
#[derive(Debug, PartialEq, Eq)]
enum Want {
    One(Cap),
    All(Vec<Want>),
    Any(Vec<Want>),
    /// Needed only under a condition: taken if it can be, never an error.
    Soft(Box<Want>),
}

fn atom(text: &str) -> Want {
    let mut words = text.split_whitespace();
    let name = words.next().unwrap_or("").to_string();
    let flag = match words.next() {
        Some("=") => "EQ",
        Some("<") => "LT",
        Some("<=") => "LE",
        Some(">") => "GT",
        Some(">=") => "GE",
        _ => "",
    };
    let evr = words.next().filter(|_| !flag.is_empty()).map(Evr::parse);
    Want::One(Cap { name, flag: flag.to_string(), evr })
}

/// Split at an operator that is not inside parentheses.
fn split_top<'a>(text: &'a str, op: &str) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let (mut depth, mut last, mut i) = (0usize, 0usize, 0usize);
    let bytes = text.as_bytes();
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            _ if depth == 0 && text[i..].starts_with(op) => {
                parts.push(&text[last..i]);
                i += op.len();
                last = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&text[last..]);
    parts
}

fn parse_want(text: &str) -> Want {
    let t = text.trim();
    let Some(inner) = t.strip_prefix('(').and_then(|x| x.strip_suffix(')')) else { return atom(t) };
    // The condition of "if" and "unless" is about what is installed, which
    // is not known here; what it guards is taken where it exists.
    for op in [" if ", " unless "] {
        let parts = split_top(inner, op);
        if parts.len() > 1 {
            let mut all = vec![Want::Soft(Box::new(parse_want(parts[0])))];
            if let Some(other) = split_top(parts[1], " else ").get(1) {
                all.push(Want::Soft(Box::new(parse_want(other))));
            }
            return Want::All(all);
        }
    }
    let alternatives = split_top(inner, " or ");
    if alternatives.len() > 1 {
        return Want::Any(alternatives.into_iter().map(parse_want).collect());
    }
    let all = split_top(inner, " and ");
    if all.len() > 1 {
        return Want::All(all.into_iter().map(parse_want).collect());
    }
    // "a with b" and "a without b" are about one package: a decides.
    for op in [" with ", " without "] {
        let parts = split_top(inner, op);
        if parts.len() > 1 {
            return parse_want(parts[0]);
        }
    }
    parse_want(inner)
}

#[derive(Debug, Default)]
pub struct Resolution {
    /// Indexes into `Index::packages`.
    pub selected: BTreeSet<usize>,
    pub warnings: Vec<String>,
    /// File paths that are required and that the package list does not place.
    pub unplaced_files: BTreeSet<String>,
}

struct Walk<'a> {
    index: &'a Index,
    archs: &'a [String],
    out: Resolution,
    queue: VecDeque<usize>,
}

impl Walk<'_> {
    fn take(&mut self, i: usize) {
        if self.out.selected.insert(i) {
            self.queue.push_back(i);
        }
    }

    /// Returns whether the requirement is met.
    fn satisfy(&mut self, want: &Want) -> bool {
        match want {
            Want::One(cap) => {
                let candidates = self.index.providers(cap);
                if candidates.iter().any(|i| self.out.selected.contains(i)) {
                    return true;
                }
                match self.index.best(&candidates, &cap.name, self.archs) {
                    Some(i) => {
                        self.take(i);
                        true
                    }
                    None => {
                        if cap.name.starts_with('/') {
                            self.out.unplaced_files.insert(cap.name.clone());
                        }
                        false
                    }
                }
            }
            Want::All(list) => {
                // Every part is tried, also after one has failed.
                let mut ok = true;
                for w in list {
                    ok &= self.satisfy(w);
                }
                ok
            }
            Want::Any(list) => {
                // One that is already met is enough; else the first that can be.
                let already = |w: &Want| matches!(w, Want::One(c) if self.index.providers(c).iter().any(|i| self.out.selected.contains(i)));
                list.iter().any(already) || list.iter().any(|w| self.satisfy(w))
            }
            Want::Soft(inner) => {
                self.satisfy(inner);
                true
            }
        }
    }
}

/// The named packages and everything they need. `archs` are the accepted
/// architectures, best first (`noarch` is always accepted).
pub fn resolve(index: &Index, roots: &[String], archs: &[String], recommends: bool) -> Result<Resolution, String> {
    let mut walk = Walk { index, archs, out: Resolution::default(), queue: VecDeque::new() };
    for root in roots {
        // A package name, or anything a package provides (a command, a library).
        let by_name = index.by_name.get(root).cloned().unwrap_or_default();
        let candidates = if by_name.is_empty() { index.providers(&Cap { name: root.clone(), flag: String::new(), evr: None }) } else { by_name };
        match index.best(&candidates, root, archs) {
            Some(i) => walk.take(i),
            None => return Err(root.clone()),
        }
    }
    while let Some(i) = walk.queue.pop_front() {
        let package = &index.packages[i];
        for cap in &package.requires {
            if cap.name.starts_with("rpmlib(") {
                continue; // a feature of rpm itself, not a package
            }
            if !walk.satisfy(&parse_want(&cap.name).with_version(cap)) {
                let w = format!("nothing provides {} needed by {}-{}", cap.name, package.name, package.evr);
                if !walk.out.warnings.contains(&w) {
                    walk.out.warnings.push(w);
                }
            }
        }
        if recommends {
            for cap in &package.recommends {
                walk.satisfy(&Want::Soft(Box::new(parse_want(&cap.name).with_version(cap))));
            }
        }
    }
    Ok(walk.out)
}

impl Want {
    /// A plain requirement carries its version in the attributes of the
    /// entry, not in its name.
    fn with_version(self, cap: &Cap) -> Want {
        match self {
            Want::One(c) if c.flag.is_empty() => Want::One(Cap { name: c.name, flag: cap.flag.clone(), evr: cap.evr.clone() }),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(text: &str) -> Cap {
        if text.starts_with('(') {
            // A boolean requirement is carried whole, in the name.
            return Cap { name: text.to_string(), flag: String::new(), evr: None };
        }
        match atom(text) {
            Want::One(c) => c,
            _ => unreachable!(),
        }
    }

    fn pkg(name: &str, evr: &str, arch: &str, provides: &[&str], requires: &[&str], files: &[&str]) -> Package {
        let mut provides: Vec<Cap> = provides.iter().map(|p| cap(p)).collect();
        provides.push(Cap { name: name.into(), flag: "EQ".into(), evr: Some(Evr::parse(evr)) });
        Package {
            name: name.into(),
            arch: arch.into(),
            evr: Evr::parse(evr),
            pkgid: format!("{name}-{evr}.{arch}"),
            checksum_type: "sha256".into(),
            href: format!("Packages/{name}-{evr}.{arch}.rpm"),
            provides,
            requires: requires.iter().map(|r| cap(r)).collect(),
            recommends: Vec::new(),
            files: files.iter().map(|f| f.to_string()).collect(),
        }
    }

    fn names(index: &Index, r: &Resolution) -> Vec<String> {
        let mut v: Vec<String> = r.selected.iter().map(|i| format!("{}-{}.{}", index.packages[*i].name, index.packages[*i].evr, index.packages[*i].arch)).collect();
        v.sort();
        v
    }

    fn archs() -> Vec<String> {
        vec!["x86_64".into(), "noarch".into()]
    }

    #[test]
    fn requirements_are_followed_to_what_provides_them() {
        let index = Index::new(vec![
            pkg("app", "1.0-1", "x86_64", &[], &["libfoo.so.1()(64bit)", "/usr/bin/sh", "data >= 2", "rpmlib(Zstd) <= 5", "config(app) = 1.0-1"], &[]),
            pkg("app", "0.9-1", "x86_64", &[], &[], &[]),
            pkg("foo-libs", "3-1", "x86_64", &["libfoo.so.1()(64bit)"], &["glibc"], &[]),
            pkg("foo-libs", "3-1", "i686", &["libfoo.so.1"], &[], &[]),
            pkg("bash", "5-1", "x86_64", &[], &[], &["/usr/bin/sh", "/usr/bin/bash"]),
            pkg("data", "1-1", "noarch", &[], &[], &[]),
            pkg("data", "2.5-1", "noarch", &[], &[], &[]),
            pkg("glibc", "2.34-1", "x86_64", &[], &[], &[]),
            pkg("glibc", "2.34-2", "x86_64", &[], &[], &[]),
            pkg("unrelated", "1-1", "x86_64", &[], &[], &[]),
        ]);
        let r = resolve(&index, &["app".into()], &archs(), false).unwrap();
        assert_eq!(names(&index, &r), ["app-1.0-1.x86_64", "bash-5-1.x86_64", "data-2.5-1.noarch", "foo-libs-3-1.x86_64", "glibc-2.34-2.x86_64"]);
        assert_eq!(r.warnings, ["nothing provides config(app) needed by app-1.0-1"]);
        assert!(r.unplaced_files.is_empty());
        // By what a package provides, and what cannot be found.
        let by_file = resolve(&index, &["/usr/bin/bash".into()], &archs(), false).unwrap();
        assert_eq!(names(&index, &by_file), ["bash-5-1.x86_64"]);
        assert_eq!(resolve(&index, &["nope".into()], &archs(), false).unwrap_err(), "nope");
    }

    #[test]
    fn versions_choices_and_boolean_requirements() {
        let index = Index::new(vec![
            pkg("app", "1-1", "x86_64", &[], &["lib < 2", "(langpack-en or langpack-all)", "(extra if gui)", "(tool >= 1 with tool < 3)", "/opt/odd/file", "mta"], &[]),
            pkg("lib", "1.5-1", "x86_64", &[], &[], &[]),
            pkg("lib", "2.0-1", "x86_64", &[], &[], &[]),
            pkg("langpack-all", "1-1", "x86_64", &[], &[], &[]),
            pkg("langpack-en", "1-1", "x86_64", &[], &[], &[]),
            pkg("extra", "1-1", "x86_64", &[], &[], &[]),
            pkg("tool", "2-1", "x86_64", &[], &[], &[]),
            pkg("postfix-long-name", "1-1", "x86_64", &["mta"], &[], &[]),
            pkg("exim", "1-1", "x86_64", &["mta"], &[], &[]),
            pkg("odd", "1-1", "x86_64", &[], &[], &[]),
        ]);
        let r = resolve(&index, &["app".into()], &archs(), false).unwrap();
        assert_eq!(names(&index, &r), ["app-1-1.x86_64", "exim-1-1.x86_64", "extra-1-1.x86_64", "langpack-en-1-1.x86_64", "lib-1.5-1.x86_64", "tool-2-1.x86_64"]);
        assert_eq!(r.unplaced_files.iter().collect::<Vec<_>>(), ["/opt/odd/file"]);

        // The file lists place the file; then it resolves.
        let mut index = index;
        index.add_files(&[("/opt/odd/file".to_string(), ["odd-1-1.x86_64".to_string()].into_iter().collect())].into_iter().collect());
        let r = resolve(&index, &["app".into()], &archs(), false).unwrap();
        assert!(names(&index, &r).contains(&"odd-1-1.x86_64".to_string()) && r.unplaced_files.is_empty() && r.warnings.is_empty());

        assert_eq!(parse_want("(a or (b and c))"), Want::Any(vec![atom("a"), Want::All(vec![atom("b"), atom("c")])]));
        assert_eq!(parse_want("(a if b else c)"), Want::All(vec![Want::Soft(Box::new(atom("a"))), Want::Soft(Box::new(atom("c")))]));
        assert_eq!(parse_want("glibc >= 2.34"), atom("glibc >= 2.34"));
    }
}
