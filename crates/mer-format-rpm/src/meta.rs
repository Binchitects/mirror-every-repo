//! Reading `repomd.xml`, `primary.xml` and `filelists.xml`. Each package
//! element is kept as the exact bytes upstream wrote, so the subset
//! repository repeats upstream's metadata rather than re-creating it.
use crate::version::Evr;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

/// One `<data>` entry of `repomd.xml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DataRef {
    pub kind: String,
    pub href: String,
    /// (algorithm, hex) of the file as stored.
    pub checksum: (String, String),
    pub open_checksum: Option<(String, String)>,
    pub open_size: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct Repomd {
    pub revision: Option<u64>,
    pub data: Vec<DataRef>,
}

fn attr(e: &BytesStart, name: &[u8]) -> Option<String> {
    e.attributes().flatten().find(|a| a.key.as_ref() == name).and_then(|a| a.unescape_value().ok()).map(|v| v.into_owned())
}

pub fn parse_repomd(text: &str) -> Result<Repomd, String> {
    let mut r = Reader::from_str(text);
    let mut out = Repomd::default();
    let mut cur: Option<DataRef> = None;
    let mut field: Vec<u8> = Vec::new();
    let mut algo = String::new();
    loop {
        match r.read_event().map_err(|e| format!("repomd.xml: {e}"))? {
            Event::Start(e) | Event::Empty(e) => {
                let name = e.local_name().as_ref().to_vec();
                match name.as_slice() {
                    b"data" => cur = Some(DataRef { kind: attr(&e, b"type").unwrap_or_default(), ..Default::default() }),
                    b"location" => {
                        if let Some(d) = cur.as_mut() {
                            d.href = attr(&e, b"href").unwrap_or_default();
                        }
                    }
                    _ => {}
                }
                algo = attr(&e, b"type").unwrap_or_default();
                field = name;
            }
            Event::Text(t) => {
                let v = t.unescape().map_err(|e| format!("repomd.xml: {e}"))?.trim().to_string();
                match (field.as_slice(), cur.as_mut()) {
                    (b"revision", None) => out.revision = v.parse().ok(),
                    (b"checksum", Some(d)) => d.checksum = (algo.clone(), v),
                    (b"open-checksum", Some(d)) => d.open_checksum = Some((algo.clone(), v)),
                    (b"open-size", Some(d)) => d.open_size = v.parse().ok(),
                    _ => {}
                }
            }
            Event::End(e) => {
                if e.local_name().as_ref() == b"data" {
                    if let Some(d) = cur.take() {
                        out.data.push(d);
                    }
                }
                field.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// One package of `primary.xml`.
#[derive(Clone, Debug)]
pub struct Package {
    pub name: String,
    pub arch: String,
    pub evr: Evr,
    /// (algorithm, hex), and whether it is the package id.
    pub checksum: (String, String),
    pub href: String,
    pub size: u64,
    pub provides: Vec<String>,
    pub requires: Vec<String>,
    pub files: Vec<String>,
    /// The `<package>` element exactly as upstream wrote it.
    pub raw: String,
}

#[derive(PartialEq, Eq)]
enum Section {
    None,
    Provides,
    Requires,
    Other,
}

/// All packages of a `primary.xml`. Elements other than `type="rpm"` packages are ignored.
pub fn parse_primary(text: &str) -> Result<Vec<Package>, String> {
    let mut r = Reader::from_str(text);
    let mut out = Vec::new();
    let mut cur: Option<Package> = None;
    let mut start = 0usize;
    let mut field: Vec<u8> = Vec::new();
    let mut section = Section::None;
    loop {
        let before = r.buffer_position() as usize;
        let ev = r.read_event().map_err(|e| format!("primary.xml: {e}"))?;
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(ev, Event::Empty(_));
                let name = e.name().as_ref().to_vec();
                match name.as_slice() {
                    b"package" if attr(e, b"type").as_deref() == Some("rpm") => {
                        start = before;
                        cur = Some(Package {
                            name: String::new(),
                            arch: String::new(),
                            evr: Evr::new("0", "", ""),
                            checksum: (String::new(), String::new()),
                            href: String::new(),
                            size: 0,
                            provides: vec![],
                            requires: vec![],
                            files: vec![],
                            raw: String::new(),
                        });
                    }
                    _ if cur.is_none() => {}
                    b"version" => {
                        let p = cur.as_mut().expect("in a package");
                        p.evr = Evr::new(&attr(e, b"epoch").unwrap_or_default(), &attr(e, b"ver").unwrap_or_default(), &attr(e, b"rel").unwrap_or_default());
                    }
                    b"checksum" => cur.as_mut().expect("in a package").checksum.0 = attr(e, b"type").unwrap_or_default(),
                    b"location" => {
                        let p = cur.as_mut().expect("in a package");
                        p.href = attr(e, b"href").unwrap_or_default();
                        if attr(e, b"xml:base").is_some() {
                            return Err(format!("primary.xml: package {} names another server (xml:base), which is not supported", p.name));
                        }
                    }
                    b"size" => cur.as_mut().expect("in a package").size = attr(e, b"package").and_then(|s| s.parse().ok()).unwrap_or(0),
                    b"rpm:provides" if !empty => section = Section::Provides,
                    b"rpm:requires" if !empty => section = Section::Requires,
                    b"rpm:conflicts" | b"rpm:obsoletes" | b"rpm:recommends" | b"rpm:suggests" | b"rpm:supplements" | b"rpm:enhances" if !empty => section = Section::Other,
                    b"rpm:entry" => {
                        let p = cur.as_mut().expect("in a package");
                        let n = attr(e, b"name").unwrap_or_default();
                        match section {
                            Section::Provides => p.provides.push(n),
                            Section::Requires => p.requires.push(n),
                            _ => {}
                        }
                    }
                    _ => {}
                }
                field = name;
            }
            Event::Text(t) if cur.is_some() => {
                let v = t.unescape().map_err(|e| format!("primary.xml: {e}"))?;
                let p = cur.as_mut().expect("in a package");
                match field.as_slice() {
                    b"name" => p.name = v.trim().to_string(),
                    b"arch" => p.arch = v.trim().to_string(),
                    b"checksum" => p.checksum.1 = v.trim().to_string(),
                    b"file" => p.files.push(v.trim().to_string()),
                    _ => {}
                }
            }
            Event::End(e) => {
                match e.name().as_ref() {
                    b"package" => {
                        if let Some(mut p) = cur.take() {
                            p.raw = text[start..r.buffer_position() as usize].trim().to_string();
                            out.push(p);
                        }
                    }
                    b"rpm:provides" | b"rpm:requires" | b"rpm:conflicts" | b"rpm:obsoletes" | b"rpm:recommends" | b"rpm:suggests" | b"rpm:supplements" | b"rpm:enhances" => {
                        section = Section::None
                    }
                    _ => {}
                }
                field.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// `filelists.xml`: for each package id, the byte range of its element and its files.
pub fn parse_filelists(text: &str, mut each: impl FnMut(&str, (usize, usize), Vec<String>)) -> Result<(), String> {
    let mut r = Reader::from_str(text);
    let mut cur: Option<(String, usize, Vec<String>)> = None;
    let mut in_file = false;
    loop {
        let before = r.buffer_position() as usize;
        match r.read_event().map_err(|e| format!("filelists.xml: {e}"))? {
            Event::Start(e) if e.local_name().as_ref() == b"package" => cur = Some((attr(&e, b"pkgid").unwrap_or_default(), before, vec![])),
            Event::Start(e) if e.local_name().as_ref() == b"file" => in_file = cur.is_some(),
            Event::Text(t) if in_file => {
                if let Some(c) = cur.as_mut() {
                    c.2.push(t.unescape().map_err(|e| format!("filelists.xml: {e}"))?.trim().to_string());
                }
            }
            Event::End(e) if e.local_name().as_ref() == b"file" => in_file = false,
            Event::End(e) if e.local_name().as_ref() == b"package" => {
                if let Some((id, s, files)) = cur.take() {
                    each(&id, (s, r.buffer_position() as usize), files);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(())
}

/// A rich (boolean) dependency: `(a or (b if c))`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rich {
    Cap(String),
    And(Vec<Rich>),
    Or(Vec<Rich>),
    /// then, condition, else
    If(Box<Rich>, Box<Rich>, Option<Box<Rich>>),
    Unless(Box<Rich>, Box<Rich>, Option<Box<Rich>>),
}

/// Split a rich dependency into parentheses and words. A parenthesis inside
/// a word belongs to it: `libc.so.6(GLIBC_2.34)(64bit)`, `perl(Foo::Bar)`.
fn rich_tokens(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < b.len() {
        match b[i] {
            c if c.is_ascii_whitespace() => i += 1,
            b'(' | b')' => {
                out.push(&s[i..i + 1]);
                i += 1;
            }
            _ => {
                let (start, mut depth) = (i, 0usize);
                while i < b.len() {
                    match b[i] {
                        b'(' => depth += 1,
                        b')' if depth == 0 => break,
                        b')' => depth -= 1,
                        c if c.is_ascii_whitespace() && depth == 0 => break,
                        _ => {}
                    }
                    i += 1;
                }
                out.push(&s[start..i]);
            }
        }
    }
    out
}

pub fn parse_rich(s: &str) -> Option<Rich> {
    let toks = rich_tokens(s);
    let mut i = 0;
    let r = rich_expr(&toks, &mut i)?;
    (i == toks.len()).then_some(r)
}

fn rich_expr(t: &[&str], i: &mut usize) -> Option<Rich> {
    if t.get(*i) != Some(&"(") {
        return None;
    }
    *i += 1;
    let mut operands = vec![rich_operand(t, i)?];
    let mut ops: Vec<&str> = Vec::new();
    while let Some(&tok) = t.get(*i) {
        if tok == ")" {
            *i += 1;
            break;
        }
        if !matches!(tok, "and" | "or" | "if" | "else" | "unless" | "with" | "without") {
            return None;
        }
        *i += 1;
        ops.push(tok);
        operands.push(rich_operand(t, i)?);
    }
    let first = ops.first().copied();
    Some(match first {
        None => operands.remove(0),
        Some("and") | Some("with") => Rich::And(operands),
        Some("without") => operands.remove(0),
        Some("or") => Rich::Or(operands),
        Some(kind @ ("if" | "unless")) => {
            let mut it = operands.into_iter();
            let then = Box::new(it.next()?);
            let cond = Box::new(it.next()?);
            let other = it.next().map(Box::new);
            if kind == "if" { Rich::If(then, cond, other) } else { Rich::Unless(then, cond, other) }
        }
        _ => return None,
    })
}

fn rich_operand(t: &[&str], i: &mut usize) -> Option<Rich> {
    if t.get(*i) == Some(&"(") {
        return rich_expr(t, i);
    }
    let name = t.get(*i)?.to_string();
    *i += 1;
    // A version comparison after a name: `foo >= 1.2`.
    if matches!(t.get(*i), Some(&("<" | "<=" | "=" | ">=" | ">"))) {
        *i += 2;
    }
    Some(Rich::Cap(name))
}
