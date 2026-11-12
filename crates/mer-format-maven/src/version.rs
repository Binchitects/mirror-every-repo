//! Maven version ordering (`ComparableVersion`) and version ranges.
//!
//! The ordering is checked against Maven itself: see `tests/gen_fixture.py`
//! and the truth table it writes.
use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    /// Digits without leading zeros ("" is zero), so any length compares.
    Int(String),
    /// A qualifier, lower-case, with aliases applied ("" is a release).
    Str(String),
    List(Vec<Item>),
}

const QUALIFIERS: [&str; 7] = ["alpha", "beta", "milestone", "rc", "snapshot", "", "sp"];

/// Known qualifiers order by their place in the list; unknown ones come
/// after all of them, in alphabetical order.
fn qualifier_key(q: &str) -> String {
    match QUALIFIERS.iter().position(|k| *k == q) {
        Some(i) => i.to_string(),
        None => format!("{}-{q}", QUALIFIERS.len()),
    }
}

fn string_item(text: &str, followed_by_digit: bool) -> Item {
    let expanded = match (followed_by_digit, text) {
        (true, "a") => "alpha",
        (true, "b") => "beta",
        (true, "m") => "milestone",
        _ => text,
    };
    Item::Str(
        match expanded {
            "ga" | "final" | "release" => "",
            "cr" => "rc",
            other => other,
        }
        .to_string(),
    )
}

fn parse_item(is_digit: bool, text: &str) -> Item {
    if is_digit {
        Item::Int(text.trim_start_matches('0').to_string())
    } else {
        string_item(text, false)
    }
}

impl Item {
    fn is_null(&self) -> bool {
        match self {
            Item::Int(d) => d.is_empty(),
            Item::Str(s) => s.is_empty(),
            Item::List(l) => l.is_empty(),
        }
    }

    /// Compare with an item that is not there (a shorter version).
    fn cmp_absent(&self) -> Ordering {
        match self {
            Item::Int(d) => {
                if d.is_empty() {
                    Ordering::Equal
                } else {
                    Ordering::Greater
                }
            }
            Item::Str(s) => qualifier_key(s).cmp(&qualifier_key("")),
            Item::List(l) => l.iter().map(Item::cmp_absent).find(|o| o.is_ne()).unwrap_or(Ordering::Equal),
        }
    }

    fn cmp_item(&self, other: &Item) -> Ordering {
        match (self, other) {
            (Item::Int(a), Item::Int(b)) => a.len().cmp(&b.len()).then_with(|| a.cmp(b)),
            (Item::Int(_), _) => Ordering::Greater, // 1.1 > 1-sp, 1.1 > 1-1
            (Item::Str(_), Item::Int(_)) => Ordering::Less,
            (Item::Str(a), Item::Str(b)) => qualifier_key(a).cmp(&qualifier_key(b)),
            (Item::Str(_), Item::List(_)) => Ordering::Less, // 1.any < 1-1
            (Item::List(_), Item::Int(_)) => Ordering::Less,
            (Item::List(_), Item::Str(_)) => Ordering::Greater,
            (Item::List(a), Item::List(b)) => {
                for i in 0..a.len().max(b.len()) {
                    let o = match (a.get(i), b.get(i)) {
                        (Some(l), Some(r)) => l.cmp_item(r),
                        (Some(l), None) => l.cmp_absent(),
                        (None, Some(r)) => r.cmp_absent().reverse(),
                        (None, None) => Ordering::Equal,
                    };
                    if o.is_ne() {
                        return o;
                    }
                }
                Ordering::Equal
            }
        }
    }
}

/// Drop trailing items that mean nothing: `1.0.0` is `1`, `1.0-ga` is `1`.
fn normalize(list: &mut Vec<Item>) {
    let mut i = list.len();
    while i > 0 {
        i -= 1;
        if list[i].is_null() {
            // A zero counts only where a number follows it: 1.0.1 keeps its
            // zero, 1.0-1 and 1.0-rc do not.
            let remove = !matches!(list.get(i + 1), Some(Item::Int(_)));
            if remove {
                list.remove(i);
            }
        } else if !matches!(list[i], Item::List(_)) {
            break;
        }
    }
}

/// A list being built, innermost last. Closing one normalises it and puts
/// it into its parent.
struct Builder {
    stack: Vec<Vec<Item>>,
}

