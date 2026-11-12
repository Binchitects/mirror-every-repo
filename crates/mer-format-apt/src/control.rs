//! Debian control-file syntax: stanzas of `Field: value` lines separated by
//! blank lines, with continuation lines starting with a space or tab.

/// One stanza. `raw` is the original text (without the blank separator), kept
/// so a package entry can be copied into our own index byte for byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stanza {
    pub raw: String,
    fields: Vec<(String, String)>,
}

impl Stanza {
    /// Field value with continuation lines joined by `\n`. Names are case-insensitive.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Split text into stanzas. Malformed lines are an error: an index that does
/// not parse cleanly is not used at all.
pub fn parse_stanzas(text: &str) -> Result<Vec<Stanza>, String> {
    let mut out = Vec::new();
    let mut raw = String::new();
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut flush = |raw: &mut String, fields: &mut Vec<(String, String)>| {
        if !fields.is_empty() {
            out.push(Stanza { raw: std::mem::take(raw), fields: std::mem::take(fields) });
        }
        raw.clear();
    };
    for (n, line) in text.lines().enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() {
            flush(&mut raw, &mut fields);
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            let Some((_, value)) = fields.last_mut() else {
                return Err(format!("line {}: continuation without a field", n + 1));
            };
            value.push('\n');
            value.push_str(line.trim_start());
        } else {
            let Some((name, value)) = line.split_once(':') else {
                return Err(format!("line {}: not a field", n + 1));
            };
            if name.is_empty() || name.contains(char::is_whitespace) {
                return Err(format!("line {}: bad field name", n + 1));
            }
            fields.push((name.to_string(), value.trim().to_string()));
        }
        raw.push_str(line);
        raw.push('\n');
    }
    flush(&mut raw, &mut fields);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "Package: hello\nVersion: 2.10-3\nDepends: libc6 (>= 2.34)\nDescription: example package\n the classic greeting\n .\n second paragraph\n\n\nPackage: libc6\nversion: 2.36-9\n";

    #[test]
    fn stanzas_fields_and_continuations() {
        let s = parse_stanzas(TEXT).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].get("Package"), Some("hello"));
        assert_eq!(s[0].get("depends"), Some("libc6 (>= 2.34)"));
        assert_eq!(s[0].get("Description"), Some("example package\nthe classic greeting\n.\nsecond paragraph"));
        assert_eq!(s[1].get("Version"), Some("2.36-9"), "field names are case-insensitive");
        assert_eq!(s[0].get("Missing"), None);
    }

    #[test]
    fn raw_text_is_kept_exactly() {
        let s = parse_stanzas(TEXT).unwrap();
        assert_eq!(format!("{}\n{}", s[0].raw, s[1].raw), TEXT.replace("\n\n\n", "\n\n"));
        // Re-parsing our own output gives the same stanzas.
        assert_eq!(parse_stanzas(&format!("{}\n{}", s[0].raw, s[1].raw)).unwrap(), s);
    }

    #[test]
    fn crlf_comments_and_empty_input() {
        let s = parse_stanzas("# comment\r\nPackage: a\r\nVersion: 1\r\n\r\n").unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].get("Version"), Some("1"));
        assert!(parse_stanzas("").unwrap().is_empty());
        assert!(parse_stanzas("\n\n  \n").unwrap().is_empty());
    }

    #[test]
    fn malformed_input_is_rejected() {
        assert!(parse_stanzas("no colon here\n").is_err());
        assert!(parse_stanzas(" leading continuation\n").is_err());
        assert!(parse_stanzas(": empty name\n").is_err());
        assert!(parse_stanzas("bad name: x\n").is_err());
    }
}
