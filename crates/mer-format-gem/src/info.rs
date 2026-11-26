//! The compact index (`/info/<gem>`) and `Gemfile.lock`.
//!
//! An info line is `version[-platform] dep:req&req,dep:req|key:value,...`,
//! where the keys are `checksum` (SHA-256 of the `.gem`), `ruby` and
//! `rubygems` (requirements on the installing Ruby), and `created_at`.
use crate::version::{Requirement, Version};

/// A gem or platform name that is safe in a file name.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 200 && !n.starts_with(['.', '-']) && n.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[derive(Clone, Debug)]
pub struct Line {
    /// The line as upstream wrote it; the vault serves it unchanged.
    pub raw: String,
    pub version: Version,
    /// `ruby`, or a platform such as `x86_64-linux-gnu`.
    pub platform: String,
    pub deps: Vec<(String, String)>,
    pub checksum: Option<[u8; 32]>,
    /// What Ruby the gem needs (`>= 3.2&< 4.1.dev`).
    pub ruby: Option<String>,
    pub created_at: Option<String>,
}

impl Line {
    /// `version` or `version-platform`, as in file names and `/versions`.
    pub fn full_version(&self) -> String {
        if self.platform == "ruby" {
            self.version.as_str().to_string()
        } else {
            format!("{}-{}", self.version.as_str(), self.platform)
        }
    }
}

/// Parse an info file. Lines that cannot be read are counted, not kept.
pub fn parse_info(text: &str) -> (Vec<Line>, usize) {
    let (mut out, mut bad) = (Vec::new(), 0);
    for raw in text.lines() {
        if raw.is_empty() || raw == "---" {
            continue;
        }
        match parse_line(raw) {
            Some(l) => out.push(l),
            None => bad += 1,
        }
    }
    (out, bad)
}

fn parse_line(raw: &str) -> Option<Line> {
    let (head, meta) = raw.split_once('|').unwrap_or((raw, ""));
    let (vp, deps_text) = head.split_once(' ').unwrap_or((head, ""));
    // Versions in the index are normalised (no hyphens), so the first
    // hyphen starts the platform.
    let (v, platform) = match vp.split_once('-') {
        Some((v, p)) if valid_name(p) => (v, p.to_string()),
        Some(_) => return None,
        None => (vp, "ruby".to_string()),
    };
    if !v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.') {
        return None;
    }
    let version = Version::parse(v)?;
    let mut deps = Vec::new();
    for d in deps_text.split(',').map(str::trim).filter(|d| !d.is_empty()) {
        let (name, req) = d.split_once(':')?;
        if !valid_name(name) {
            return None;
        }
        deps.push((name.to_string(), req.to_string()));
    }
    let mut line = Line { raw: raw.to_string(), version, platform, deps, checksum: None, ruby: None, created_at: None };
    for m in meta.split(',') {
        let Some((k, v)) = m.split_once(':') else { continue };
        match k {
            "checksum" => {
                let mut sum = [0u8; 32];
                hex::decode_to_slice(v, &mut sum).ok()?;
                line.checksum = Some(sum);
            }
            "ruby" => line.ruby = Some(v.to_string()),
            "created_at" => line.created_at = Some(v.to_string()),
            _ => {}
        }
    }
    Some(line)
}

/// Whether a line suits the Ruby that will install it.
pub fn suits_ruby(line: &Line, ruby: Option<&Version>) -> bool {
    match (ruby, &line.ruby) {
        (Some(v), Some(req)) => Requirement::parse(req).is_none_or(|r| r.matches(v)),
        _ => true,
    }
}

/// The gems a `Gemfile.lock` names, from its `GEM` sections: (name, version, platform).
pub fn parse_lockfile(text: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut in_specs = false;
    let mut in_gem = false;
    for line in text.lines() {
        if !line.starts_with(' ') {
            in_gem = line == "GEM";
            in_specs = false;
            continue;
        }
        if in_gem && line.trim() == "specs:" {
            in_specs = true;
            continue;
        }
        // Gems are indented four spaces; their dependencies six.
        if in_specs && line.starts_with("    ") && !line.starts_with("     ") {
            let Some((name, rest)) = line.trim().split_once(" (") else { continue };
            let vp = rest.trim_end_matches(')');
            let (v, p) = vp.split_once('-').unwrap_or((vp, "ruby"));
            if valid_name(name) && Version::parse(v).is_some() && valid_name(p) {
                out.push((name.to_string(), v.to_string(), p.to_string()));
            }
        }
    }
    out
}
