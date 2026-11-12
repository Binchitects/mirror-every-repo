use crate::*;
use mer_core::{Digest, EntryOp, EntryRec, EntryRole};
use std::collections::BTreeMap;

const A: EntryRole = EntryRole::Artifact;

fn meta_with_repo() -> (Meta, i64) {
    let m = Meta::open_in_memory().unwrap();
    let id = m.write(|t| Ok(t.repo_create("r", "raw")?.id)).unwrap();
    (m, id)
}

fn blob(t: &Tx, body: &str) -> Digest {
    let d = Digest::of(body.as_bytes());
    t.blob_present(&d, body.len() as u64, 0).unwrap();
    d
}

/// Build one sealed snapshot from (path, Some(body)) puts and (path, None) removals.
fn snap(m: &Meta, repo: i64, changes: &[(&str, Option<&str>)]) -> u64 {
    m.write(|t| {
        let seq = t.snapshot_begin(repo, 0)?;
        for (path, body) in changes {
            match body {
                Some(b) => {
                    let d = blob(t, b);
                    t.put_entry(repo, seq, path, A, &d)?;
                }
                None => {
                    t.remove_entry(repo, seq, path)?;
                }
            }
        }
        t.snapshot_seal(repo, seq, None, 0)?;
        Ok(seq)
    })
    .unwrap()
}

fn view(m: &Meta, repo: i64, seq: u64) -> BTreeMap<String, Digest> {
    m.read(|t| Ok(t.entries_at(repo, seq)?.into_iter().map(|e| (e.path, e.digest)).collect())).unwrap()
}

fn want(pairs: &[(&str, &str)]) -> BTreeMap<String, Digest> {
    pairs.iter().map(|(p, b)| (p.to_string(), Digest::of(b.as_bytes()))).collect()
}

#[test]
fn every_snapshot_stays_readable_as_it_was() {
    let (m, r) = meta_with_repo();
    let s1 = snap(&m, r, &[("a", Some("a1")), ("b", Some("b1"))]);
    let s2 = snap(&m, r, &[("a", Some("a2")), ("c", Some("c1"))]);
    let s3 = snap(&m, r, &[("b", None), ("c", Some("c2"))]);
    assert_eq!((s1, s2, s3), (1, 2, 3));
    assert_eq!(view(&m, r, 1), want(&[("a", "a1"), ("b", "b1")]));
    assert_eq!(view(&m, r, 2), want(&[("a", "a2"), ("b", "b1"), ("c", "c1")]));
    assert_eq!(view(&m, r, 3), want(&[("a", "a2"), ("c", "c2")]));
    m.read(|t| {
        assert_eq!(t.lookup(r, 1, "a")?.unwrap().digest, Digest::of(b"a1"));
        assert_eq!(t.lookup(r, 3, "a")?.unwrap().digest, Digest::of(b"a2"));
        assert!(t.lookup(r, 3, "b")?.is_none());
        assert!(t.lookup(r, 1, "c")?.is_none());
        let head = t.snapshot_head(r)?.unwrap();
        assert_eq!((head.seq, head.entry_count, head.total_bytes), (3, 2, 4));
        Ok(())
    })
    .unwrap();
}

#[test]
fn sealed_snapshots_are_immutable_and_one_build_at_a_time() {
    let (m, r) = meta_with_repo();
    snap(&m, r, &[("a", Some("1"))]);
    let err = m.write(|t| {
        let d = blob(t, "2");
        t.put_entry(r, 1, "a", A, &d)
    });
    assert!(matches!(err, Err(MetaError::Conflict(_))));
    m.write(|t| t.snapshot_begin(r, 0)).unwrap();
    assert!(matches!(m.write(|t| t.snapshot_begin(r, 0)), Err(MetaError::Conflict(_))));
}

#[test]
fn entries_need_a_stored_blob_and_a_safe_path() {
    let (m, r) = meta_with_repo();
    m.write(|t| {
        let seq = t.snapshot_begin(r, 0)?;
        assert!(matches!(
            t.put_entry(r, seq, "a", A, &Digest::of(b"never stored")),
            Err(MetaError::Invalid(_))
        ));
        let d = blob(t, "x");
        assert!(matches!(t.put_entry(r, seq, "../etc/passwd", A, &d), Err(MetaError::Core(_))));
        Ok(())
    })
    .unwrap();
}

