use crate::*;
use mer_core::EntryRole::Artifact;

fn open() -> (tempfile::TempDir, Storage) {
    let dir = tempfile::tempdir().unwrap();
    let st = Storage::open(dir.path()).unwrap();
    st.create_repo("files", "raw").unwrap();
    (dir, st)
}

/// Ingest and snapshot a set of files; returns the sealed seq.
fn commit(st: &Storage, files: &[(&str, &[u8])]) -> u64 {
    let mut w = st.begin("files").unwrap();
    for (path, body) in files {
        let (d, _) = st.ingest_bytes(body).unwrap();
        w.put(path, Artifact, &d).unwrap();
    }
    w.seal(None).unwrap().expect("snapshot has changes").seq
}

fn body(st: &Storage, path: &str) -> Option<Vec<u8>> {
    match st.get("files", path).unwrap() {
        Lookup::Found(e) => Some(st.read_blob(&e.digest).unwrap()),
        _ => None,
    }
}

#[test]
fn nothing_is_served_until_approved_and_published() {
    let (_d, st) = open();
    let s1 = commit(&st, &[("a.txt", b"one")]);
    assert_eq!(st.get("files", "a.txt").unwrap(), Lookup::NotPublished);
    assert!(st.publish("files", s1, "alice").is_err(), "sealed but not approved");
    st.decide("files", s1, true, "alice").unwrap();
    st.publish("files", s1, "alice").unwrap();
    assert_eq!(body(&st, "a.txt").unwrap(), b"one");
}

#[test]
fn publish_is_atomic_and_rollback_restores_the_old_view() {
    let (_d, st) = open();
    let s1 = commit(&st, &[("a.txt", b"one"), ("b.txt", b"bee")]);
    st.decide("files", s1, true, "alice").unwrap();
    st.publish("files", s1, "alice").unwrap();

    let mut w = st.begin("files").unwrap();
    let (d, _) = st.ingest_bytes(b"two").unwrap();
    w.put("a.txt", Artifact, &d).unwrap();
    w.remove("b.txt").unwrap();
    let s2 = w.seal(None).unwrap().unwrap().seq;

    // Sealed but not published: clients still see snapshot 1 in full.
    assert_eq!(body(&st, "a.txt").unwrap(), b"one");
    assert_eq!(body(&st, "b.txt").unwrap(), b"bee");

    st.decide("files", s2, true, "alice").unwrap();
    st.publish("files", s2, "alice").unwrap();
    assert_eq!(body(&st, "a.txt").unwrap(), b"two");
    assert!(body(&st, "b.txt").is_none());

    st.rollback("files", s1, "bob").unwrap();
    assert_eq!(body(&st, "a.txt").unwrap(), b"one");
    assert_eq!(body(&st, "b.txt").unwrap(), b"bee");
}

#[test]
fn misses_feed_the_wishlist() {
    let (_d, st) = open();
    let s1 = commit(&st, &[("a.txt", b"one")]);
    st.decide("files", s1, true, "a").unwrap();
    st.publish("files", s1, "a").unwrap();
    assert_eq!(st.get("files", "wanted-1.2.3.tar.gz").unwrap(), Lookup::Miss);
    assert_eq!(st.get("files", "wanted-1.2.3.tar.gz").unwrap(), Lookup::Miss);
    assert_eq!(st.get("files", "../../etc/shadow").unwrap(), Lookup::Miss);
    let misses = st.meta().read(|t| t.miss_list(10)).unwrap();
    assert_eq!(misses.len(), 1, "unsafe paths are not recorded");
    assert_eq!((misses[0].path.as_str(), misses[0].count), ("wanted-1.2.3.tar.gz", 2));
}

#[test]
fn unchanged_snapshot_is_not_created() {
    let (_d, st) = open();
    commit(&st, &[("a.txt", b"one")]);
    let mut w = st.begin("files").unwrap();
    let (d, _) = st.ingest_bytes(b"one").unwrap();
    w.put("a.txt", Artifact, &d).unwrap();
    assert!(w.seal(None).unwrap().is_none());
    assert_eq!(st.head("files").unwrap().unwrap().seq, 1);
}

#[test]
fn dropped_writer_rolls_back() {
    let (_d, st) = open();
    commit(&st, &[("a.txt", b"one")]);
    {
        let mut w = st.begin("files").unwrap();
        let (d, _) = st.ingest_bytes(b"junk").unwrap();
        w.put("a.txt", Artifact, &d).unwrap();
        w.put("b.txt", Artifact, &d).unwrap();
        // dropped without seal: simulates an error path
    }
    let s = commit(&st, &[("c.txt", b"sea")]);
    let e = st.get_at("files", s, "a.txt").unwrap().unwrap();
    assert_eq!(st.read_blob(&e.digest).unwrap(), b"one");
    assert!(st.get_at("files", s, "b.txt").unwrap().is_none());
}

#[test]
fn large_snapshot_crosses_batch_boundaries() {
    let (_d, st) = open();
    let (d, _) = st.ingest_bytes(b"shared").unwrap();
    let mut w = st.begin("files").unwrap();
    for i in 0..2500 {
        w.put(&format!("pool/{i:05}"), Artifact, &d).unwrap();
    }
    let snap = w.seal(None).unwrap().unwrap();
    assert_eq!(snap.entry_count, 2500);
    assert_eq!(st.meta().read(|t| t.blob_count()).unwrap().0, 1, "one blob behind 2500 paths");
}

