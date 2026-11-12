use crate::*;
use mer_bundle_format::{SegmentSink, SegmentSource, SlotOutcome};
use mer_core::{ChannelId, EntryRole};
use mer_crypto::{KeyPair, TrustStore};
use mer_storage::{Lookup, Storage};
use std::collections::BTreeMap;
use std::sync::Arc;

const CH: ChannelId = ChannelId([7; 8]);

struct Side {
    dir: tempfile::TempDir,
    st: Arc<Storage>,
}

fn side() -> Side {
    let dir = tempfile::tempdir().unwrap();
    let st = Arc::new(Storage::open(dir.path()).unwrap());
    Side { dir, st }
}

fn key() -> KeyPair {
    KeyPair::from_seed([42; 32])
}

fn opts(window: u64) -> SealOptions {
    SealOptions { segment_size: 4096, step_window: window, collector: "test".into() }
}

fn vault_on(s: &Side, auto_publish: bool) -> Vault {
    let trust = TrustStore::new(vec![key().public()], 1);
    Vault::new(s.st.clone(), trust, ImportPolicy { auto_publish }).unwrap()
}

/// One collector-side snapshot: Some(bytes) puts a file, None removes it.
fn change(st: &Storage, repo: &str, files: &[(&str, Option<&[u8]>)]) {
    if st.meta().read(|t| t.repo_get(repo)).unwrap().is_none() {
        st.create_repo(repo, "raw").unwrap();
    }
    let mut w = st.begin(repo).unwrap();
    for (path, body) in files {
        match body {
            Some(b) => {
                let (d, _) = st.ingest_bytes(b).unwrap();
                w.put(path, EntryRole::Artifact, &d).unwrap();
            }
            None => w.remove(path).unwrap(),
        }
    }
    w.seal(None).unwrap();
}

fn seal(c: &Side, window: u64) -> SealedBundle {
    seal_bundle(&c.st, CH, &[&key()], &opts(window)).unwrap().expect("something to ship")
}

/// Hand every block to the vault, last block first.
fn deliver(b: &SealedBundle, v: &Vault) {
    let id = v.offer_descriptor(b.descriptor_envelope()).unwrap();
    for i in (0..b.params().segment_count).rev() {
        let out = v.offer_block(&id, i, &b.read_block(i).unwrap());
        assert_eq!(out, SlotOutcome::Accepted, "segment {i}");
    }
    assert!(v.missing(&id).is_empty());
}

/// Full content of the newest snapshot of a repo.
fn head_view(st: &Storage, repo: &str) -> BTreeMap<String, Vec<u8>> {
    let Some(r) = st.meta().read(|t| t.repo_get(repo)).unwrap() else { return BTreeMap::new() };
    let Some(head) = st.meta().read(|t| t.snapshot_head(r.id)).unwrap() else { return BTreeMap::new() };
    st.meta()
        .read(|t| t.entries_at(r.id, head.seq))
        .unwrap()
        .into_iter()
        .map(|e| (e.path, st.read_blob(&e.digest).unwrap()))
        .collect()
}

fn big(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| seed.wrapping_add((i % 253) as u8)).collect()
}

#[test]
fn content_crosses_the_gap_and_waits_for_approval() {
    let (c, v) = (side(), side());
    let vault = vault_on(&v, false);
    change(&c.st, "debian", &[("pool/a.deb", Some(&big(1, 20_000))), ("dists/Release", Some(b"rel-1"))]);
    change(&c.st, "pypi", &[("simple/requests/index.html", Some(b"<html>"))]);
    let b = seal(&c, 8);
    assert!(b.params().segment_count > 3, "spans several segments");

    deliver(&b, &vault);
    let reports = vault.import_ready().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].repos.len(), 2);
    assert!(reports[0].repos.iter().all(|r| r.state == ApplyState::Applied && r.applied_seq == 1));

    assert_eq!(head_view(&v.st, "debian"), head_view(&c.st, "debian"));
    assert_eq!(head_view(&v.st, "pypi"), head_view(&c.st, "pypi"));

    // Quarantine: imported content is not visible to package clients yet.
    assert_eq!(v.st.get("debian", "pool/a.deb").unwrap(), Lookup::NotPublished);
    let seq = reports[0].repos.iter().find(|r| r.repo == "debian").unwrap().snapshot.unwrap();
    v.st.decide("debian", seq, true, "alice").unwrap();
    v.st.publish("debian", seq, "alice").unwrap();
    assert!(matches!(v.st.get("debian", "pool/a.deb").unwrap(), Lookup::Found(_)));
    assert!(vault.staged().is_empty());
    assert!(!v.dir.path().join("staging").join(CH.to_hex()).read_dir().unwrap().any(|_| true), "staging cleaned");
}

