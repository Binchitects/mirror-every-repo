//! The version ordering against Maven's own (see tests/gen_fixture.py).
use crate::version::{Range, Version};
use std::cmp::Ordering;

#[test]
fn ordering_equals_maven_on_every_pair() {
    let truth: serde_json::Value = serde_json::from_str(include_str!("../tests/maven_truth.json")).unwrap();
    let versions: Vec<&str> = truth["versions"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    let mut wrong = Vec::new();
    for (i, a) in versions.iter().enumerate() {
        for (j, b) in versions.iter().enumerate() {
            let want = match truth["cmp"][i][j].as_i64().unwrap() {
                -1 => Ordering::Less,
                0 => Ordering::Equal,
                _ => Ordering::Greater,
            };
            let (va, vb) = (Version::parse(a), Version::parse(b));
            if va.cmp(&vb) != want {
                wrong.push(format!("{a} vs {b}: maven says {want:?}, got {:?}   {} | {}", va.cmp(&vb), va.tokens(), vb.tokens()));
            }
        }
    }
    assert!(wrong.is_empty(), "{} of {} pairs differ:\n{}", wrong.len(), versions.len() * versions.len(), wrong[..wrong.len().min(25)].join("\n"));
    assert!(versions.len() >= 70);
}

#[test]
fn ranges() {
    let v = Version::parse;
    assert!(Range::parse("1.0").is_none() && Range::parse("").is_none());
    let r = Range::parse("[1.0,2.0)").unwrap();
    assert!(r.contains(&v("1.0")) && r.contains(&v("1.9.9")) && !r.contains(&v("2.0")) && !r.contains(&v("0.9")));
    assert!(r.contains(&v("2.0-rc1")), "a release candidate of 2.0 is below 2.0, as in Maven");
    let exact = Range::parse("[1.5]").unwrap();
    assert!(exact.contains(&v("1.5")) && exact.contains(&v("1.5.0")) && !exact.contains(&v("1.5.1")));
    let open = Range::parse("(,1.5],[2.0,)").unwrap();
    assert!(open.contains(&v("0.1")) && open.contains(&v("1.5")) && !open.contains(&v("1.6")) && open.contains(&v("3")));
    assert!(Range::parse("(1.0,)").unwrap().contains(&v("1.0.1")) && !Range::parse("(1.0,)").unwrap().contains(&v("1.0")));
    assert!(Range::parse("[1.0,2.0").is_none());
    assert!(Version::parse("1.0-SNAPSHOT").is_prerelease() && Version::parse("2.0.0.RC1").is_prerelease() && !Version::parse("2.0.0.RELEASE").is_prerelease());
}