#[test]
fn putting_a_value_back_leaves_no_trace() {
    let (m, r) = meta_with_repo();
    snap(&m, r, &[("a", Some("1")), ("b", Some("1"))]);
    m.write(|t| {
        let seq = t.snapshot_begin(r, 0)?;
        let one = blob(t, "1");
        let two = blob(t, "2");
        assert!(t.put_entry(r, seq, "a", A, &two)?);
        assert!(t.put_entry(r, seq, "a", A, &one)?); // back to the old value
        assert!(t.remove_entry(r, seq, "b")?);
        assert!(t.put_entry(r, seq, "b", A, &one)?); // removed then restored
        assert!(!t.put_entry(r, seq, "a", A, &one)?); // already there
        assert_eq!(t.snapshot_mutations(r, seq)?, 0);
        t.snapshot_seal(r, seq, None, 0)?;
        assert!(t.diff(r, 1, 2)?.is_empty());
        Ok(())
    })
    .unwrap();
    assert_eq!(view(&m, r, 2), view(&m, r, 1));
}

#[test]
fn abort_is_an_exact_undo() {
    let (m, r) = meta_with_repo();
    snap(&m, r, &[("a", Some("1")), ("b", Some("1"))]);
    let before = view(&m, r, 1);
    m.write(|t| {
        let seq = t.snapshot_begin(r, 0)?;
        let d = blob(t, "2");
        t.put_entry(r, seq, "a", A, &d)?;
        t.put_entry(r, seq, "new", A, &d)?;
        t.remove_entry(r, seq, "b")?;
        t.snapshot_abort(r, seq)
    })
    .unwrap();
    assert_eq!(view(&m, r, 1), before);
    // The aborted seq is not reused and the next snapshot builds on the old state.
    let s = snap(&m, r, &[("c", Some("3"))]);
    assert_eq!(s, 3);
    assert_eq!(view(&m, r, 3), want(&[("a", "1"), ("b", "1"), ("c", "3")]));
    m.read(|t| {
        assert_eq!(t.snapshot_get(r, 2)?.unwrap().state, SnapState::Aborted);
        Ok(())
    })
    .unwrap();
}

/// Applying diff(from, to) to the view at `from` must give the view at `to`,
/// for every pair, over a random history.
#[test]
fn diff_matches_brute_force_on_random_history() {
    let (m, r) = meta_with_repo();
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move |n: u64| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % n
    };
    let mut last = 0;
    for _ in 0..25 {
        let mut changes: Vec<(String, Option<String>)> = Vec::new();
        for _ in 0..(1 + next(6)) {
            let path = format!("dir{}/f{}", next(3), next(8));
            let body = if next(4) == 0 { None } else { Some(format!("v{}", next(5))) };
            changes.push((path, body));
        }
        let borrowed: Vec<(&str, Option<&str>)> =
            changes.iter().map(|(p, b)| (p.as_str(), b.as_deref())).collect();
        last = snap(&m, r, &borrowed);
    }
    for from in 0..=last {
        for to in from..=last {
            let mut state: BTreeMap<String, EntryRec> = m
                .read(|t| Ok(t.entries_at(r, from)?.into_iter().map(|e| (e.path.clone(), e)).collect()))
                .unwrap();
            let ops = m.read(|t| t.diff(r, from, to)).unwrap();
            let paths: Vec<&str> = ops.iter().map(|o| o.path()).collect();
            let mut sorted = paths.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(paths, sorted, "ops sorted, one per path");
            for op in ops {
                match op {
                    EntryOp::Put(e) => {
                        assert_ne!(state.get(&e.path), Some(&e), "no-op put in diff {from}->{to}");
                        state.insert(e.path.clone(), e);
                    }
                    EntryOp::Del { path } => {
                        assert!(state.remove(&path).is_some(), "delete of absent path {from}->{to}");
                    }
                }
            }
            let target: BTreeMap<String, EntryRec> = m
                .read(|t| Ok(t.entries_at(r, to)?.into_iter().map(|e| (e.path.clone(), e)).collect()))
                .unwrap();
            assert_eq!(state, target, "diff {from}->{to}");
        }
    }
}

