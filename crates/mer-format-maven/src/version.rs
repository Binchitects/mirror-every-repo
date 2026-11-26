//! Maven versions, ordered as `org.apache.maven.artifact.versioning.ComparableVersion`
//! (Maven 3.9) orders them, and version ranges.
use std::cmp::Ordering;

const QUALIFIERS: [&str; 7] = ["alpha", "beta", "milestone", "rc", "snapshot", "", "sp"];

#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    /// Digits without leading zeros ("" is zero). Maven's int, long and
    /// BigInteger items order by magnitude, which this does.
    Num(String),
    Str(String),
    /// Letters followed by digits, `alpha1`.
    Comb(String, String),
    List(Vec<Item>),
}

fn qualifier(s: &str) -> String {
    match QUALIFIERS.iter().position(|q| *q == s) {
        Some(i) => i.to_string(),
        None => format!("{}-{s}", QUALIFIERS.len()),
    }
}

fn str_item(v: &str, followed_by_digit: bool) -> String {
    let v = if followed_by_digit && v.len() == 1 {
        match v {
            "a" => "alpha",
            "b" => "beta",
            "m" => "milestone",
            other => other,
        }
    } else {
        v
    };
    match v {
        "ga" | "final" | "release" => String::new(),
        "cr" => "rc".into(),
        other => other.to_string(),
    }
}

fn num(s: &str) -> Item {
    Item::Num(s.trim_start_matches('0').to_string())
}

fn cmp_num(a: &str, b: &str) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn parse_item(combination: bool, digit: bool, buf: &str) -> Item {
    if combination {
        let buf = buf.replace('-', "");
        let i = buf.find(|c: char| c.is_ascii_digit()).unwrap_or(buf.len());
        Item::Comb(str_item(&buf[..i], true), buf[i..].trim_start_matches('0').to_string())
    } else if digit {
        num(buf)
    } else {
        Item::Str(str_item(buf, false))
    }
}

impl Item {
    fn is_null(&self) -> bool {
        match self {
            Item::Num(n) => n.is_empty(),
            Item::Str(s) => qualifier(s) == "5",
            Item::Comb(..) => false,
            Item::List(l) => l.is_empty(),
        }
    }

    /// `other` = None means "nothing there".
    fn cmp_to(&self, other: Option<&Item>) -> Ordering {
        use Item::*;
        match (self, other) {
            (Num(n), None) => if n.is_empty() { Ordering::Equal } else { Ordering::Greater },
            (Num(a), Some(Num(b))) => cmp_num(a, b),
            (Num(_), Some(_)) => Ordering::Greater,

            (Str(s), None) => qualifier(s).cmp(&"5".to_string()),
            (Str(_), Some(Num(_))) | (Str(_), Some(List(_))) => Ordering::Less,
            (Str(a), Some(Str(b))) => qualifier(a).cmp(&qualifier(b)),
            (Str(a), Some(Comb(bs, _))) => match qualifier(a).cmp(&qualifier(bs)) {
                Ordering::Equal => Ordering::Less,
                o => o,
            },

            (Comb(s, _), None) => qualifier(s).cmp(&"5".to_string()),
            (Comb(..), Some(Num(_))) | (Comb(..), Some(List(_))) => Ordering::Less,
            (Comb(s, _), Some(Str(b))) => match qualifier(s).cmp(&qualifier(b)) {
                Ordering::Equal => Ordering::Greater,
                o => o,
            },
            (Comb(s, d), Some(Comb(bs, bd))) => qualifier(s).cmp(&qualifier(bs)).then_with(|| cmp_num(d, bd)),

            (List(l), None) => l.iter().map(|i| i.cmp_to(None)).find(|o| *o != Ordering::Equal).unwrap_or(Ordering::Equal),
            (List(_), Some(Num(_))) => Ordering::Less,
            (List(_), Some(Str(_))) | (List(_), Some(Comb(..))) => Ordering::Greater,
            (List(a), Some(List(b))) => {
                let n = a.len().max(b.len());
                for i in 0..n {
                    let o = match (a.get(i), b.get(i)) {
                        (None, None) => Ordering::Equal,
                        (None, Some(r)) => r.cmp_to(None).reverse(),
                        (Some(l), r) => l.cmp_to(r),
                    };
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                Ordering::Equal
            }
        }
    }
}

fn normalize(l: &mut Vec<Item>) {
    let mut i = l.len();
    while i > 0 {
        i -= 1;
        if l[i].is_null() {
            l.remove(i);
        } else if !matches!(l[i], Item::List(_)) {
            break;
        }
    }
}

/// A list under construction; `path` is the chain of nested lists.
struct Builder {
    root: Vec<Item>,
    depth: usize,
}

impl Builder {
    fn cur(&mut self) -> &mut Vec<Item> {
        let mut l = &mut self.root;
        for _ in 0..self.depth {
            match l.last_mut() {
                Some(Item::List(inner)) => l = inner,
                _ => unreachable!("a nested list is always last"),
            }
        }
        l
    }
    fn push(&mut self, i: Item) {
        self.cur().push(i);
    }
    fn open(&mut self) {
        self.cur().push(Item::List(Vec::new()));
        self.depth += 1;
    }
    fn is_empty(&mut self) -> bool {
        self.cur().is_empty()
    }
}

fn normalize_all(l: &mut Vec<Item>) {
    for i in l.iter_mut() {
        if let Item::List(inner) = i {
            normalize_all(inner);
        }
    }
    normalize(l);
}

fn parse(version: &str) -> Vec<Item> {
    let v = version.to_lowercase();
    let b = v.as_bytes();
    let mut list = Builder { root: Vec::new(), depth: 0 };
    let (mut digit, mut combination, mut start) = (false, false, 0usize);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'.' {
            if i == start {
                list.push(num("0"));
            } else {
                list.push(parse_item(combination, digit, &v[start..i]));
            }
            combination = false;
            start = i + 1;
        } else if c == b'-' {
            if i == start {
                list.push(num("0"));
            } else {
                // X-1 is X1.
                if !digit && i != b.len() - 1 && b[i + 1].is_ascii_digit() {
                    combination = true;
                    i += 1;
                    continue;
                }
                list.push(parse_item(combination, digit, &v[start..i]));
            }
            start = i + 1;
            list.open();
            combination = false;
        } else if c.is_ascii_digit() {
            if !digit && i > start {
                // X1
                combination = true;
                if !list.is_empty() {
                    list.open();
                }
            }
            digit = true;
        } else {
            if digit && i > start {
                list.push(parse_item(combination, true, &v[start..i]));
                start = i;
                list.open();
                combination = false;
            }
            digit = false;
        }
        i += 1;
    }
    if b.len() > start {
        // 1.0.0.X1 < 1.0.0-X2: a trailing string qualifier after "." counts as after "-".
        if !digit && !list.is_empty() {
            list.open();
        }
        list.push(parse_item(combination, digit, &v[start..]));
    }
    let mut root = list.root;
    normalize_all(&mut root);
    root
}

