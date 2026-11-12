//! Dependency specifications (PEP 508) and environment markers.
use crate::version::{Specifier, SpecifierSet, Version};
use std::collections::BTreeMap;

/// PEP 503: lower-case, runs of `-`, `_`, `.` become one `-`.
pub fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut sep = false;
    for c in name.trim().chars() {
        if matches!(c, '-' | '_' | '.') {
            sep = true;
        } else {
            if sep && !out.is_empty() {
                out.push('-');
            }
            sep = false;
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requirement {
    /// Normalised project name.
    pub name: String,
    pub extras: Vec<String>,
    pub spec: SpecifierSet,
    pub marker: Option<String>,
    /// `name @ https://...`: a direct reference, which a mirror cannot follow.
    pub direct_url: bool,
}

/// `name[extra1,extra2] (>=1.0,<2) ; python_version < "3.9"`
pub fn parse_requirement(text: &str) -> Result<Requirement, String> {
    let bad = |why: &str| format!("requirement {text:?}: {why}");
    let (head, marker) = match text.split_once(';') {
        Some((h, m)) => (h.trim(), Some(m.trim().to_string()).filter(|m| !m.is_empty())),
        None => (text.trim(), None),
    };
    let end = head.find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))).unwrap_or(head.len());
    let (raw_name, mut rest) = (&head[..end], head[end..].trim());
    if !valid_name(raw_name) {
        return Err(bad("bad project name"));
    }
    let mut extras = Vec::new();
    if let Some(after) = rest.strip_prefix('[') {
        let (inside, tail) = after.split_once(']').ok_or_else(|| bad("unclosed extras"))?;
        for e in inside.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            if !valid_name(e) {
                return Err(bad("bad extra name"));
            }
            extras.push(normalize_name(e));
        }
        rest = tail.trim();
    }
    let name = normalize_name(raw_name);
    if rest.starts_with('@') {
        return Ok(Requirement { name, extras, spec: SpecifierSet::default(), marker, direct_url: true });
    }
    let inner = match rest.strip_prefix('(') {
        Some(r) => r.strip_suffix(')').ok_or_else(|| bad("unclosed parenthesis"))?,
        None => rest,
    };
    let spec = SpecifierSet::parse(inner).ok_or_else(|| bad("bad version specifier"))?;
    Ok(Requirement { name, extras, spec, marker, direct_url: false })
}

/// The environment a marker is evaluated against.
#[derive(Clone, Debug)]
pub struct Env(pub BTreeMap<&'static str, String>);

impl Env {
    /// CPython on Linux. `full` is the three-part version, for example `3.11.4`.
    pub fn cpython_linux(full: &str, machine: &str) -> Env {
        let short: String = full.split('.').take(2).collect::<Vec<_>>().join(".");
        Env(BTreeMap::from([
            ("os_name", "posix".to_string()),
            ("sys_platform", "linux".to_string()),
            ("platform_system", "Linux".to_string()),
            ("platform_machine", machine.to_string()),
            ("platform_python_implementation", "CPython".to_string()),
            ("implementation_name", "cpython".to_string()),
            ("platform_release", String::new()),
            ("platform_version", String::new()),
            ("python_version", short),
            ("python_full_version", full.to_string()),
            ("implementation_version", full.to_string()),
            ("extra", String::new()),
        ]))
    }

    pub fn with_extra(mut self, extra: &str) -> Env {
        self.0.insert("extra", normalize_name(extra));
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Str(String),
    Var(String),
    Op(String),
    And,
    Or,
    Open,
    Close,
}

fn tokenize(s: &str) -> Result<Vec<Tok>, String> {
    let b: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '(' {
            out.push(Tok::Open);
            i += 1;
        } else if c == ')' {
            out.push(Tok::Close);
            i += 1;
        } else if c == '"' || c == '\'' {
            let end = b[i + 1..].iter().position(|x| *x == c).ok_or("unterminated string in marker")?;
            out.push(Tok::Str(b[i + 1..i + 1 + end].iter().collect()));
            i += end + 2;
        } else if "<>=!~".contains(c) {
            let start = i;
            while i < b.len() && "<>=!~".contains(b[i]) {
                i += 1;
            }
            out.push(Tok::Op(b[start..i].iter().collect()));
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '_' || b[i] == '.') {
                i += 1;
            }
            let word: String = b[start..i].iter().collect();
            match word.as_str() {
                "and" => out.push(Tok::And),
                "or" => out.push(Tok::Or),
                "in" => out.push(Tok::Op("in".into())),
                "not" => out.push(Tok::Op("not".into())),
                _ => out.push(Tok::Var(word)),
            }
        } else {
            return Err(format!("unexpected character {c:?} in marker"));
        }
    }
    // "not" "in" -> one operator.
    let mut merged = Vec::with_capacity(out.len());
    let mut it = out.into_iter().peekable();
    while let Some(t) = it.next() {
        if t == Tok::Op("not".into()) {
            if it.next() != Some(Tok::Op("in".into())) {
                return Err("'not' must be followed by 'in' in a marker".into());
            }
            merged.push(Tok::Op("not in".into()));
        } else {
            merged.push(t);
        }
    }
    Ok(merged)
}

