use crate::packet::{self, DataHeader};
use crate::DiodeError;
use mer_bundle_format::SegmentSource;
use mer_core::Digest;
use mer_crypto::LinkKey;
use mer_fec::BlockEncoder;
use std::time::{Duration, Instant};

/// Where packets go. A UDP socket in production, a buffer in tests.
pub trait PacketOut {
    fn send_packet(&mut self, pkt: &[u8]) -> std::io::Result<()>;
}

impl PacketOut for std::net::UdpSocket {
    fn send_packet(&mut self, pkt: &[u8]) -> std::io::Result<()> {
        self.send(pkt).map(|_| ())
    }
}

/// Collects packets in memory.
#[derive(Default)]
pub struct VecOut(pub Vec<Vec<u8>>);

impl PacketOut for VecOut {
    fn send_packet(&mut self, pkt: &[u8]) -> std::io::Result<()> {
        self.0.push(pkt.to_vec());
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SenderConfig {
    /// Bytes of segment data per packet. 1368 fits a 1500-byte MTU over IPv6.
    pub symbol_size: u16,
    /// Extra repair symbols per pass, as a fraction of the source symbols.
    pub overhead: f64,
    /// Hard cap on the send rate. Anything above the diode's capacity is
    /// silently lost, so this must not exceed it. 0 = unlimited (tests).
    pub rate_bps: u64,
    /// Segments whose packets are interleaved, so a burst of loss is spread
    /// thinly over many segments instead of wiping out one.
    pub interleave: usize,
    /// Repeat the signed descriptor after this many data packets.
    pub announce_every: u32,
}

impl Default for SenderConfig {
    fn default() -> Self {
        SenderConfig { symbol_size: 1368, overhead: 0.10, rate_bps: 0, interleave: 8, announce_every: 2000 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassStats {
    pub packets: u64,
    pub bytes: u64,
}

pub struct Sender<O: PacketOut> {
    pub out: O,
    key: LinkKey,
    session: u32,
    link_seq: u64,
    cfg: SenderConfig,
    started: Instant,
    paced_bytes: u64,
}

impl<O: PacketOut> Sender<O> {
    /// `session` must be higher than any session this sender used before
    /// (for example the start time in seconds); receivers ignore older ones.
    pub fn new(out: O, key: LinkKey, session: u32, cfg: SenderConfig) -> Self {
        Sender { out, key, session, link_seq: 0, cfg, started: Instant::now(), paced_bytes: 0 }
    }

    fn emit(&mut self, ptype: u8, src: &dyn SegmentSource, extra: &[u8], payload: &[u8], stats: &mut PassStats) -> std::io::Result<()> {
        let pkt = packet::build(ptype, &src.descriptor().channel, self.session, self.link_seq, extra, payload, &self.key);
        self.link_seq += 1;
        if self.cfg.rate_bps > 0 {
            self.paced_bytes += pkt.len() as u64;
            let due = self.started + Duration::from_secs_f64(self.paced_bytes as f64 * 8.0 / self.cfg.rate_bps as f64);
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
        }
        self.out.send_packet(&pkt)?;
        stats.packets += 1;
        stats.bytes += pkt.len() as u64;
        Ok(())
    }

    fn announce(&mut self, src: &dyn SegmentSource, stats: &mut PassStats) -> Result<(), DiodeError> {
        let env = src.descriptor_envelope();
        let frags: Vec<&[u8]> = env.chunks(self.cfg.symbol_size as usize).collect();
        if frags.len() > packet::MAX_FRAGMENTS {
            return Err(DiodeError::DescriptorTooLarge(frags.len()));
        }
        let tag: [u8; 4] = Digest::of(env).0[..4].try_into().unwrap();
        for (i, frag) in frags.iter().enumerate() {
            let extra = packet::announce_extra(i as u8, frags.len() as u8, tag);
            self.emit(packet::T_ANNOUNCE, src, &extra, frag, stats)?;
        }
        Ok(())
    }

    /// Send the whole bundle once. Even passes carry the data itself plus a
    /// little repair; odd passes carry only fresh repair symbols. Symbols of
    /// different passes never repeat, so every extra pass adds information.
    pub fn send_pass(&mut self, src: &dyn SegmentSource, pass: u16) -> Result<PassStats, DiodeError> {
        let desc = src.descriptor().clone();
        let params = src.params();
        let t = self.cfg.symbol_size;
        let mut stats = PassStats::default();
        let mut since_announce = 0u32;
        self.announce(src, &mut stats)?;

        let bundle_id8: [u8; 8] = desc.bundle_id.0[..8].try_into().unwrap();
        let segments: Vec<u32> = (0..params.segment_count).collect();
        for window in segments.chunks(self.cfg.interleave.max(1)) {
            // Encode every block of the window, then emit round-robin.
            let mut lists: Vec<(u32, u32, Vec<(u32, Vec<u8>)>)> = Vec::with_capacity(window.len());
            for &segment in window {
                let block = src.read_block(segment)?;
                let enc = BlockEncoder::new(&block, t)?;
                let k = enc.k();
                let extra = ((k as f64 * self.cfg.overhead).ceil() as u32).max(1);
                // Each pass owns a private range of repair indexes. Symbol ids
                // are 24 bits on the wire, so very late passes wrap around.
                let span = (1u32 << 24) - 60_000 - (k + extra);
                let start = ((pass as u64 * (k + extra) as u64) % span as u64) as u32;
                let symbols = if pass % 2 == 0 {
                    let mut s = enc.source();
                    s.extend(enc.repair(start, extra));
                    s
                } else {
                    enc.repair(start, k + extra)
                };
                lists.push((segment, block.len() as u32, symbols));
            }
            let rounds = lists.iter().map(|l| l.2.len()).max().unwrap_or(0);
            for round in 0..rounds {
                for (segment, block_len, symbols) in &lists {
                    let Some((esi, symbol)) = symbols.get(round) else { continue };
                    let h = DataHeader { segment: *segment, esi: *esi, block_len: *block_len, symbol_size: t, bundle_id8 };
                    let extra = packet::data_extra(desc.epoch, desc.seq, &h, pass);
                    self.emit(packet::T_DATA, src, &extra, symbol, &mut stats)?;
                    since_announce += 1;
                    if since_announce >= self.cfg.announce_every {
                        since_announce = 0;
                        self.announce(src, &mut stats)?;
                    }
                }
            }
        }
        let extra = packet::end_extra(desc.epoch, desc.seq, pass);
        self.emit(packet::T_END, src, &extra, &[], &mut stats)?;
        Ok(stats)
    }
}