impl Builder {
    fn add(&mut self, item: Item) {
        self.stack.last_mut().expect("never empty").push(item);
    }
    fn open(&mut self) {
        self.stack.push(Vec::new());
    }
    fn is_empty(&self) -> bool {
        self.stack.last().expect("never empty").is_empty()
    }
    fn finish(mut self) -> Vec<Item> {
        while self.stack.len() > 1 {
            let mut inner = self.stack.pop().expect("checked");
            normalize(&mut inner);
            self.add(Item::List(inner));
        }
        let mut top = self.stack.pop().expect("one left");
        normalize(&mut top);
        top
    }
}

/// A version as Maven orders it.
#[derive(Clone, Debug)]
pub struct Version {
    text: String,
    items: Vec<Item>,
}

impl Version {
    pub fn parse(text: &str) -> Version {
        let lower = text.to_lowercase();
        let chars: Vec<char> = lower.chars().collect();
        let slice = |from: usize, to: usize| chars[from..to].iter().collect::<String>();
        let mut b = Builder { stack: vec![Vec::new()] };
        let (mut is_digit, mut start) = (false, 0usize);
        for (i, &c) in chars.iter().enumerate() {
            if c == '.' || c == '-' {
                if i == start {
                    b.add(Item::Int(String::new()));
                } else {
                    b.add(parse_item(is_digit, &slice(start, i)));
                }
                start = i + 1;
                if c == '-' {
                    b.open();
                }
            } else if c.is_ascii_digit() {
                if !is_digit && i > start {
                    // 1.0.0.x1 is 1.0.0-x1: a qualifier always starts a sub-list.
                    if !b.is_empty() {
                        b.open();
                    }
                    b.add(string_item(&slice(start, i), true));
                    start = i;
                    b.open();
                }
                is_digit = true;
            } else {
                if is_digit && i > start {
                    b.add(parse_item(true, &slice(start, i)));
                    start = i;
                    b.open();
                }
                is_digit = false;
            }
        }
        if chars.len() > start {
            if !is_digit && !b.is_empty() {
                b.open();
            }
            b.add(parse_item(is_digit, &slice(start, chars.len())));
        }
        Version { text: text.to_string(), items: b.finish() }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Not a release: a snapshot, or an alpha, beta, milestone or release candidate.
    pub fn is_prerelease(&self) -> bool {
        fn any(items: &[Item]) -> bool {
            items.iter().any(|i| match i {
                Item::Str(s) => matches!(s.as_str(), "alpha" | "beta" | "milestone" | "rc" | "snapshot"),
                Item::List(l) => any(l),
                Item::Int(_) => false,
            })
        }
        any(&self.items)
    }

    #[cfg(test)]
    pub(crate) fn tokens(&self) -> String {
        format!("{:?}", self.items)
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for Version {}
impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        Item::List(self.items.clone()).cmp_item(&Item::List(other.items.clone()))
    }
}

/// A version requirement of a dependency: `1.0` (a preference, taken as
/// that version), `[1.0]`, `[1.0,2.0)`, `(,1.5],[2.0,)`.
#[derive(Clone, Debug)]
pub struct Range {
    /// (lower, inclusive, upper, inclusive); `None` is unbounded.
    sets: Vec<(Option<Version>, bool, Option<Version>, bool)>,
}

impl Range {
    /// `None` when the text is a plain version, not a range.
    pub fn parse(text: &str) -> Option<Range> {
        let t = text.trim();
        if !t.starts_with(['[', '(']) {
            return None;
        }
        let mut sets = Vec::new();
        let mut rest = t;
        while !rest.is_empty() {
            let lower_inclusive = match rest.chars().next()? {
                '[' => true,
                '(' => false,
                _ => return None,
            };
            let end = rest.find([']', ')'])?;
            let upper_inclusive = rest[end..].starts_with(']');
            let inner = &rest[1..end];
            let bound = |s: &str| (!s.trim().is_empty()).then(|| Version::parse(s.trim()));
            match inner.split_once(',') {
                Some((lo, hi)) => sets.push((bound(lo), lower_inclusive, bound(hi), upper_inclusive)),
                // [1.0] is exactly 1.0.
                None => sets.push((bound(inner), true, bound(inner), true)),
            }
            rest = rest[end + 1..].trim_start().trim_start_matches(',').trim_start();
        }
        (!sets.is_empty()).then_some(Range { sets })
    }

    pub fn contains(&self, v: &Version) -> bool {
        self.sets.iter().any(|(lo, lo_inc, hi, hi_inc)| {
            lo.as_ref().is_none_or(|l| if *lo_inc { v >= l } else { v > l }) && hi.as_ref().is_none_or(|h| if *hi_inc { v <= h } else { v < h })
        })
    }
}