#[test]
fn second_bundle_carries_only_what_changed() {
    let (c, v) = (side(), side());
    let vault = vault_on(&v, true);
    change(&c.st, "debian", &[("a", Some(&big(1, 50_000))), ("b", Some(&big(2, 50_000)))]);
    let b1 = seal(&c, 8);
    deliver(&b1, &vault);
    vault.import_ready().unwrap();

    // Change one file, delete one, add a copy of existing content under a new name.
    change(&c.st, "debian", &[("a", Some(&big(3, 1000))), ("b", None), ("a-copy", Some(&big(1, 50_000)))]);
    let b2 = seal(&c, 8);
    assert!(b2.payload_bytes() < 5_000, "only the new 1000 bytes and change lists, got {}", b2.payload_bytes());
    assert!(b2.stream_len() < b1.stream_len() / 10);

    deliver(&b2, &vault);
    let rep = vault.import_ready().unwrap();
    assert_eq!(rep[0].repos[0].state, ApplyState::Applied);
    assert_eq!(head_view(&v.st, "debian"), head_view(&c.st, "debian"));
    // auto-publish policy: clients see the new state, the deleted file is gone.
    assert!(matches!(v.st.get("debian", "a-copy").unwrap(), Lookup::Found(_)));
    assert_eq!(v.st.get("debian", "b").unwrap(), Lookup::Miss);
}

#[test]
fn nothing_changed_means_no_bundle() {
    let c = side();
    assert!(seal_bundle(&c.st, CH, &[&key()], &opts(8)).unwrap().is_none());
    change(&c.st, "r", &[("a", Some(b"1"))]);
    assert_eq!(seal(&c, 8).seq(), 1);
    assert!(seal_bundle(&c.st, CH, &[&key()], &opts(8)).unwrap().is_none());
    change(&c.st, "r", &[("a", Some(b"2"))]);
    change(&c.st, "r", &[("a", Some(b"1"))]); // back to what was shipped
    assert!(seal_bundle(&c.st, CH, &[&key()], &opts(8)).unwrap().is_none());
}

#[test]
fn untrusted_signer_and_tampered_blocks_are_refused() {
    let (c, v) = (side(), side());
    change(&c.st, "r", &[("a", Some(&big(1, 10_000)))]);
    let b = seal(&c, 8);

    let stranger = Vault::new(
        v.st.clone(),
        TrustStore::new(vec![KeyPair::from_seed([1; 32]).public()], 1),
        ImportPolicy::default(),
    )
    .unwrap();
    assert!(stranger.offer_descriptor(b.descriptor_envelope()).is_err());
    assert!(stranger.staged().is_empty(), "nothing is staged for an untrusted bundle");

    let vault = vault_on(&v, false);
    let id = vault.offer_descriptor(b.descriptor_envelope()).unwrap();
    let mut bad = b.read_block(1).unwrap();
    let last = bad.len() - 1;
    bad[last] ^= 1;
    assert!(matches!(vault.offer_block(&id, 1, &bad), SlotOutcome::Rejected(_)));
    assert!(matches!(vault.offer_block(&id, 0, &b.read_block(1).unwrap()), SlotOutcome::Rejected(_)));
    assert!(!vault.has(&id, 1));
    // The genuine block is still accepted afterwards.
    assert_eq!(vault.offer_block(&id, 1, &b.read_block(1).unwrap()), SlotOutcome::Accepted);
    assert_eq!(vault.offer_block(&id, 1, &b.read_block(1).unwrap()), SlotOutcome::Duplicate);
    assert!(vault.import_ready().unwrap().is_empty(), "incomplete bundle is not imported");
}

