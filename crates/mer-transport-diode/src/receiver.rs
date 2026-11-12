use crate::packet::{self, Refused};
use mer_bundle_format::{BundleParams, SegmentSink, SlotOutcome};
use mer_core::{BundleId, Digest};
use mer_crypto::LinkKey;
use mer_fec::BlockDecoder;
use std::collections::{HashMap, HashSet};
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

const REPLAY_WINDOW: u64 = 4096;
const MAX_REASSEMBLIES: usize = 4;
const MAX_SETTLED_TAGS: usize = 1024;

#[derive(Clone, Debug)]
pub struct ReceiverConfig {
    /// Segments that may be partly received at the same time. Bounds memory:
    /// each open segment holds up to one segment of symbols.
    pub max_open_blocks: usize,
    /// A partly received segment that has seen no packet for this many data
    /// packets may be dropped to make room (its bundle was probably abandoned).
    pub stale_after_packets: u64,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        ReceiverConfig { max_open_blocks: 256, stale_after_packets: 1_000_000 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxStats {
    pub packets: u64,
    pub malformed: u64,
    pub bad_mac: u64,
    pub stale_session: u64,
    pub replayed: u64,
    /// Data for a bundle whose descriptor has not been accepted (yet).
    pub unknown_bundle: u64,
    pub invalid_data: u64,
    /// Data dropped because too many segments were open at once. Not an
    /// error (a later pass delivers it), but a sign that `max_open_blocks`
    /// is smaller than the sender's interleave.
    pub no_room: u64,
    pub descriptors_accepted: u64,
    pub descriptors_rejected: u64,
    pub segments_completed: u64,
    /// Segments that decoded but failed verification (only with a sender bug
    /// or a leaked link key). They start over on the next pass.
    pub segments_poisoned: u64,
    pub passes_ended: u64,
    /// Packets the link lost, estimated from gaps in the sequence numbers.
    pub lost: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxEvent {
    /// A segment was rebuilt, verified and stored.
    SegmentStored { bundle: BundleId, segment: u32 },
    /// The sender finished one pass over a bundle.
    PassEnded { seq: u64, pass: u16 },
}

struct OpenBlock {
    dec: BlockDecoder,
    symbol_size: u16,
    touched: u64,
}

struct Reassembly {
    frags: Vec<Option<Vec<u8>>>,
    bytes: usize,
}

pub struct Receiver {
    sink: Arc<dyn SegmentSink>,
    key: LinkKey,
    cfg: ReceiverConfig,
    session: Option<u32>,
    highest: u64,
    first: u64,
    received_in_session: u64,
    seen: HashSet<u64>,
    reassembly: HashMap<[u8; 4], Reassembly>,
    /// Announce tags already decided, so repeats cost nothing.
    settled: HashSet<[u8; 4]>,
    bundles: HashMap<[u8; 8], (BundleId, BundleParams)>,
    open: HashMap<([u8; 8], u32), OpenBlock>,
    tick: u64,
    stats: RxStats,
}

impl Receiver {
    pub fn new(sink: Arc<dyn SegmentSink>, key: LinkKey, cfg: ReceiverConfig) -> Self {
        Receiver {
            sink,
            key,
            cfg,
            session: None,
            highest: 0,
            first: 0,
            received_in_session: 0,
            seen: HashSet::new(),
            reassembly: HashMap::new(),
            settled: HashSet::new(),
            bundles: HashMap::new(),
            open: HashMap::new(),
            tick: 0,
            stats: RxStats::default(),
        }
    }

    pub fn stats(&self) -> RxStats {
        let mut s = self.stats;
        if self.session.is_some() {
            s.lost += (self.highest - self.first + 1).saturating_sub(self.received_in_session);
        }
        s
    }

    /// Session and replay check. Returns false when the packet must be dropped.
    fn admit(&mut self, session: u32, link_seq: u64) -> bool {
        match self.session {
            Some(cur) if session < cur => {
                self.stats.stale_session += 1;
                return false;
            }
            Some(cur) if session == cur => {}
            _ => {
                // A newer sender start: fold the old session's loss into the
                // total and start a fresh replay window.
                if self.session.is_some() {
                    self.stats.lost += (self.highest - self.first + 1).saturating_sub(self.received_in_session);
                }
                self.session = Some(session);
                self.seen.clear();
                self.highest = link_seq;
                self.first = link_seq;
                self.received_in_session = 0;
            }
        }
        if link_seq + REPLAY_WINDOW <= self.highest || !self.seen.insert(link_seq) {
            self.stats.replayed += 1;
            return false;
        }
        self.received_in_session += 1;
        if link_seq > self.highest {
            self.highest = link_seq;
        }
        if self.seen.len() > 2 * REPLAY_WINDOW as usize {
            let floor = self.highest.saturating_sub(REPLAY_WINDOW);
            self.seen.retain(|s| *s > floor);
        }
        true
    }

    /// Process one received datagram.
    pub fn handle(&mut self, pkt: &[u8]) -> Option<RxEvent> {
        self.stats.packets += 1;
        let p = match packet::parse(pkt, &self.key) {
            Ok(p) => p,
            Err(Refused::BadMac) => {
                self.stats.bad_mac += 1;
                return None;
            }
            Err(Refused::Malformed) => {
                self.stats.malformed += 1;
                return None;
            }
        };
        if !self.admit(p.session, p.link_seq) {
            return None;
        }
        match p.ptype {
            packet::T_ANNOUNCE => {
                self.on_announce(p.extra, p.payload);
                None
            }
            packet::T_DATA => self.on_data(p.extra, p.payload),
            packet::T_END => {
                self.stats.passes_ended += 1;
                Some(RxEvent::PassEnded {
                    seq: u64::from_be_bytes(p.extra[4..12].try_into().unwrap()),
                    pass: u16::from_be_bytes([p.extra[12], p.extra[13]]),
                })
            }
            _ => None,
        }
    }

    fn on_announce(&mut self, extra: &[u8], frag: &[u8]) {
        let (kind, idx, cnt) = (extra[0], extra[1] as usize, extra[2] as usize);
        let tag: [u8; 4] = extra[4..8].try_into().unwrap();
        if kind != 1 || cnt == 0 || cnt > packet::MAX_FRAGMENTS || idx >= cnt || frag.is_empty() {
            self.stats.invalid_data += 1;
            return;
        }
        if self.settled.contains(&tag) {
            return;
        }
        if !self.reassembly.contains_key(&tag) && self.reassembly.len() >= MAX_REASSEMBLIES {
            self.reassembly.clear();
        }
        let r = self.reassembly.entry(tag).or_insert_with(|| Reassembly { frags: vec![None; cnt], bytes: 0 });
        if r.frags.len() != cnt {
            self.reassembly.remove(&tag);
            return;
        }
        if r.frags[idx].is_none() {
            r.bytes += frag.len();
            r.frags[idx] = Some(frag.to_vec());
        }
        if r.frags.iter().any(Option::is_none) {
            return;
        }
        let object: Vec<u8> = self.reassembly.remove(&tag).unwrap().frags.into_iter().flatten().flatten().collect();
        if Digest::of(&object).0[..4] != tag {
            return; // fragments of different objects got mixed; wait for the next repeat
        }
        // The sink verifies the signature before it looks at anything else.
        // Only acceptance is remembered: a bundle refused now (for example
        // because it is too far ahead) may be acceptable on a later repeat.
        match self.sink.offer_descriptor(&object) {
            Ok(id) => {
                if self.settled.len() >= MAX_SETTLED_TAGS {
                    self.settled.clear();
                }
                self.settled.insert(tag);
                if let Some(params) = self.sink.params(&id) {
                    self.bundles.insert(id.0[..8].try_into().unwrap(), (id, params));
                    self.stats.descriptors_accepted += 1;
                }
            }
            Err(_) => self.stats.descriptors_rejected += 1,
        }
    }

    fn on_data(&mut self, extra: &[u8], symbol: &[u8]) -> Option<RxEvent> {
        let h = packet::parse_data(extra);
        let Some(&(id, params)) = self.bundles.get(&h.bundle_id8) else {
            // No buffering of data for unknown bundles: the descriptor is
            // repeated often, and the next pass refills what is dropped here.
            self.stats.unknown_bundle += 1;
            return None;
        };
        if h.segment >= params.segment_count
            || h.block_len as usize != params.block_len(h.segment)
            || symbol.len() != h.symbol_size as usize
            || h.esi >= 1 << 24
        {
            self.stats.invalid_data += 1;
            return None;
        }
        let slot = (h.bundle_id8, h.segment);
        if self.sink.has(&id, h.segment) {
            self.open.remove(&slot);
            return None;
        }
        self.tick += 1;
        if !self.open.contains_key(&slot) {
            if self.open.len() >= self.cfg.max_open_blocks {
                // Out of room. Evicting whichever segment was touched least
                // recently would livelock under interleaving (every packet
                // would evict another segment and none would ever finish).
                // So the newcomer is refused and the admitted segments are
                // allowed to complete; a later pass serves the rest. Only a
                // segment that has had no packet for a long time is evicted.
                let oldest = self.open.iter().min_by_key(|(_, b)| b.touched).map(|(k, b)| (*k, b.touched));
                match oldest {
                    Some((k, touched)) if self.tick - touched > self.cfg.stale_after_packets => {
                        self.open.remove(&k);
                    }
                    _ => {
                        self.stats.no_room += 1;
                        return None;
                    }
                }
            }
            match BlockDecoder::new(h.block_len as usize, h.symbol_size) {
                Ok(dec) => {
                    self.open.insert(slot, OpenBlock { dec, symbol_size: h.symbol_size, touched: self.tick });
                }
                Err(_) => {
                    self.stats.invalid_data += 1;
                    return None;
                }
            }
        }
        let block = self.open.get_mut(&slot).expect("just inserted");
        if block.symbol_size != h.symbol_size {
            self.stats.invalid_data += 1;
            return None;
        }
        block.touched = self.tick;
        let decoded = match block.dec.push(h.esi, symbol) {
            Ok(Some(b)) => b,
            Ok(None) => return None,
            Err(_) => {
                self.stats.invalid_data += 1;
                return None;
            }
        };
        self.open.remove(&slot);
        match self.sink.offer_block(&id, h.segment, &decoded) {
            SlotOutcome::Accepted => {
                self.stats.segments_completed += 1;
                Some(RxEvent::SegmentStored { bundle: id, segment: h.segment })
            }
            SlotOutcome::Duplicate => None,
            SlotOutcome::Rejected(_) => {
                self.stats.segments_poisoned += 1;
                None
            }
        }
    }

    /// Receive from a socket until `on_event` returns false, or until nothing
    /// has arrived for `idle` (when given).
    pub fn run(
        &mut self,
        sock: &UdpSocket,
        idle: Option<Duration>,
        on_event: &mut dyn FnMut(RxEvent) -> bool,
    ) -> std::io::Result<()> {
        sock.set_read_timeout(Some(Duration::from_millis(200)))?;
        let mut buf = vec![0u8; 65_536];
        let mut last = Instant::now();
        loop {
            match sock.recv(&mut buf) {
                Ok(n) => {
                    last = Instant::now();
                    if let Some(ev) = self.handle(&buf[..n]) {
                        if !on_event(ev) {
                            return Ok(());
                        }
                    }
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    if idle.is_some_and(|d| last.elapsed() >= d) {
                        return Ok(());
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
}