/// Compare two Maven versions.
pub fn compare(a: &str, b: &str) -> Ordering {
    Item::List(parse(a)).cmp_to(Some(&Item::List(parse(b))))
}

/// A version range: `[1.0,2.0)`, `[1.5]`, `(,1.0]`, unions `[1,2),[3,4)`.
/// One end of an interval: the version and whether it is included.
type Bound = Option<(String, bool)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Range {
    parts: Vec<(Bound, Bound)>,
}

impl Range {
    /// `None` for a plain version (a "soft" requirement, not a range).
    pub fn parse(spec: &str) -> Option<Result<Range, String>> {
        let s = spec.trim();
        if !s.starts_with('[') && !s.starts_with('(') {
            return None;
        }
        let mut parts = Vec::new();
        let mut rest = s;
        while !rest.is_empty() {
            let open = rest.chars().next()?;
            let close_at = match rest.find([']', ')']) {
                Some(i) => i,
                None => return Some(Err(format!("range {spec:?} is not closed"))),
            };
            let inner = &rest[1..close_at];
            let close = rest.as_bytes()[close_at];
            let (lo, hi) = match inner.split_once(',') {
                Some((lo, hi)) => (lo.trim(), hi.trim()),
                None => {
                    if open != '[' || close != b']' {
                        return Some(Err(format!("range {spec:?}: a single version needs [ ]")));
                    }
                    (inner.trim(), inner.trim())
                }
            };
            let bound = |v: &str, incl: bool| (!v.is_empty()).then(|| (v.to_string(), incl));
            parts.push((bound(lo, open == '['), bound(hi, close == b']')));
            rest = rest[close_at + 1..].trim_start().trim_start_matches(',').trim_start();
            if !rest.is_empty() && !rest.starts_with(['[', '(']) {
                return Some(Err(format!("range {spec:?} is not understood")));
            }
        }
        Some(Ok(Range { parts }))
    }

    pub fn contains(&self, v: &str) -> bool {
        self.parts.iter().any(|(lo, hi)| {
            lo.as_ref().is_none_or(|(l, incl)| match compare(v, l) {
                Ordering::Greater => true,
                Ordering::Equal => *incl,
                Ordering::Less => false,
            }) && hi.as_ref().is_none_or(|(h, incl)| match compare(v, h) {
                Ordering::Less => true,
                Ordering::Equal => *incl,
                Ordering::Greater => false,
            })
        })
    }
}