#[test]
fn replay_is_a_no_op_and_a_fork_is_refused() {
    let (c, c2, v) = (side(), side(), side());
    let vault = vault_on(&v, false);
    change(&c.st, "r", &[("a", Some(b"genuine"))]);
    let b = seal(&c, 8);
    deliver(&b, &vault);
    assert_eq!(vault.import_ready().unwrap().len(), 1);

    // Same bundle again (a second USB copy, or a diode repeat pass).
    let id = vault.offer_descriptor(b.descriptor_envelope()).unwrap();
    assert_eq!(vault.offer_block(&id, 0, &b.read_block(0).unwrap()), SlotOutcome::Duplicate);
    assert!(vault.missing(&id).is_empty());
    assert!(vault.import_ready().unwrap().is_empty());

    // A different bundle, validly signed, claiming the same position.
    change(&c2.st, "r", &[("a", Some(b"forged"))]);
    let forged = seal(&c2, 8);
    assert_eq!(forged.seq(), 1);
    let err = vault.offer_descriptor(forged.descriptor_envelope()).unwrap_err();
    assert!(err.to_string().contains("fork"), "{err}");
    assert_eq!(head_view(&v.st, "r")["a"], b"genuine");
}

#[test]
fn a_missed_bundle_stalls_only_until_it_arrives() {
    let (c, v) = (side(), side());
    let vault = vault_on(&v, false);
    change(&c.st, "r", &[("a", Some(b"a1"))]);
    change(&c.st, "other", &[("x", Some(b"x1"))]);
    let b1 = seal(&c, 8);
    change(&c.st, "r", &[("b", Some(b"b2"))]);
    let b2 = seal(&c, 8);
    change(&c.st, "r", &[("c", Some(b"c3")), ("a", None)]);
    change(&c.st, "other", &[("y", Some(b"y3"))]);
    let b3 = seal(&c, 8);

    deliver(&b1, &vault);
    vault.import_ready().unwrap();
    let after_b1 = head_view(&v.st, "r");

    // Bundle 2 is lost. Bundle 3 still imports.
    deliver(&b3, &vault);
    let rep = vault.import_ready().unwrap();
    let r = rep[0].repos.iter().find(|r| r.repo == "r").unwrap();
    assert_eq!((r.state, r.applied_seq), (ApplyState::Incomplete, 1), "{}", r.detail);
    assert_eq!(head_view(&v.st, "r"), after_b1, "repo stays exactly at its old state");
    // The repo that was not touched by the lost bundle moved on.
    let other = rep[0].repos.iter().find(|r| r.repo == "other").unwrap();
    assert_eq!((other.state, other.applied_seq), (ApplyState::Applied, 3));
    assert_eq!(head_view(&v.st, "other"), head_view(&c.st, "other"));

    // Bundle 2 turns up later (re-exported to media): everything catches up.
    deliver(&b2, &vault);
    let rep = vault.import_ready().unwrap();
    let r = rep[0].repos.iter().find(|r| r.repo == "r").unwrap();
    assert_eq!((r.state, r.applied_seq), (ApplyState::Applied, 3));
    assert_eq!(head_view(&v.st, "r"), head_view(&c.st, "r"));

    // Both sides agree on the chained root of the repo.
    let c_root = c.st.meta().read(|t| Ok(t.step_last(&CH, t.repo_require("r")?.id)?.unwrap().root)).unwrap();
    let v_root = v.st.head("r").unwrap().unwrap().chain_root.unwrap();
    assert_eq!(c_root, v_root);
}

