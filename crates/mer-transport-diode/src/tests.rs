use crate::*;
use mer_bundle::{seal_bundle, ImportPolicy, SealOptions, SealedBundle, Vault};
use mer_bundle_format::{SegmentSink, SegmentSource};
use mer_core::{ChannelId, EntryRole};
use mer_crypto::{KeyPair, LinkKey, TrustStore};
use mer_storage::Storage;
use std::sync::Arc;

const CH: ChannelId = ChannelId([9; 8]);

struct Rig {
    _dirs: Vec<tempfile::TempDir>,
    bundle: SealedBundle,
    vault: Arc<Vault>,
    content: Vec<u8>,
}

/// Collector with one file of `len` bytes sealed into 64 KiB segments
/// (about 48 packets per segment), and an empty vault.
fn rig(len: usize) -> Rig {
    let (cd, vd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let collector = Arc::new(Storage::open(cd.path()).unwrap());
    let key = KeyPair::from_seed([5; 32]);
    let content: Vec<u8> = (0..len as u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
    collector.create_repo("r", "raw").unwrap();
    let (d, _) = collector.ingest_bytes(&content).unwrap();
    let mut w = collector.begin("r").unwrap();
    w.put("big.bin", EntryRole::Artifact, &d).unwrap();
    w.seal(None).unwrap();
    let opts = SealOptions { segment_size: 65_536, step_window: 8, collector: "t".into() };
    let bundle = seal_bundle(&collector, CH, &[&key], &opts).unwrap().unwrap();
    let vault = Arc::new(
        Vault::new(
            Arc::new(Storage::open(vd.path()).unwrap()),
            TrustStore::new(vec![key.public()], 1),
            ImportPolicy::default(),
        )
        .unwrap(),
    );
    Rig { _dirs: vec![cd, vd], bundle, vault, content }
}

fn link() -> LinkKey {
    LinkKey::from_bytes([0xAB; 32])
}

fn sender(session: u32) -> Sender<VecOut> {
    Sender::new(VecOut::default(), link(), session, SenderConfig { announce_every: 100, ..Default::default() })
}

fn receiver(r: &Rig) -> Receiver {
    Receiver::new(r.vault.clone(), link(), ReceiverConfig::default())
}

fn pass(s: &mut Sender<VecOut>, r: &Rig, n: u16) -> Vec<Vec<u8>> {
    s.send_pass(&r.bundle, n).unwrap();
    std::mem::take(&mut s.out.0)
}

fn complete(r: &Rig) -> bool {
    r.vault.params(&r.bundle.id()).is_some() && r.vault.missing(&r.bundle.id()).is_empty()
}

fn assert_imported(r: &Rig) {
    assert_eq!(r.vault.import_ready().unwrap().len(), 1);
    let st = r.vault.storage();
    let seq = st.head("r").unwrap().unwrap().seq;
    let e = st.get_at("r", seq, "big.bin").unwrap().unwrap();
    assert_eq!(st.read_blob(&e.digest).unwrap(), r.content);
}

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

#[test]
fn clean_link_needs_one_pass() {
    let r = rig(600_000);
    let mut tx = sender(1);
    let mut rx = receiver(&r);
    let packets = pass(&mut tx, &r, 0);
    assert!(packets.iter().all(|p| p.len() <= 1368 + 68 + 16), "fits a 1500 byte MTU");
    for p in &packets {
        rx.handle(p);
    }
    let s = rx.stats();
    assert_eq!(s.segments_completed, r.bundle.params().segment_count as u64);
    assert_eq!((s.lost, s.bad_mac, s.malformed, s.unknown_bundle), (0, 0, 0, 0));
    assert_eq!(s.passes_ended, 1);
    assert_imported(&r);
}

#[test]
fn random_loss_is_repaired_without_any_feedback() {
    for (loss_percent, max_passes) in [(5u64, 2u16), (20, 3), (40, 5)] {
        let r = rig(600_000);
        let mut tx = sender(1);
        let mut rx = receiver(&r);
        let mut rng = Rng(0x1234_5678_9ABC_DEF1 + loss_percent);
        let mut used = 0;
        for n in 0..max_passes {
            for p in pass(&mut tx, &r, n) {
                if rng.below(100) >= loss_percent {
                    rx.handle(&p);
                }
            }
            used = n + 1;
            if complete(&r) {
                break;
            }
        }
        assert!(complete(&r), "{loss_percent}% loss: not complete after {used} passes");
        assert!(rx.stats().lost > 0);
        assert_imported(&r);
    }
}

#[test]
fn a_long_burst_is_spread_over_segments_and_filled_by_the_next_pass() {
    let r = rig(1_000_000);
    let mut tx = sender(1);
    let mut rx = receiver(&r);
    let p0 = pass(&mut tx, &r, 0);
    // The link is dead for 300 consecutive packets (several whole segments' worth).
    for (i, p) in p0.iter().enumerate() {
        if !(200..500).contains(&i) {
            rx.handle(p);
        }
    }
    assert!(!complete(&r));
    for p in pass(&mut tx, &r, 1) {
        rx.handle(&p);
    }
    assert!(complete(&r));
    assert_eq!(rx.stats().lost, 300);
    assert_imported(&r);
}

#[test]
fn receiver_that_starts_late_catches_up() {
    let r = rig(600_000);
    let mut tx = sender(1);
    let mut rx = receiver(&r);
    // The receiver comes up after the first 150 packets, missing the descriptor.
    let p0 = pass(&mut tx, &r, 0);
    for p in &p0[150..] {
        rx.handle(p);
    }
    let s = rx.stats();
    assert!(s.unknown_bundle > 0, "data before the first descriptor repeat is dropped, not buffered");
    assert_eq!(s.descriptors_accepted, 1, "the descriptor is repeated inside the pass");
    for n in 1..3 {
        for p in pass(&mut tx, &r, n) {
            rx.handle(&p);
        }
        if complete(&r) {
            break;
        }
    }
    assert!(complete(&r));
    assert_imported(&r);
}

#[test]
fn reordered_and_duplicated_packets_are_harmless() {
    let r = rig(600_000);
    let mut tx = sender(1);
    let mut rx = receiver(&r);
    let mut packets = pass(&mut tx, &r, 0);
    let mut rng = Rng(77);
    // Local reordering within a window of 30 packets, as a switch might do.
    for i in 0..packets.len() {
        let j = (i + rng.below(30) as usize).min(packets.len() - 1);
        packets.swap(i, j);
    }
    for p in &packets {
        rx.handle(p);
        rx.handle(p); // every packet delivered twice
    }
    for p in pass(&mut tx, &r, 1) {
        rx.handle(&p);
    }
    assert!(complete(&r));
    assert_eq!(rx.stats().replayed as usize, packets.len());
    assert_imported(&r);
}

#[test]
fn packets_without_the_link_key_cause_no_work() {
    let r = rig(300_000);
    let mut rx = receiver(&r);
    // An attacker on the wire without the link key, replaying a real bundle.
    let mut evil = Sender::new(VecOut::default(), LinkKey::from_bytes([1; 32]), 1, SenderConfig::default());
    evil.send_pass(&r.bundle, 0).unwrap();
    for p in &evil.out.0 {
        rx.handle(p);
    }
    let s = rx.stats();
    assert_eq!(s.bad_mac as usize, evil.out.0.len());
    assert_eq!((s.descriptors_accepted, s.segments_completed), (0, 0));
    assert!(r.vault.staged().is_empty(), "nothing was staged");

    // Corrupted and truncated genuine packets are dropped too.
    let mut tx = sender(1);
    let good = pass(&mut tx, &r, 0);
    let mut flipped = good[1].clone();
    flipped[80] ^= 1;
    rx.handle(&flipped);
    rx.handle(&good[1][..100]); // truncated: the MAC no longer matches
    rx.handle(&good[1][..40]); // shorter than any valid packet
    rx.handle(b"");
    rx.handle(&[0u8; 2000]);
    let s = rx.stats();
    assert_eq!(s.bad_mac as usize, evil.out.0.len() + 2);
    assert_eq!(s.malformed, 3);
    assert_eq!(s.segments_completed, 0);
}

#[test]
fn an_old_session_cannot_be_replayed_into_a_new_one() {
    let r = rig(300_000);
    let mut rx = receiver(&r);
    let mut old = sender(5);
    let old_packets = pass(&mut old, &r, 0);
    for p in &old_packets[..20] {
        rx.handle(p);
    }
    // The sender restarts with a higher session id.
    let mut new = sender(6);
    for p in pass(&mut new, &r, 0) {
        rx.handle(&p);
    }
    assert!(complete(&r));
    let before = rx.stats();
    for p in &old_packets {
        rx.handle(p); // recorded traffic from the old session
    }
    let after = rx.stats();
    assert_eq!(after.stale_session - before.stale_session, old_packets.len() as u64);
    assert_eq!(after.segments_completed, before.segments_completed);
    assert_imported(&r);
}

#[test]
fn memory_stays_bounded_when_many_segments_are_open() {
    let r = rig(2_000_000);
    let mut tx = Sender::new(
        VecOut::default(),
        link(),
        1,
        SenderConfig { interleave: 32, announce_every: 100, ..Default::default() },
    );
    let cfg = ReceiverConfig { max_open_blocks: 4, ..Default::default() };
    let mut rx = Receiver::new(r.vault.clone(), link(), cfg);
    // Room for only 4 segments against an interleave of 32: each pass can
    // finish 4 segments, the rest is refused and served by later passes.
    let segments = r.bundle.params().segment_count as u16;
    let mut passes = 0;
    while !complete(&r) && passes < 40 {
        for p in pass(&mut tx, &r, passes) {
            rx.handle(&p);
        }
        passes += 1;
        assert!(rx.stats().segments_completed >= 4 * passes.min(segments / 4) as u64, "steady progress");
    }
    assert!(complete(&r), "not complete after {passes} passes");
    assert!(passes <= segments.div_ceil(4), "{passes} passes for {segments} segments");
    assert!(rx.stats().no_room > 0);
    assert_imported(&r);
}

#[test]
fn an_abandoned_segment_is_evicted_once_stale() {
    let a = rig(300_000);
    // Room for a single open segment, and a segment counts as stale after 50 packets.
    let cfg = ReceiverConfig { max_open_blocks: 1, stale_after_packets: 50 };
    let mut rx = Receiver::new(a.vault.clone(), link(), cfg);
    let mut tx = Sender::new(
        VecOut::default(),
        link(),
        1,
        SenderConfig { interleave: 1, announce_every: 100, ..Default::default() },
    );
    let packets = pass(&mut tx, &a, 0);
    // Deliver the descriptor and only 3 symbols of segment 0, then the rest
    // of the pass from segment 1 onwards: segment 0 is stuck half-open.
    let k = 49; // symbols per 64 KiB segment at 1368 bytes
    for p in &packets[..4] {
        rx.handle(p);
    }
    for p in &packets[4 + k + 6..] {
        rx.handle(p);
    }
    let s = rx.stats();
    assert!(s.no_room > 0, "newcomers were refused while segment 0 was fresh");
    assert!(s.segments_completed >= 2, "after it went stale, others got the room: {s:?}");
}

#[test]
fn real_udp_socket_on_loopback() {
    use std::net::UdpSocket;
    let r = rig(1_500_000);
    let rx_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = rx_sock.local_addr().unwrap();

    std::thread::scope(|scope| {
        let bundle = &r.bundle;
        scope.spawn(move || {
            let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
            sock.connect(addr).unwrap();
            let cfg = SenderConfig { rate_bps: 80_000_000, ..Default::default() };
            let mut tx = Sender::new(sock, link(), 1, cfg);
            for n in 0..6 {
                // A real diode gives no feedback; the sender just keeps going.
                if tx.send_pass(bundle, n).is_err() {
                    break;
                }
            }
        });

        let mut rx = receiver(&r);
        let vault = r.vault.clone();
        let id = r.bundle.id();
        rx.run(&rx_sock, Some(std::time::Duration::from_secs(3)), &mut |ev| {
            !(matches!(ev, RxEvent::SegmentStored { .. }) && vault.missing(&id).is_empty())
        })
        .unwrap();
        let s = rx.stats();
        assert!(complete(&r), "incomplete after UDP transfer: {s:?}");
        assert_eq!(s.bad_mac + s.malformed, 0);
    });
    assert_imported(&r);
}
