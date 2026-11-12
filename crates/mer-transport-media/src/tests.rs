use crate::*;
use mer_bundle::{seal_bundle, ImportPolicy, SealOptions, SealedBundle, Vault};
use mer_core::{ChannelId, EntryRole};
use mer_crypto::{KeyPair, TrustStore};
use mer_storage::Storage;
use std::sync::Arc;

const CH: ChannelId = ChannelId([3; 8]);

struct Rig {
    _dirs: Vec<tempfile::TempDir>,
    collector: Arc<Storage>,
    vault: Vault,
    bundle: SealedBundle,
    media: tempfile::TempDir,
    content: Vec<u8>,
}

/// A collector with one 200 KB file, sealed into 4 KiB segments, and an empty vault.
fn rig() -> Rig {
    let (cd, vd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let collector = Arc::new(Storage::open(cd.path()).unwrap());
    let key = KeyPair::from_seed([5; 32]);
    let content: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
    collector.create_repo("r", "raw").unwrap();
    let (d, _) = collector.ingest_bytes(&content).unwrap();
    let mut w = collector.begin("r").unwrap();
    w.put("big.bin", EntryRole::Artifact, &d).unwrap();
    w.seal(None).unwrap();
    let opts = SealOptions { segment_size: 4096, step_window: 8, collector: "t".into() };
    let bundle = seal_bundle(&collector, CH, &[&key], &opts).unwrap().unwrap();
    let vault = Vault::new(
        Arc::new(Storage::open(vd.path()).unwrap()),
        TrustStore::new(vec![key.public()], 1),
        ImportPolicy::default(),
    )
    .unwrap();
    Rig { _dirs: vec![cd, vd], collector, vault, bundle, media: tempfile::tempdir().unwrap(), content }
}

fn imported_content(r: &Rig) -> Vec<u8> {
    assert_eq!(r.vault.import_ready().unwrap().len(), 1);
    let st = r.vault.storage();
    let seq = st.head("r").unwrap().unwrap().seq;
    let e = st.get_at("r", seq, "big.bin").unwrap().unwrap();
    st.read_blob(&e.digest).unwrap()
}

#[test]
fn bundle_is_split_to_the_medium_size_and_reassembled() {
    let r = rig();
    let volume_size = 64 * 1024;
    let vols = write_volumes(&r.bundle, r.media.path(), volume_size).unwrap();
    assert!(vols.len() >= 4, "{} volumes", vols.len());
    for v in &vols {
        assert!(std::fs::metadata(v).unwrap().len() <= volume_size);
    }
    let name = vols[0].file_name().unwrap().to_str().unwrap().to_string();
    assert_eq!(name, format!("{}-e1-s0000000001-d0000.mab", CH.to_hex()));
    assert!(std::fs::read_to_string(r.media.path().join("CONTENTS.txt")).unwrap().starts_with("UNAUTHENTICATED"));
    assert!(!r.media.path().read_dir().unwrap().any(|e| e.unwrap().path().extension().unwrap() == "part"));

    let rep = import_dir(r.media.path(), &r.vault).unwrap();
    assert_eq!(rep.volumes_read as usize, vols.len());
    assert_eq!(rep.slots_accepted, r.bundle.params().segment_count as u64);
    assert_eq!(rep.bundles, vec![(r.bundle.id(), vec![])]);
    assert_eq!(imported_content(&r), r.content);
    let _ = &r.collector;
}

#[test]
fn one_medium_at_a_time_in_any_order_with_a_clear_missing_report() {
    let r = rig();
    let vols = write_volumes(&r.bundle, r.media.path(), 64 * 1024).unwrap();
    // Each "stick" holds one volume. Insert them last to first, skipping one.
    let lost = 1;
    let mut missing_after = Vec::new();
    for (i, vol) in vols.iter().enumerate().rev() {
        if i == lost {
            continue;
        }
        let stick = tempfile::tempdir().unwrap();
        // The importer must not depend on file names.
        std::fs::copy(vol, stick.path().join(format!("renamed-{i}.mab"))).unwrap();
        let rep = import_dir(stick.path(), &r.vault).unwrap();
        missing_after = rep.bundles[0].1.clone();
    }
    assert_eq!(missing_after.len(), 1, "one contiguous range is missing: {missing_after:?}");
    assert!(r.vault.import_ready().unwrap().is_empty(), "incomplete bundle must not import");

    // The missing volume is carried over again.
    let stick = tempfile::tempdir().unwrap();
    std::fs::copy(&vols[lost], stick.path().join("again.mab")).unwrap();
    let rep = import_dir(stick.path(), &r.vault).unwrap();
    assert!(rep.bundles[0].1.is_empty());
    assert_eq!(imported_content(&r), r.content);
}

#[test]
fn a_bad_sector_costs_only_the_slots_it_touches() {
    let r = rig();
    let vols = write_volumes(&r.bundle, r.media.path(), 1 << 30).unwrap();
    assert_eq!(vols.len(), 1);
    let slot = r.bundle.params().slot_size as usize;
    let mut bytes = std::fs::read(&vols[0]).unwrap();
    let first_slot = 4096 + 4096; // header + padded descriptor
    for b in &mut bytes[first_slot + 5 * slot + 100..first_slot + 5 * slot + 600] {
        *b = 0xEE; // 500 unreadable bytes inside segment 5
    }
    let damaged = tempfile::tempdir().unwrap();
    std::fs::write(damaged.path().join("v.mab"), &bytes).unwrap();

    let rep = import_dir(damaged.path(), &r.vault).unwrap();
    assert_eq!(rep.slots_damaged, 1);
    assert_eq!(rep.bundles[0].1, vec![5..6]);

    // A second, clean copy only needs to supply that one segment.
    let rep = import_dir(r.media.path(), &r.vault).unwrap();
    assert_eq!((rep.slots_accepted, rep.slots_damaged), (1, 0));
    assert_eq!(rep.slots_already_had, r.bundle.params().segment_count as u64 - 1);
    assert_eq!(imported_content(&r), r.content);
}

#[test]
fn truncated_volume_gives_what_it_has() {
    let r = rig();
    let vols = write_volumes(&r.bundle, r.media.path(), 1 << 30).unwrap();
    let bytes = std::fs::read(&vols[0]).unwrap();
    let cut = tempfile::tempdir().unwrap();
    std::fs::write(cut.path().join("v.mab"), &bytes[..bytes.len() / 2]).unwrap();
    let rep = import_dir(cut.path(), &r.vault).unwrap();
    let n = r.bundle.params().segment_count as u64;
    assert!(rep.slots_accepted > n / 3 && rep.slots_accepted < n, "{} of {n}", rep.slots_accepted);
    assert_eq!(rep.bundles[0].1.len(), 1, "the tail is missing as one range");
}

#[test]
fn junk_foreign_and_lying_files_are_skipped_safely() {
    let r = rig();
    let vols = write_volumes(&r.bundle, r.media.path(), 1 << 30).unwrap();
    let good = std::fs::read(&vols[0]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let p = |n: &str| dir.path().join(n);

    std::fs::write(p("empty.mab"), b"").unwrap();
    std::fs::write(p("junk.mab"), vec![0x41u8; 10_000]).unwrap();
    std::fs::write(p("notes.txt"), b"ignored: wrong extension").unwrap();
    std::fs::create_dir(p("dir.mab")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&vols[0], p("link.mab")).unwrap();

    // Header with a valid checksum that lies about the slot size.
    let mut lying = good.clone();
    lying[76..80].copy_from_slice(&(1u32 << 20).to_be_bytes());
    let crc = crc32c::crc32c(&lying[..4092]);
    lying[4092..4096].copy_from_slice(&crc.to_be_bytes());
    std::fs::write(p("lying.mab"), &lying).unwrap();

    // A volume whose descriptor signature has been altered.
    let mut forged = good.clone();
    forged[4096 + 60] ^= 1;
    std::fs::write(p("forged.mab"), &forged).unwrap();

    // Claims an absurd descriptor length.
    let mut huge = good.clone();
    huge[84..88].copy_from_slice(&u32::MAX.to_be_bytes());
    let crc = crc32c::crc32c(&huge[..4092]);
    huge[4092..4096].copy_from_slice(&crc.to_be_bytes());
    std::fs::write(p("huge.mab"), &huge).unwrap();

    let rep = import_dir(dir.path(), &r.vault).unwrap();
    assert_eq!(rep.volumes_read, 0);
    assert_eq!(rep.slots_accepted, 0);
    let skipped: Vec<String> =
        rep.skipped.iter().map(|(p, _)| p.file_name().unwrap().to_str().unwrap().to_string()).collect();
    for name in ["empty.mab", "junk.mab", "dir.mab", "lying.mab", "forged.mab", "huge.mab"] {
        assert!(skipped.contains(&name.to_string()), "{name} should be skipped: {:?}", rep.skipped);
    }
    #[cfg(unix)]
    assert!(skipped.contains(&"link.mab".to_string()), "symlinks are not followed");
    assert!(!skipped.contains(&"notes.txt".to_string()));
}

#[test]
fn volume_smaller_than_one_slot_is_a_clear_error() {
    let r = rig();
    match write_volumes(&r.bundle, r.media.path(), 10_000) {
        Err(MediaError::VolumeTooSmall { needed, .. }) => assert_eq!(needed, 4096 + 4096 + 4096 + 8192),
        other => panic!("expected VolumeTooSmall, got {other:?}"),
    }
}
