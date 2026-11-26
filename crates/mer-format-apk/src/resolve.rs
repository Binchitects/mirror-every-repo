//! What to mirror so that apk can install the wanted packages.
//!
//! Requirements are followed by name and capability, not solved: version
//! constraints are ignored and the newest package of each name is kept (an
//! index normally holds one version of each). A capability is met by the
//! package of that name, else by the provider with the highest priority
//! (`k:`), then the one providing it at the higher version, as apk picks
//! (checked against apk for real packages). `!conflicts` are not requirements. Packages that
//! apk installs by themselves (`i:`, install-if) are added once everything
//! they wait for is mirrored, so apk never asks for one that is missing.
use crate::index::{bare, Entry};
use crate::version;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, VecDeque};

/// The packages to mirror, as indexes into `pool`, and what could not be met.
#[derive(Debug)]
pub struct Selection {
    pub selected: BTreeSet<usize>,
    pub warnings: Vec<String>,
}

/// Packages of several repositories for one architecture: (repository number, entry).
pub struct Pool<'a> {
    pkgs: &'a [(usize, Entry)],
    /// Name -> the newest package of that name.
    by_name: HashMap<&'a str, usize>,
    /// Capability or name -> packages providing it (newest of each name only).
    provides: HashMap<&'a str, Vec<usize>>,
}

impl<'a> Pool<'a> {
    pub fn new(pkgs: &'a [(usize, Entry)]) -> Pool<'a> {
        let mut by_name: HashMap<&str, usize> = HashMap::new();
        for (i, (repo, e)) in pkgs.iter().enumerate() {
            match by_name.get(e.name.as_str()) {
                // Newer wins; at the same version, the repository listed first.
                Some(&j) if newer(&pkgs[j], (*repo, e)) != Ordering::Less => {}
                _ => {
                    by_name.insert(&e.name, i);
                }
            }
        }
        let mut provides: HashMap<&str, Vec<usize>> = HashMap::new();
        for &i in by_name.values() {
            let e = &pkgs[i].1;
            provides.entry(&e.name).or_default().push(i);
            for p in &e.provides {
                provides.entry(bare(p)).or_default().push(i);
            }
        }
        for v in provides.values_mut() {
            v.sort_unstable();
            v.dedup();
        }
        Pool { pkgs, by_name, provides }
    }

    pub fn named(&self, name: &str) -> Option<usize> {
        self.by_name.get(name).copied()
    }

    /// The package apk would pick for `cap`.
    fn best(&self, cap: &str) -> Option<usize> {
        if let Some(i) = self.named(cap) {
            return Some(i);
        }
        let provs = self.provides.get(cap)?;
        // The version a package provides `cap` at (`so:libisl.so.23=23.3.0`).
        let offered = |e: &'a Entry| e.provides.iter().find(|p| bare(p) == cap).and_then(|p| p.split_once('=')).map(|(_, v)| v);
        provs.iter().copied().max_by(|&a, &b| {
            let (ea, eb) = (&self.pkgs[a].1, &self.pkgs[b].1);
            // Priority first, then the higher version of what is provided, as apk picks.
            ea.priority
                .cmp(&eb.priority)
                .then_with(|| match (offered(ea), offered(eb)) {
                    (Some(x), Some(y)) => version::compare(x, y),
                    (Some(_), None) => Ordering::Greater,
                    (None, Some(_)) => Ordering::Less,
                    (None, None) => Ordering::Equal,
                })
                .then(eb.name.len().cmp(&ea.name.len()))
                .then(eb.name.cmp(&ea.name))
        })
    }
}

/// Order two packages of the same name: version first, then earlier repository.
fn newer(a: &(usize, Entry), b: (usize, &Entry)) -> Ordering {
    version::compare(&a.1.version, &b.1.version).then(b.0.cmp(&a.0))
}

/// Everything `wanted` needs, in `pool`. Wanted names that no repository
/// has are returned as an error.
pub fn resolve(pool: &Pool, wanted: &[String]) -> Result<Selection, String> {
    let mut sel = Selection { selected: BTreeSet::new(), warnings: Vec::new() };
    let mut queue = VecDeque::new();
    let mut missing = Vec::new();
    for w in wanted {
        match pool.named(bare(w)) {
            Some(i) => {
                if sel.selected.insert(i) {
                    queue.push_back(i);
                }
            }
            None => missing.push(w.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(format!("no repository has {}", missing.join(", ")));
    }
    // Is `cap` met by what is selected?
    let met = |sel: &BTreeSet<usize>, cap: &str| pool.provides.get(cap).is_some_and(|v| v.iter().any(|i| sel.contains(i)));
    loop {
        while let Some(i) = queue.pop_front() {
            let e = &pool.pkgs[i].1;
            for d in &e.depends {
                if d.starts_with('!') {
                    continue;
                }
                let cap = bare(d);
                if met(&sel.selected, cap) {
                    continue;
                }
                match pool.best(cap) {
                    Some(j) => {
                        if sel.selected.insert(j) {
                            queue.push_back(j);
                        }
                    }
                    None => sel.warnings.push(format!("{}-{} needs {cap}, which none of the repositories provides", e.name, e.version)),
                }
            }
        }
        // Install-if: what apk would add by itself, now that more is selected.
        let mut added = false;
        for &i in pool.by_name.values() {
            let e = &pool.pkgs[i].1;
            if e.install_if.is_empty() || sel.selected.contains(&i) {
                continue;
            }
            let waits_for = e.install_if.iter().filter(|w| !w.starts_with('!'));
            if waits_for.clone().count() > 0 && waits_for.clone().all(|w| met(&sel.selected, bare(w))) {
                sel.selected.insert(i);
                queue.push_back(i);
                added = true;
            }
        }
        if !added && queue.is_empty() {
            break;
        }
    }
    sel.warnings.sort();
    sel.warnings.dedup();
    Ok(sel)
}
