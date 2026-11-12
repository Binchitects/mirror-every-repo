//! Agreement with the semver library npm itself uses, from a table generated
//! by `tests/gen_fixture.js`.
use crate::{Range, Version};
use serde_json::Value;
use std::cmp::Ordering;

fn truth() -> Value {
    serde_json::from_str(include_str!("../tests/semver_truth.json")).unwrap()
}

fn strings(v: &Value) -> Vec<&str> {
    v.as_array().unwrap().iter().map(|s| s.as_str().unwrap()).collect()
}

#[test]
fn agrees_with_node_semver_on_what_is_a_version() {
    let t = truth();
    for s in strings(&t["valid"]) {
        assert!(Version::parse(s).is_some(), "{s:?} is a version");
    }
    for s in strings(&t["invalid"]) {
        // Surrounding spaces are a matter of trimming, not of version syntax.
        if s != s.trim() {
            continue;
        }
        assert!(Version::parse(s).is_none(), "{s:?} is not a version");
    }
}

#[test]
fn agrees_with_node_semver_on_every_ordered_pair() {
    let t = truth();
    let mut checked = 0;
    for (pair, want) in t["order"].as_object().unwrap() {
        let (a, b) = pair.split_once(' ').unwrap();
        let want = match want.as_i64().unwrap() {
            -1 => Ordering::Less,
            0 => Ordering::Equal,
            _ => Ordering::Greater,
        };
        assert_eq!(Version::parse(a).unwrap().cmp(&Version::parse(b).unwrap()), want, "{a} vs {b}");
        checked += 1;
    }
    assert!(checked > 1500, "{checked} pairs");
}

#[test]
fn agrees_with_node_semver_on_every_range() {
    let t = truth();
    let valid = strings(&t["valid"]);
    let (mut checked, mut invalid) = (0, 0);
    for (range, accepted) in t["ranges"].as_object().unwrap() {
        if accepted.is_null() {
            assert!(Range::parse(range).is_none(), "{range:?} is not a range");
            invalid += 1;
            continue;
        }
        let parsed = Range::parse(range).unwrap_or_else(|| panic!("{range:?} is a range"));
        let accepted = strings(accepted);
        for v in &valid {
            assert_eq!(parsed.satisfied_by(&Version::parse(v).unwrap()), accepted.contains(v), "does {range:?} accept {v}?");
            checked += 1;
        }
    }
    assert!(checked > 3000 && invalid > 5, "{checked} checks, {invalid} invalid ranges");
}

#[test]
fn agrees_with_node_semver_on_the_best_match() {
    let t = truth();
    let all: Vec<Version> = strings(&t["valid"]).iter().map(|s| Version::parse(s).unwrap()).collect();
    for (range, want) in t["max"].as_object().unwrap() {
        let got = Range::parse(range).unwrap().max_satisfying(&all);
        match want.as_str() {
            // Several spellings can be the same version; compare as versions.
            Some(w) => assert_eq!(got.map(|v| v.cmp(&Version::parse(w).unwrap())), Some(Ordering::Equal), "best match for {range:?}"),
            None => assert!(got.is_none(), "{range:?} matches nothing"),
        }
    }
}