#[test]
fn bundles_arriving_in_any_order_converge() {
    let (c, v) = (side(), side());
    let vault = vault_on(&v, false);
    let mut bundles = Vec::new();
    for round in 0u8..5 {
        let name = format!("f{}", round % 3);
        change(&c.st, "r", &[(name.as_str(), Some(&big(round, 3000))), ("always", Some(&[round]))]);
        bundles.push(seal(&c, 8));
    }
    for i in [3usize, 0, 4, 2, 1] {
        deliver(&bundles[i], &vault);
        vault.import_ready().unwrap();
    }
    assert_eq!(head_view(&v.st, "r"), head_view(&c.st, "r"));
    assert_eq!(v.st.meta().read(|t| Ok(t.channel_get(&CH)?.unwrap().imported_seq)).unwrap(), 5);
}

#[test]
fn vault_too_far_behind_waits_for_the_missing_base() {
    let (c, v) = (side(), side());
    let vault = vault_on(&v, false);
    let mut bundles = Vec::new();
    for round in 0u8..3 {
        change(&c.st, "r", &[("f", Some(&[round]))]);
        bundles.push(seal(&c, 1)); // window of 1: each bundle carries only its own step
    }
    deliver(&bundles[2], &vault);
    let rep = vault.import_ready().unwrap();
    assert_eq!(rep[0].repos[0].state, ApplyState::WaitingBase);
    assert!(head_view(&v.st, "r").is_empty());

    deliver(&bundles[0], &vault);
    deliver(&bundles[1], &vault);
    vault.import_ready().unwrap();
    assert_eq!(head_view(&v.st, "r"), head_view(&c.st, "r"));
}

#[test]
fn a_half_received_bundle_survives_a_restart() {
    let (c, v) = (side(), side());
    change(&c.st, "r", &[("a", Some(&big(9, 40_000)))]);
    let b = seal(&c, 8);
    let n = b.params().segment_count;
    assert!(n >= 8);
    let id = {
        let vault = vault_on(&v, false);
        let id = vault.offer_descriptor(b.descriptor_envelope()).unwrap();
        for i in (0..n).step_by(2) {
            assert_eq!(vault.offer_block(&id, i, &b.read_block(i).unwrap()), SlotOutcome::Accepted);
        }
        id
        // process stops here: first USB stick done, second one not inserted yet
    };
    let vault = vault_on(&v, false);
    let info = &vault.staged()[0];
    assert_eq!((info.bundle_id, info.segments_have, info.segments_total), (id, n.div_ceil(2), n));
    assert_eq!(vault.missing(&id).len() as u32, n / 2);
    for i in 0..n {
        let want = if i % 2 == 0 { SlotOutcome::Duplicate } else { SlotOutcome::Accepted };
        assert_eq!(vault.offer_block(&id, i, &b.read_block(i).unwrap()), want);
    }
    vault.import_ready().unwrap();
    assert_eq!(head_view(&v.st, "r"), head_view(&c.st, "r"));
}

#[test]
fn damaged_staging_is_detected_on_restart() {
    let (c, v) = (side(), side());
    change(&c.st, "r", &[("a", Some(&big(9, 20_000)))]);
    let b = seal(&c, 8);
    {
        let vault = vault_on(&v, false);
        deliver(&b, &vault);
    }
    // Flip one byte in the middle of the staged file while the vault is down.
    let dir = std::fs::read_dir(v.dir.path().join("staging").join(CH.to_hex())).unwrap().next().unwrap().unwrap().path();
    let mut bytes = std::fs::read(dir.join("slots.part")).unwrap();
    let slot = b.params().slot_size as usize;
    bytes[2 * slot + 500] ^= 0xFF;
    std::fs::write(dir.join("slots.part"), bytes).unwrap();

    let vault = vault_on(&v, false);
    let id = b.id();
    assert_eq!(vault.missing(&id), vec![2..3], "exactly the damaged segment must be re-sent");
    assert!(vault.import_ready().unwrap().is_empty());
    assert_eq!(vault.offer_block(&id, 2, &b.read_block(2).unwrap()), SlotOutcome::Accepted);
    assert_eq!(vault.import_ready().unwrap().len(), 1);
    assert_eq!(head_view(&v.st, "r"), head_view(&c.st, "r"));
}

