use crate::*;
use mer_core::*;
use mer_crypto::{KeyPair, TrustStore};

fn rec(path: &str, body: &[u8]) -> EntryRec {
    EntryRec { path: path.into(), role: EntryRole::Artifact, digest: Digest::of(body), size: body.len() as u64 }
}

/// Cut a stream into segments, build the tree and a signed descriptor.
fn bundle(stream: &[u8], segment_size: u32, key: &KeyPair) -> (Descriptor, Vec<u8>, BundleParams, Vec<Vec<u8>>) {
    let params = BundleParams::new(stream.len() as u64, segment_size).unwrap();
    let segs: Vec<&[u8]> = stream.chunks(segment_size as usize).collect();
    let tree = MerkleTree::from_leaves(
        segs.iter().enumerate().map(|(i, s)| leaf_hash(i as u32, s)).collect(),
    );
    let manifest_sha256 = Digest::of(b"manifest");
    let index_sha256 = Digest::of(b"index");
    let desc = Descriptor {
        format_version: FORMAT_VERSION,
        channel: ChannelId([1; 8]),
        epoch: 1,
        seq: 1,
        bundle_id: BundleId::compute(&manifest_sha256, &index_sha256),
        created_at: 0,
        stream_len: stream.len() as u64,
        segment_size,
        segment_count: params.segment_count,
        merkle_root: tree.root(),
        manifest_len: 8,
        manifest_sha256,
        index_len: 0,
        index_sha256,
    };
    let blocks = segs
        .iter()
        .enumerate()
        .map(|(i, s)| encode_block(i as u32, &desc.bundle_id, &tree.proof(i as u32), s))
        .collect();
    let env = sign_descriptor(&desc, &[key]);
    (desc, env, params, blocks)
}

