//! Reading go.mod files: the module path, the Go version and the requirements.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoMod {
    pub module: String,
    /// The `go` directive as (major, minor).
    pub go: Option<(u64, u64)>,
    /// (module, version) of every `require`, direct or indirect.
    pub requires: Vec<(String, String)>,
}

impl GoMod {
    /// From Go 1.17 a go.mod lists everything needed to build the module,
    /// so the go command does not read further than its requirements.
    pub fn lists_all_dependencies(&self) -> bool {
        self.go.is_some_and(|v| v >= (1, 17))
    }
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"').and_then(|x| x.strip_suffix('"')).or_else(|| s.strip_prefix('`').and_then(|x| x.strip_suffix('`'))).unwrap_or(s)
}

/// `replace` and `exclude` are not read: Go applies them only in the main
/// module, never in a dependency.
pub fn parse_gomod(text: &str) -> GoMod {
    let mut out = GoMod::default();
    let mut block: Option<String> = None;
    for line in text.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if block.is_some() {
            if line == ")" {
                block = None;
                continue;
            }
        } else if let Some(name) = line.strip_suffix('(').map(str::trim) {
            block = Some(name.to_string());
            continue;
        }
        let mut words = line.split_whitespace();
        let directive = match &block {
            Some(b) => b.as_str(),
            None => words.next().unwrap_or(""),
        };
        let args: Vec<&str> = words.map(unquote).collect();
        match (directive, args.as_slice()) {
            ("module", [path, ..]) => out.module = (*path).to_string(),
            ("go", [version, ..]) => {
                let mut parts = version.split('.').map(|p| p.parse::<u64>().ok());
                if let (Some(Some(major)), Some(Some(minor))) = (parts.next(), parts.next()) {
                    out.go = Some((major, minor));
                }
            }
            ("require", [path, version, ..]) => out.requires.push(((*path).to_string(), (*version).to_string())),
            _ => {}
        }
    }
    out
}
