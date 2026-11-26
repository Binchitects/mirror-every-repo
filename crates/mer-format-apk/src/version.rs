//! Alpine package versions, ordered as apk orders them.
//!
//! A port of `apk_version_compare` from apk-tools 3.0 (`src/version.c`):
//! `digit{.digit}...{letter}{_suffix{digit}}...{~hash}{-r#}`. Checked against
//! `apk version -t` with the truth table in `tests/apk_truth.json`.
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Token {
    InitialDigit,
    Digit,
    Letter,
    Suffix,
    SuffixNo,
    CommitHash,
    RevisionNo,
    End,
    Invalid,
}

/// Suffixes in their order; `None` is a version without one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Suffix {
    Invalid,
    Alpha,
    Beta,
    Pre,
    Rc,
    None,
    Cvs,
    Svn,
    Git,
    Hg,
    P,
}

fn suffix_value(s: &[u8]) -> Suffix {
    match s {
        b"alpha" => Suffix::Alpha,
        b"beta" => Suffix::Beta,
        b"pre" => Suffix::Pre,
        b"rc" => Suffix::Rc,
        b"cvs" => Suffix::Cvs,
        b"svn" => Suffix::Svn,
        b"git" => Suffix::Git,
        b"hg" => Suffix::Hg,
        b"p" => Suffix::P,
        _ => Suffix::Invalid,
    }
}

struct State<'a> {
    token: Token,
    suffix: Suffix,
    number: u64,
    value: &'a [u8],
    rest: &'a [u8],
}

impl<'a> State<'a> {
    fn first(v: &'a [u8]) -> State<'a> {
        let mut t = State { token: Token::InitialDigit, suffix: Suffix::Invalid, number: 0, value: b"", rest: v };
        t.digits();
        t
    }

    fn digits(&mut self) {
        let n = self.rest.iter().take_while(|b| b.is_ascii_digit()).count();
        self.value = &self.rest[..n];
        // apk-tools accumulates in a uint64_t and lets it wrap.
        self.number = self.value.iter().fold(0u64, |acc, d| acc.wrapping_mul(10).wrapping_add(u64::from(d - b'0')));
        self.rest = &self.rest[n..];
        if n == 0 {
            self.token = Token::Invalid;
        }
    }

    fn next(&mut self) {
        let Some(&c) = self.rest.first() else {
            self.token = Token::End;
            return;
        };
        match c {
            b'a'..=b'z' => {
                if self.token > Token::Digit {
                    self.token = Token::Invalid;
                    return;
                }
                self.value = &self.rest[..1];
                self.token = Token::Letter;
                self.rest = &self.rest[1..];
            }
            b'.' | b'0'..=b'9' => {
                if c == b'.' {
                    if self.token > Token::Digit {
                        self.token = Token::Invalid;
                        return;
                    }
                    self.rest = &self.rest[1..];
                }
                self.token = match self.token {
                    Token::InitialDigit | Token::Digit => Token::Digit,
                    Token::Suffix => Token::SuffixNo,
                    _ => {
                        self.token = Token::Invalid;
                        return;
                    }
                };
                self.digits();
            }
            b'_' => {
                if self.token > Token::SuffixNo {
                    self.token = Token::Invalid;
                    return;
                }
                self.rest = &self.rest[1..];
                let n = self.rest.iter().take_while(|b| b.is_ascii_lowercase()).count();
                self.value = &self.rest[..n];
                self.rest = &self.rest[n..];
                self.suffix = suffix_value(self.value);
                self.token = if self.suffix == Suffix::Invalid { Token::Invalid } else { Token::Suffix };
            }
            b'~' => {
                if self.token >= Token::CommitHash {
                    self.token = Token::Invalid;
                    return;
                }
                self.rest = &self.rest[1..];
                let n = self.rest.iter().take_while(|b| b.is_ascii_hexdigit()).count();
                self.value = &self.rest[..n];
                self.rest = &self.rest[n..];
                self.token = if n == 0 { Token::Invalid } else { Token::CommitHash };
            }
            b'-' => {
                if self.token >= Token::RevisionNo || !self.rest.starts_with(b"-r") {
                    self.token = Token::Invalid;
                    return;
                }
                self.rest = &self.rest[2..];
                self.token = Token::RevisionNo;
                self.digits();
            }
            _ => self.token = Token::Invalid,
        }
    }
}

fn token_cmp(a: &State, b: &State) -> Ordering {
    let (x, y) = match a.token {
        // A leading zero in either: compare as strings, as Gentoo does.
        Token::Digit if a.value.first() == Some(&b'0') || b.value.first() == Some(&b'0') => return a.value.cmp(b.value),
        Token::Digit | Token::InitialDigit | Token::SuffixNo | Token::RevisionNo => (a.number, b.number),
        Token::Letter => (u64::from(a.value[0]), u64::from(b.value[0])),
        Token::Suffix => (a.suffix as u64, b.suffix as u64),
        _ => return a.value.cmp(b.value),
    };
    x.cmp(&y)
}

/// Whether apk accepts `v` as a version.
pub fn valid(v: &str) -> bool {
    let mut t = State::first(v.as_bytes());
    while t.token < Token::End {
        t.next();
    }
    t.token == Token::End
}

/// Order two versions as `apk version -t` does.
pub fn compare(a: &str, b: &str) -> Ordering {
    let (mut ta, mut tb) = (State::first(a.as_bytes()), State::first(b.as_bytes()));
    while ta.token == tb.token && ta.token < Token::End {
        let r = token_cmp(&ta, &tb);
        if r != Ordering::Equal {
            return r;
        }
        ta.next();
        tb.next();
    }
    if ta.token == tb.token {
        return Ordering::Equal;
    }
    // Equal so far: the longer one is newer, unless what it has more is a
    // pre-release suffix.
    if ta.token == Token::Suffix && ta.suffix < Suffix::None {
        return Ordering::Less;
    }
    if tb.token == Token::Suffix && tb.suffix < Suffix::None {
        return Ordering::Greater;
    }
    if ta.token > tb.token {
        return Ordering::Less;
    }
    if tb.token > ta.token {
        return Ordering::Greater;
    }
    Ordering::Equal
}