fn stream(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[test]
fn cbor_round_trips_and_rejects_trailing_bytes() {
    let ops = vec![EntryOp::Put(rec("a/b", b"1")), EntryOp::Del { path: "c".into() }];
    let bytes = encode_ops(&ops);
    assert_eq!(decode_ops(&bytes).unwrap(), ops);
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(decode_ops(&longer).is_err());
    assert!(decode_ops(&bytes[..bytes.len() - 1]).is_err());
}

#[test]
fn digests_are_cbor_byte_strings() {
    // 0x58 0x20 = byte string of length 32: compact and unambiguous on the wire.
    let bytes = to_cbor(&Digest::of(b"x"));
    assert_eq!(bytes.len(), 34);
    assert_eq!(&bytes[..2], &[0x58, 0x20]);
    assert_eq!(from_cbor::<Digest>(&bytes).unwrap(), Digest::of(b"x"));
    assert!(from_cbor::<Digest>(&to_cbor(&vec![0u8; 31])).is_err());
}

#[test]
fn ops_with_unsafe_or_unsorted_paths_are_refused() {
    let traversal = to_cbor(&vec![EntryOp::Put(rec("../../etc/passwd", b"x"))]);
    assert!(decode_ops(&traversal).is_err());
    let unsorted = to_cbor(&vec![EntryOp::Del { path: "b".into() }, EntryOp::Del { path: "a".into() }]);
    assert!(decode_ops(&unsorted).is_err());
    let twice = to_cbor(&vec![EntryOp::Del { path: "a".into() }, EntryOp::Put(rec("a", b"x"))]);
    assert!(decode_ops(&twice).is_err());
}

#[test]
fn deeply_nested_cbor_is_refused_not_a_stack_overflow() {
    let mut bomb = vec![0x81u8; 100_000]; // 100k nested one-element arrays
    bomb.push(0x00);
    assert!(from_cbor::<Vec<EntryOp>>(&bomb).is_err());
}

fn manifest() -> Manifest {
    let g = SnapshotId::genesis();
    let o1 = Digest::of(b"ops1");
    let o2 = Digest::of(b"ops2");
    let r1 = SnapshotId::step(&g, &o1);
    Manifest {
        v: 1,
        channel: ChannelId([1; 8]),
        epoch: 1,
        seq: 5,
        created_at: 0,
        collector: "c".into(),
        repos: vec![RepoDelta {
            repo: "debian".into(),
            format: "raw".into(),
            steps: vec![
                Step { seq: 3, prev_seq: 0, prev_root: g, ops: o1, op_count: 1, added_bytes: 1, root: r1 },
                Step { seq: 5, prev_seq: 3, prev_root: r1, ops: o2, op_count: 1, added_bytes: 1, root: SnapshotId::step(&r1, &o2) },
            ],
        }],
        blob_count: 2,
        payload_bytes: 2,
    }
}

#[test]
fn manifest_round_trips_and_its_step_chain_is_checked() {
    let m = manifest();
    assert_eq!(decode_manifest(&to_cbor(&m)).unwrap(), m);

    let mut forged_root = m.clone();
    forged_root.repos[0].steps[1].root = SnapshotId::genesis();
    assert!(decode_manifest(&to_cbor(&forged_root)).is_err());

    let mut broken_chain = m.clone();
    broken_chain.repos[0].steps.remove(0);
    broken_chain.repos[0].steps.insert(
        0,
        Step {
            seq: 2,
            prev_seq: 0,
            prev_root: SnapshotId::genesis(),
            ops: Digest::of(b"other"),
            op_count: 0,
            added_bytes: 0,
            root: SnapshotId::step(&SnapshotId::genesis(), &Digest::of(b"other")),
        },
    );
    assert!(decode_manifest(&to_cbor(&broken_chain)).is_err());

    let mut bad_name = m.clone();
    bad_name.repos[0].repo = "../vault".into();
    assert!(decode_manifest(&to_cbor(&bad_name)).is_err());

    let mut future = m.clone();
    future.v = 2;
    assert!(matches!(decode_manifest(&to_cbor(&future)), Err(FormatError::Unsupported(_))));

    let mut from_the_future = m;
    from_the_future.repos[0].steps[1].seq = 6; // step newer than the bundle carrying it
    assert!(decode_manifest(&to_cbor(&from_the_future)).is_err());
}

#[test]
fn blob_index_round_trips_and_must_fill_the_payload_exactly() {
    let recs = vec![
        IndexRec { digest: Digest::of(b"ops"), len: 10, class: BlobClass::Meta },
        IndexRec { digest: Digest::of(b"deb"), len: 5000, class: BlobClass::Artifact },
    ];
    let bytes = encode_index(&recs);
    assert_eq!(bytes.len(), 112);
    assert_eq!(decode_index(&bytes, 5010).unwrap(), recs);
    assert!(decode_index(&bytes, 5011).is_err());
    assert!(decode_index(&bytes[..100], 5010).is_err());
    let twice = encode_index(&[recs[0], recs[0]]);
    assert!(decode_index(&twice, 20).is_err());
    let mut overflow = encode_index(&[recs[0], recs[1]]);
    overflow[32..40].copy_from_slice(&u64::MAX.to_be_bytes());
    overflow[40..48].copy_from_slice(&u64::MAX.to_be_bytes());
    assert!(decode_index(&overflow, 5010).is_err());
}

#[test]
fn merkle_proofs_verify_for_every_leaf_count() {
    for n in [1u32, 2, 3, 4, 5, 7, 8, 9, 33] {
        let leaves: Vec<Digest> = (0..n).map(|i| leaf_hash(i, &i.to_be_bytes())).collect();
        let tree = MerkleTree::from_leaves(leaves.clone());
        assert_eq!(tree.depth(), depth_for(n));
        for i in 0..n {
            let proof = tree.proof(i);
            assert!(verify_proof(&tree.root(), i, leaves[i as usize], &proof), "n={n} i={i}");
            // The same leaf is not valid at another position.
            assert!(!verify_proof(&tree.root(), (i + 1) % (1 << tree.depth()), leaves[i as usize], &proof));
            let mut bad = proof.clone();
            bad[0].0[0] ^= 1;
            assert!(!verify_proof(&tree.root(), i, leaves[i as usize], &bad));
        }
        assert!(!verify_proof(&tree.root(), 1 << tree.depth(), leaves[0], &tree.proof(0)));
    }
    assert_eq!((depth_for(1), depth_for(2), depth_for(3), depth_for(1024), depth_for(1025)), (1, 1, 2, 10, 11));
}

#[test]
fn geometry_of_a_bundle() {
    let p = BundleParams::new(10_000, 4096).unwrap();
    assert_eq!((p.segment_count, p.depth), (3, 2));
    assert_eq!((p.segment_len(0), p.segment_len(2)), (4096, 1808));
    assert_eq!(p.segment_range(2), 8192..10_000);
    assert_eq!(p.block_len(0), 32 + 64 + 4096);
    assert_eq!(p.slot_size, 8192);
    assert!(BundleParams::new(10, 1000).is_err(), "not a power of two");
    assert!(BundleParams::new(10, 1024).is_err(), "below minimum");
    assert!(BundleParams::new(0, 4096).is_err());
    assert!(BundleParams::new(u64::MAX, 4096).is_err(), "segment count overflow");
}

#[test]
fn every_block_verifies_alone_and_in_any_order() {
    let key = KeyPair::from_seed([1; 32]);
    let s = stream(20_000);
    let (desc, _env, params, blocks) = bundle(&s, 4096, &key);
    let mut rebuilt = vec![0u8; s.len()];
    for i in (0..params.segment_count).rev() {
        let data = verify_block(&desc, &params, i, &blocks[i as usize]).unwrap();
        let r = params.segment_range(i);
        rebuilt[r.start as usize..r.end as usize].copy_from_slice(data);
    }
    assert_eq!(rebuilt, s);
}

#[test]
fn any_corruption_or_misplacement_of_a_block_is_caught() {
    let key = KeyPair::from_seed([1; 32]);
    let (desc, _env, params, blocks) = bundle(&stream(20_000), 4096, &key);
    let good = &blocks[1];
    for i in 0..good.len() {
        let mut bad = good.clone();
        bad[i] ^= 0x80;
        assert!(verify_block(&desc, &params, 1, &bad).is_err(), "flip at {i}");
    }
    assert!(verify_block(&desc, &params, 2, good).is_err(), "offered at the wrong index");
    assert!(verify_block(&desc, &params, 99, good).is_err());
    assert!(verify_block(&desc, &params, 1, &good[..good.len() - 1]).is_err());

    // A block of a different bundle with the same geometry is refused.
    let (_d2, _e2, _p2, other) = bundle(&vec![9u8; 20_000], 4096, &key);
    assert!(verify_block(&desc, &params, 1, &other[1]).is_err());
}

#[test]
fn slots_detect_media_damage() {
    let key = KeyPair::from_seed([1; 32]);
    let (desc, _env, params, blocks) = bundle(&stream(10_000), 4096, &key);
    for (i, block) in blocks.iter().enumerate() {
        let slot = pad_slot(block, params.slot_size);
        assert_eq!(slot.len(), params.slot_size as usize);
        let back = unpad_slot(&slot, &params, i as u32).unwrap();
        assert_eq!(back, &block[..]);
        verify_block(&desc, &params, i as u32, back).unwrap();
        let mut damaged = slot.clone();
        damaged[100] ^= 1;
        assert!(unpad_slot(&damaged, &params, i as u32).is_err());
    }
}

#[test]
fn descriptor_needs_a_trusted_signature_and_consistent_fields() {
    let key = KeyPair::from_seed([1; 32]);
    let trust = TrustStore::new(vec![key.public()], 1);
    let (desc, env, _p, _b) = bundle(&stream(10_000), 4096, &key);
    assert_eq!(open_descriptor(&env, &trust).unwrap(), desc);

    let stranger = TrustStore::new(vec![KeyPair::from_seed([2; 32]).public()], 1);
    assert!(matches!(open_descriptor(&env, &stranger), Err(FormatError::Crypto(_))));

    // Correctly signed by a trusted key, but internally inconsistent.
    let resign = |f: &dyn Fn(&mut Descriptor)| {
        let mut d = desc.clone();
        f(&mut d);
        open_descriptor(&sign_descriptor(&d, &[&key]), &trust)
    };
    assert!(resign(&|d| d.segment_count += 1).is_err());
    assert!(resign(&|d| d.bundle_id = BundleId([0; 32])).is_err());
    assert!(resign(&|d| d.manifest_len = d.stream_len + 1).is_err());
    assert!(resign(&|d| d.seq = 0).is_err());
    assert!(resign(&|d| d.segment_size = 3000).is_err());
    assert!(matches!(resign(&|d| d.format_version = 2), Err(FormatError::Unsupported(_))));
}