struct Parser<'a> {
    toks: &'a [Tok],
    i: usize,
    env: &'a Env,
}

impl Parser<'_> {
    fn value(&mut self) -> Result<(String, bool), String> {
        let t = self.toks.get(self.i).ok_or("marker ends too early")?;
        self.i += 1;
        match t {
            Tok::Str(s) => Ok((s.clone(), false)),
            Tok::Var(name) => {
                // Old spellings from PEP 345.
                let key = match name.as_str() {
                    "os.name" => "os_name",
                    "sys.platform" => "sys_platform",
                    "platform.version" => "platform_version",
                    "platform.machine" => "platform_machine",
                    "platform.python_implementation" | "python_implementation" => "platform_python_implementation",
                    other => other,
                };
                self.env.0.get(key).map(|v| (v.clone(), key == "extra")).ok_or_else(|| format!("unknown marker variable {name:?}"))
            }
            _ => Err("expected a value in marker".into()),
        }
    }

    fn atom(&mut self) -> Result<bool, String> {
        if self.toks.get(self.i) == Some(&Tok::Open) {
            self.i += 1;
            let v = self.or()?;
            if self.toks.get(self.i) != Some(&Tok::Close) {
                return Err("missing ')' in marker".into());
            }
            self.i += 1;
            return Ok(v);
        }
        let (lhs, lhs_extra) = self.value()?;
        let Some(Tok::Op(op)) = self.toks.get(self.i).cloned() else { return Err("expected an operator in marker".into()) };
        self.i += 1;
        let (rhs, rhs_extra) = self.value()?;
        // Extras are compared as normalised names.
        let (lhs, rhs) = if lhs_extra || rhs_extra { (normalize_name(&lhs), normalize_name(&rhs)) } else { (lhs, rhs) };
        compare(&lhs, &op, &rhs)
    }

    fn and(&mut self) -> Result<bool, String> {
        let mut v = self.atom()?;
        while self.toks.get(self.i) == Some(&Tok::And) {
            self.i += 1;
            let r = self.atom()?;
            v = v && r;
        }
        Ok(v)
    }

    fn or(&mut self) -> Result<bool, String> {
        let mut v = self.and()?;
        while self.toks.get(self.i) == Some(&Tok::Or) {
            self.i += 1;
            let r = self.and()?;
            v = v || r;
        }
        Ok(v)
    }
}

fn compare(lhs: &str, op: &str, rhs: &str) -> Result<bool, String> {
    match op {
        "in" => return Ok(rhs.contains(lhs)),
        "not in" => return Ok(!rhs.contains(lhs)),
        _ => {}
    }
    // Version comparison when both sides are versions, otherwise plain strings.
    if let (Some(set), Some(v)) = (SpecifierSet::parse(&format!("{op}{rhs}")), Version::parse(lhs)) {
        let _: &Vec<Specifier> = &set.0;
        return Ok(set.matches(&v));
    }
    Ok(match op {
        "==" | "===" => lhs == rhs,
        "!=" => lhs != rhs,
        "<" => lhs < rhs,
        "<=" => lhs <= rhs,
        ">" => lhs > rhs,
        ">=" => lhs >= rhs,
        other => return Err(format!("operator {other:?} cannot compare {lhs:?} with {rhs:?}")),
    })
}