#[test]
fn publish_needs_approval_and_rollback_is_a_pointer_move() {
    let (m, r) = meta_with_repo();
    snap(&m, r, &[("a", Some("1"))]);
    snap(&m, r, &[("a", Some("2"))]);
    m.write(|t| {
        // Not approved yet: cannot go live.
        assert!(matches!(t.publish(r, 1, PublishKind::Publish, 0), Err(MetaError::Conflict(_))));
        t.snapshot_decide(r, 1, true)?;
        t.snapshot_decide(r, 2, true)?;
        assert_eq!(t.publish(r, 1, PublishKind::Publish, 0)?.generation, 1);
        let p = t.publish(r, 2, PublishKind::Publish, 0)?;
        assert_eq!((p.seq, p.prev_seq, p.generation), (2, Some(1), 2));
        // Going backwards through "publish" is refused; it must be a rollback.
        assert!(matches!(t.publish(r, 1, PublishKind::Publish, 0), Err(MetaError::Conflict(_))));
        let p = t.publish(r, 1, PublishKind::Rollback, 0)?;
        assert_eq!((p.seq, p.prev_seq, p.kind.as_str()), (1, Some(2), "rollback"));
        assert!(matches!(t.publish(r, 2, PublishKind::Rollback, 0), Err(MetaError::Conflict(_))));
        Ok(())
    })
    .unwrap();
}

#[test]
fn rejected_snapshot_can_never_go_live() {
    let (m, r) = meta_with_repo();
    snap(&m, r, &[("a", Some("1"))]);
    m.write(|t| {
        t.snapshot_decide(r, 1, false)?;
        assert!(matches!(t.publish(r, 1, PublishKind::Publish, 0), Err(MetaError::Conflict(_))));
        assert!(matches!(t.snapshot_decide(r, 1, true), Err(MetaError::Conflict(_))));
        Ok(())
    })
    .unwrap();
}

#[test]
fn gc_only_takes_unreferenced_blobs_after_the_grace_period() {
    let (m, r) = meta_with_repo();
    snap(&m, r, &[("a", Some("kept")), ("b", Some("old"))]);
    snap(&m, r, &[("b", Some("new"))]);
    let loose = m.write(|t| Ok(blob(t, "never referenced"))).unwrap();
    m.write(|t| {
        // "old" is still part of snapshot 1, so only the loose blob is orphaned.
        assert_eq!(t.gc_mark(1000)?, 1);
        assert!(t.gc_claim(999, 100)?.is_empty(), "inside grace period");
        Ok(())
    })
    .unwrap();
    // A reference appearing before the sweep rescues the blob.
    m.write(|t| {
        t.blob_ref_add("test", &loose)?;
        assert_eq!(t.gc_mark(2000)?, 0);
        t.blob_ref_clear("test")?;
        assert_eq!(t.gc_mark(3000)?, 1);
        Ok(())
    })
    .unwrap();
    m.write(|t| {
        let claimed = t.gc_claim(3000, 100)?;
        assert_eq!(claimed, vec![loose]);
        assert_eq!(t.gc_deleting()?, vec![loose]);
        // While it is being deleted nobody can resurrect the row half-way.
        assert!(matches!(t.blob_present(&loose, 16, 0), Err(MetaError::Busy(_))));
        t.gc_finish(&loose)?;
        assert!(t.blob_get(&loose)?.is_none());
        assert!(t.blob_is_present(&Digest::of(b"old"))?);
        Ok(())
    })
    .unwrap();
}