#[test]
fn sealed_bundle_can_be_reopened_for_re_export() {
    let (c, v) = (side(), side());
    change(&c.st, "r", &[("a", Some(&big(4, 30_000)))]);
    let sealed = seal(&c, 8);
    let reopened = SealedBundle::open(&c.st, CH, 1).unwrap();
    assert_eq!(reopened.descriptor(), sealed.descriptor());
    for i in 0..sealed.params().segment_count {
        assert_eq!(reopened.read_block(i).unwrap(), sealed.read_block(i).unwrap());
    }
    assert!(SealedBundle::open(&c.st, CH, 2).is_err());
    let vault = vault_on(&v, false);
    deliver(&reopened, &vault);
    vault.import_ready().unwrap();
    assert_eq!(head_view(&v.st, "r"), head_view(&c.st, "r"));
}

#[test]
fn vault_quota_blocks_one_repo_without_stalling_the_channel() {
    let (c, v) = (side(), side());
    let vault = vault_on(&v, false);
    change(&c.st, "small", &[("a", Some(&big(1, 100)))]);
    change(&c.st, "other", &[("x", Some(b"x"))]);
    let b1 = seal(&c, 8);
    deliver(&b1, &vault);
    vault.import_ready().unwrap();
    let id = v.st.repo("small").unwrap().id;
    v.st.meta().write(|t| t.repo_set_quota(id, Some(1000))).unwrap();

    change(&c.st, "small", &[("huge", Some(&big(2, 5000)))]);
    change(&c.st, "other", &[("y", Some(b"y"))]);
    let b2 = seal(&c, 8);
    deliver(&b2, &vault);
    let rep = vault.import_ready().unwrap();
    let small = rep[0].repos.iter().find(|r| r.repo == "small").unwrap();
    assert_eq!((small.state, small.applied_seq), (ApplyState::QuotaBlocked, 1));
    assert_eq!(rep[0].repos.iter().find(|r| r.repo == "other").unwrap().state, ApplyState::Applied);

    // Raising the quota and retrying is enough; nothing has to be re-sent.
    v.st.meta().write(|t| t.repo_set_quota(id, Some(100_000))).unwrap();
    let rep = vault.apply_pending(&CH).unwrap();
    assert_eq!(rep.iter().find(|r| r.repo == "small").unwrap().state, ApplyState::Applied);
    assert_eq!(head_view(&v.st, "small"), head_view(&c.st, "small"));
}

/// Random multi-repo history delivered in a random order: the vault must end
/// up identical to the collector.
#[test]
fn random_histories_always_converge() {
    for seed in 1u64..=6 {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut next = move |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        let (c, v) = (side(), side());
        let vault = vault_on(&v, false);
        let repos = ["alpha", "beta", "gamma"];
        let mut bundles = Vec::new();
        for _ in 0..8 {
            for repo in repos {
                if next(3) == 0 {
                    continue;
                }
                let mut files: Vec<(String, Option<Vec<u8>>)> = Vec::new();
                for _ in 0..(1 + next(4)) {
                    let path = format!("d{}/f{}", next(2), next(5));
                    if files.iter().any(|(p, _)| *p == path) {
                        continue;
                    }
                    let body = (next(4) != 0).then(|| big(next(6) as u8, 1 + next(6000) as usize));
                    files.push((path, body));
                }
                let borrowed: Vec<(&str, Option<&[u8]>)> =
                    files.iter().map(|(p, b)| (p.as_str(), b.as_deref())).collect();
                change(&c.st, repo, &borrowed);
            }
            if let Some(b) = seal_bundle(&c.st, CH, &[&key()], &opts(8)).unwrap() {
                bundles.push(b);
            }
        }
        // Shuffle the delivery order.
        let mut order: Vec<usize> = (0..bundles.len()).collect();
        for i in (1..order.len()).rev() {
            order.swap(i, next(i as u64 + 1) as usize);
        }
        for &i in &order {
            deliver(&bundles[i], &vault);
            vault.import_ready().unwrap();
        }
        for repo in repos {
            assert_eq!(head_view(&v.st, repo), head_view(&c.st, repo), "seed {seed} repo {repo}");
        }
        assert!(v.st.fsck(true).unwrap().is_clean());
    }
}