/// Is the marker true in `env`?
pub fn evaluate(marker: &str, env: &Env) -> Result<bool, String> {
    let toks = tokenize(marker)?;
    let mut p = Parser { toks: &toks, i: 0, env };
    let v = p.or()?;
    if p.i != toks.len() {
        return Err(format!("unexpected text at the end of marker {marker:?}"));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for (a, b) in [("Django", "django"), ("zope.interface", "zope-interface"), ("A__b--C..d", "a-b-c-d"), ("ruamel.yaml", "ruamel-yaml")] {
            assert_eq!(normalize_name(a), b);
        }
        for bad in ["", "-x", "x-", "a b", "a/b", "../x", "a;b"] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn requirements() {
        let r = parse_requirement("Requests[Security, socks] (>=2.8.1, ==2.8.*) ; python_version < '2.7'").unwrap();
        assert_eq!(r.name, "requests");
        assert_eq!(r.extras, ["security", "socks"]);
        assert_eq!(r.spec.to_string(), ">=2.8.1,==2.8.*");
        assert_eq!(r.marker.as_deref(), Some("python_version < '2.7'"));

        let r = parse_requirement("urllib3<3,>=1.21.1").unwrap();
        assert_eq!((r.name.as_str(), r.spec.to_string().as_str(), r.marker), ("urllib3", "<3,>=1.21.1", None));
        assert_eq!(parse_requirement("idna").unwrap().spec, SpecifierSet::default());
        assert!(parse_requirement("pip @ https://example.com/pip-1.0.whl").unwrap().direct_url);
        for bad in ["", ">=1.0", "a b c", "name[extra", "name (>=1", "name >=", "name ==x.y"] {
            assert!(parse_requirement(bad).is_err(), "{bad:?}");
        }
    }

    fn eval(marker: &str) -> bool {
        evaluate(marker, &Env::cpython_linux("3.11.4", "x86_64")).unwrap_or_else(|e| panic!("{marker}: {e}"))
    }

    #[test]
    fn markers() {
        let yes = [
            "python_version >= '3.8'", "python_version == \"3.11\"", "python_full_version >= '3.11.0'",
            "sys_platform == 'linux'", "os_name == 'posix' and platform_system != 'Windows'",
            "python_version < '3' or sys_platform == 'linux'", "(python_version < '3' or os_name == 'posix') and platform_machine == 'x86_64'",
            "implementation_name == 'cpython'", "platform_python_implementation != 'PyPy'", "'linux' in sys_platform",
            "sys_platform not in 'win32 cygwin'", "python_version >= '3.10' and python_version < '4'", "python_version ~= '3.11'",
            // 3.10 is newer than 3.9 as a version, although "3.10" < "3.9" as text.
            "python_version > '3.9'", "sys.platform == 'linux'",
        ];
        let no = [
            "python_version < '3.8'", "sys_platform == 'win32'", "platform_system == 'Darwin'", "extra == 'test'",
            "os_name == 'nt' and python_version >= '3'", "python_version < '3.11' or sys_platform == 'darwin'",
            "platform_machine == 'aarch64'", "implementation_name == 'pypy'", "python_full_version < '3.11.4'",
            "'win' in sys_platform", "extra == 'socks' or extra == 'security'",
        ];
        for m in yes {
            assert!(eval(m), "{m} should be true");
        }
        for m in no {
            assert!(!eval(m), "{m} should be false");
        }
    }

    #[test]
    fn extras_and_errors() {
        let env = Env::cpython_linux("3.11.4", "x86_64").with_extra("Dev_Tools");
        assert!(evaluate("extra == 'dev-tools'", &env).unwrap());
        assert!(evaluate("extra == 'DEV.TOOLS' and python_version > '3'", &env).unwrap());
        for bad in ["python_version", "python_version >= ", "bogus_var == 'x'", "python_version >= '3' and", "(os_name == 'posix'", "os_name == 'posix')", "os_name not 'posix'", "os_name == 'unterminated", "os_name $ 'x'"] {
            assert!(evaluate(bad, &env).is_err(), "{bad:?}");
        }
    }
}