#[test]
fn compaction_frees_old_versions_but_keeps_later_snapshots_exact() {
    let (m, r) = meta_with_repo();
    snap(&m, r, &[("a", Some("a1")), ("b", Some("b1"))]);
    snap(&m, r, &[("a", Some("a2"))]);
    snap(&m, r, &[("b", None)]);
    let v2 = view(&m, r, 2);
    let v3 = view(&m, r, 3);
    m.write(|t| {
        t.snapshot_decide(r, 2, true)?;
        t.publish(r, 2, PublishKind::Publish, 0)?;
        // Snapshot 2 is live: history cannot be cut above it.
        assert!(matches!(t.compact(r, 3), Err(MetaError::Conflict(_))));
        assert_eq!(t.compact(r, 2)?, 1); // the a1 interval
        assert_eq!(t.snapshot_get(r, 1)?.unwrap().state, SnapState::Retired);
        assert_eq!(t.gc_mark(10)?, 1); // a1 is now unreferenced, b1 is still in snapshot 2
        assert_eq!(t.gc_claim(10, 10)?, vec![Digest::of(b"a1")]);
        Ok(())
    })
    .unwrap();
    assert_eq!(view(&m, r, 2), v2);
    assert_eq!(view(&m, r, 3), v3);
}

#[test]
fn usage_counts_each_live_blob_once() {
    let (m, r) = meta_with_repo();
    snap(&m, r, &[("a", Some("12345")), ("copy-of-a", Some("12345")), ("b", Some("xy"))]);
    snap(&m, r, &[("b", None)]);
    assert_eq!(m.read(|t| t.repo_usage(r)).unwrap(), 5);
}

#[test]
fn audit_log_detects_tampering() {
    let m = Meta::open_in_memory().unwrap();
    m.write(|t| {
        t.audit(1, "alice", "publish", "repo r seq 1")?;
        t.audit(2, "bob", "rollback", "repo r seq 1")?;
        t.audit(3, "alice", "gc", "")?;
        assert_eq!(t.audit_verify()?, Ok(3));
        Ok(())
    })
    .unwrap();
    m.write(|t| {
        t.raw_execute("UPDATE audit_log SET actor = 'mallory' WHERE seq = 2")?;
        assert_eq!(t.audit_verify()?, Err(2));
        Ok(())
    })
    .unwrap();
    let m = Meta::open_in_memory().unwrap();
    m.write(|t| {
        t.audit(1, "a", "x", "")?;
        t.audit(2, "a", "y", "")?;
        t.audit(3, "a", "z", "")?;
        t.raw_execute("DELETE FROM audit_log WHERE seq = 2")?;
        assert_eq!(t.audit_verify()?, Err(2));
        Ok(())
    })
    .unwrap();
}

#[test]
fn failed_transaction_changes_nothing_and_data_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta.db");
    {
        let m = Meta::open(&path).unwrap();
        let r = m.write(|t| Ok(t.repo_create("r", "raw")?.id)).unwrap();
        snap(&m, r, &[("a", Some("1"))]);
        let res: Result<()> = m.write(|t| {
            let seq = t.snapshot_begin(r, 0)?;
            let d = blob(t, "2");
            t.put_entry(r, seq, "a", A, &d)?;
            Err(MetaError::Invalid("simulated failure".into()))
        });
        assert!(res.is_err());
        assert!(m.read(|t| t.snapshot_building(r)).unwrap().is_none());
    }
    let m = Meta::open(&path).unwrap();
    let r = m.read(|t| Ok(t.repo_require("r")?.id)).unwrap();
    assert_eq!(view(&m, r, 1), want(&[("a", "1")]));
    assert!(!m.read(|t| t.blob_is_present(&Digest::of(b"2"))).unwrap());
}

#[test]
fn misses_are_counted_and_ranked() {
    let m = Meta::open_in_memory().unwrap();
    m.write(|t| {
        t.miss_record("pypi", "requests/2.99.0", 1)?;
        t.miss_record("pypi", "requests/2.99.0", 5)?;
        t.miss_record("npm", "left-pad", 2)?;
        let l = t.miss_list(10)?;
        assert_eq!(l[0].path, "requests/2.99.0");
        assert_eq!((l[0].count, l[0].last_at), (2, 5));
        assert_eq!(l.len(), 2);
        Ok(())
    })
    .unwrap();
}