#[test]
fn gc_frees_space_only_for_unreachable_content() {
    let (_d, st) = open();
    let s1 = commit(&st, &[("big", &[1u8; 5000]), ("keep", b"keep")]);
    let s2 = commit(&st, &[("big", &[2u8; 6000])]);
    st.decide("files", s2, true, "a").unwrap();
    st.publish("files", s2, "a").unwrap();
    let old = mer_core::Digest::of(&[1u8; 5000]);

    // Snapshot 1 is still retained, so its content is reachable.
    assert_eq!(st.gc(0).unwrap().deleted, 0);

    st.compact("files", s2, "a").unwrap();
    assert!(st.get_at("files", s1, "big").is_err(), "retired snapshot is gone");

    // Inside the grace period nothing is deleted.
    assert_eq!(st.gc(60_000).unwrap(), GcReport { orphaned: 1, deleted: 0, bytes_freed: 0 });
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert_eq!(st.gc(1).unwrap(), GcReport { orphaned: 1, deleted: 1, bytes_freed: 5000 });

    assert!(st.blobs().stat(&old).unwrap().is_none());
    assert!(!st.has_blob(&old).unwrap());
    assert_eq!(body(&st, "big").unwrap(), vec![2u8; 6000]);
    assert_eq!(body(&st, "keep").unwrap(), b"keep");
    assert!(st.fsck(true).unwrap().is_clean());
}

#[test]
fn crash_between_gc_claim_and_delete_is_finished_on_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (d, _) = {
        let st = Storage::open(dir.path()).unwrap();
        let out = st.ingest_bytes(b"orphan").unwrap();
        st.meta()
            .write(|t| {
                t.gc_mark(10)?;
                assert_eq!(t.gc_claim(10, 10)?.len(), 1);
                Ok(())
            })
            .unwrap();
        out
        // "crash": the row is `deleting`, the bytes are still on disk
    };
    let st = Storage::open(dir.path()).unwrap();
    assert!(st.blobs().stat(&d).unwrap().is_none());
    assert!(st.meta().read(|t| t.blob_get(&d)).unwrap().is_none());
    // And the content can be ingested again afterwards.
    assert_eq!(st.ingest_bytes(b"orphan").unwrap().0, d);
    assert!(st.has_blob(&d).unwrap());
}

#[test]
fn quota_blocks_the_snapshot_that_would_exceed_it() {
    let (_d, st) = open();
    let id = st.repo("files").unwrap().id;
    st.meta().write(|t| t.repo_set_quota(id, Some(100))).unwrap();
    commit(&st, &[("a", &[0u8; 60])]);
    let mut w = st.begin("files").unwrap();
    let (d, _) = st.ingest_bytes(&[1u8; 60]).unwrap();
    w.put("b", Artifact, &d).unwrap();
    match w.seal(None) {
        Err(StorageError::QuotaExceeded { usage: 120, quota: 100, .. }) => {}
        other => panic!("expected quota error, got {other:?}"),
    }
    assert_eq!(st.head("files").unwrap().unwrap().seq, 1, "repo stays at its old head");
    // Replacing content within quota still works.
    let mut w = st.begin("files").unwrap();
    w.remove("a").unwrap();
    w.put("b", Artifact, &d).unwrap();
    assert!(w.seal(None).unwrap().is_some());
}

#[test]
fn fsck_finds_bit_rot_and_lost_files() {
    let (dir, st) = open();
    commit(&st, &[("a", b"aaaa"), ("b", b"bbbb")]);
    assert!(st.fsck(true).unwrap().is_clean());

    let path_of = |d: &mer_core::Digest| {
        let hex = d.to_hex();
        dir.path().join("store/blobs/sha256").join(&hex[0..2]).join(&hex[2..4]).join(&hex)
    };
    let a = mer_core::Digest::of(b"aaaa");
    let b = mer_core::Digest::of(b"bbbb");
    // Same size, different bytes: only a deep check sees it.
    std::fs::remove_file(path_of(&a)).unwrap();
    std::fs::write(path_of(&a), b"aaaX").unwrap();
    std::fs::remove_file(path_of(&b)).unwrap();

    assert!(st.fsck(false).unwrap().corrupt.is_empty());
    let report = st.fsck(true).unwrap();
    assert_eq!(report.corrupt, vec![a]);
    assert_eq!(report.missing, vec![b]);
    assert!(!report.is_clean());
}

#[test]
fn data_directory_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let st = Storage::open(dir.path()).unwrap();
        st.create_repo("files", "raw").unwrap();
        let s = commit(&st, &[("a.txt", b"persist")]);
        st.decide("files", s, true, "a").unwrap();
        st.publish("files", s, "a").unwrap();
    }
    let st = Storage::open(dir.path()).unwrap();
    assert_eq!(body(&st, "a.txt").unwrap(), b"persist");
    assert_eq!(st.meta().read(|t| t.audit_verify()).unwrap(), Ok(4));
}
