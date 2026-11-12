//! Forward error correction for the one-way diode link (RaptorQ, RFC 6330).
//!
//! A block is split into K source symbols. The sender can produce an endless
//! supply of extra repair symbols; the receiver rebuilds the block from about
//! any K symbols, whichever ones survive. No feedback is needed.
use raptorq::{
    EncodingPacket, ObjectTransmissionInformation, PayloadId, SourceBlockDecoder, SourceBlockEncoder,
};

/// RFC 6330 limit on source symbols per block.
pub const MAX_SYMBOLS: u32 = 56_403;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FecError {
    #[error("symbol size {0} must be a non-zero multiple of 8")]
    SymbolSize(u16),
    #[error("block of {len} bytes needs more than {MAX_SYMBOLS} symbols of {t} bytes")]
    BlockTooLarge { len: usize, t: u16 },
    #[error("empty block")]
    Empty,
    #[error("symbol has {got} bytes, expected {want}")]
    SymbolLength { got: usize, want: usize },
}

/// Number of source symbols (K) for a block.
pub fn symbol_count(block_len: usize, t: u16) -> u32 {
    block_len.div_ceil(t as usize) as u32
}

fn check(block_len: usize, t: u16) -> Result<ObjectTransmissionInformation, FecError> {
    if t == 0 || t % 8 != 0 {
        return Err(FecError::SymbolSize(t));
    }
    if block_len == 0 {
        return Err(FecError::Empty);
    }
    let k = symbol_count(block_len, t);
    if k > MAX_SYMBOLS {
        return Err(FecError::BlockTooLarge { len: block_len, t });
    }
    Ok(ObjectTransmissionInformation::new(k as u64 * t as u64, t, 1, 1, 8))
}

pub struct BlockEncoder {
    enc: SourceBlockEncoder,
    k: u32,
}

impl BlockEncoder {
    pub fn new(block: &[u8], t: u16) -> Result<Self, FecError> {
        let config = check(block.len(), t)?;
        let k = symbol_count(block.len(), t);
        let mut padded = block.to_vec();
        padded.resize(k as usize * t as usize, 0);
        Ok(BlockEncoder { enc: SourceBlockEncoder::new(0, &config, &padded), k })
    }

    pub fn k(&self) -> u32 {
        self.k
    }

    /// The K source symbols as (esi, bytes). They are the block itself, in order.
    pub fn source(&self) -> Vec<(u32, Vec<u8>)> {
        self.enc.source_packets().into_iter().map(unpack).collect()
    }

    /// `count` repair symbols starting at repair index `start`. Different
    /// indexes give different symbols, so every pass can send fresh ones.
    pub fn repair(&self, start: u32, count: u32) -> Vec<(u32, Vec<u8>)> {
        self.enc.repair_packets(start, count).into_iter().map(unpack).collect()
    }
}

fn unpack(p: EncodingPacket) -> (u32, Vec<u8>) {
    let (id, data) = p.split();
    (id.encoding_symbol_id(), data)
}

pub struct BlockDecoder {
    dec: SourceBlockDecoder,
    block_len: usize,
    t: usize,
    k: u32,
    received: u32,
    seen: std::collections::HashSet<u32>,
    done: bool,
}

impl BlockDecoder {
    pub fn new(block_len: usize, t: u16) -> Result<Self, FecError> {
        let config = check(block_len, t)?;
        let k = symbol_count(block_len, t);
        Ok(BlockDecoder {
            dec: SourceBlockDecoder::new(0, &config, k as u64 * t as u64),
            block_len,
            t: t as usize,
            k,
            received: 0,
            seen: Default::default(),
            done: false,
        })
    }

    pub fn k(&self) -> u32 {
        self.k
    }

    /// Distinct symbols received so far.
    pub fn received(&self) -> u32 {
        self.received
    }

    /// Feed one symbol. Returns the block once enough symbols have arrived.
    /// Duplicates are ignored. After the block has been returned once, further
    /// symbols return `None`.
    pub fn push(&mut self, esi: u32, symbol: &[u8]) -> Result<Option<Vec<u8>>, FecError> {
        if symbol.len() != self.t {
            return Err(FecError::SymbolLength { got: symbol.len(), want: self.t });
        }
        if self.done || !self.seen.insert(esi) {
            return Ok(None);
        }
        self.received += 1;
        let packet = EncodingPacket::new(PayloadId::new(0, esi), symbol.to_vec());
        match self.dec.decode(std::iter::once(packet)) {
            Some(mut block) => {
                self.done = true;
                block.truncate(self.block_len);
                Ok(Some(block))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 + i / 7) as u8).collect()
    }

    /// Deterministic pseudo-random stream for loss patterns.
    struct Rng(u64);
    impl Rng {
        fn chance(&mut self, percent: u64) -> bool {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % 100 < percent
        }
    }

    #[test]
    fn source_symbols_alone_rebuild_the_block() {
        let block = data(10_000);
        let enc = BlockEncoder::new(&block, 1368).unwrap();
        assert_eq!(enc.k(), 8);
        let mut dec = BlockDecoder::new(block.len(), 1368).unwrap();
        let mut out = None;
        for (esi, sym) in enc.source() {
            assert!(out.is_none());
            out = dec.push(esi, &sym).unwrap();
        }
        assert_eq!(out.unwrap(), block);
    }

    #[test]
    fn source_symbols_are_the_block_in_order() {
        let block = data(4096);
        let enc = BlockEncoder::new(&block, 1024).unwrap();
        let joined: Vec<u8> = enc.source().into_iter().flat_map(|(_, s)| s).collect();
        assert_eq!(joined, block);
    }

    #[test]
    fn repair_symbols_alone_rebuild_the_block() {
        let block = data(50_000);
        let enc = BlockEncoder::new(&block, 1368).unwrap();
        let mut dec = BlockDecoder::new(block.len(), 1368).unwrap();
        let mut out = None;
        for (esi, sym) in enc.repair(0, enc.k() + 4) {
            if let Some(b) = dec.push(esi, &sym).unwrap() {
                out = Some(b);
                break;
            }
        }
        assert_eq!(out.expect("K + 4 repair symbols decode"), block);
    }

    #[test]
    fn survives_heavy_random_loss_with_small_overhead() {
        let block = data(300_000);
        let enc = BlockEncoder::new(&block, 1368).unwrap();
        let k = enc.k();
        let mut stream = enc.source();
        stream.extend(enc.repair(0, k)); // 100 % repair available, far more than needed
        for loss in [10u64, 30, 45] {
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ loss);
            let mut dec = BlockDecoder::new(block.len(), 1368).unwrap();
            let mut out = None;
            for (esi, sym) in &stream {
                if rng.chance(loss) {
                    continue; // lost on the wire
                }
                if let Some(b) = dec.push(*esi, sym).unwrap() {
                    out = Some(b);
                    break;
                }
            }
            assert_eq!(out.as_ref(), Some(&block), "{loss}% loss");
            assert!(dec.received() <= k + 4, "{loss}% loss: needed {} symbols for K = {k}", dec.received());
        }
    }

    #[test]
    fn symbols_from_different_passes_combine() {
        let block = data(80_000);
        let enc = BlockEncoder::new(&block, 1368).unwrap();
        let k = enc.k();
        let mut dec = BlockDecoder::new(block.len(), 1368).unwrap();
        let mut out = None;
        // Half of the source symbols from pass 0, then repair symbols from two later passes.
        let mut feed: Vec<(u32, Vec<u8>)> = enc.source().into_iter().step_by(2).collect();
        feed.extend(enc.repair(0, k / 4));
        feed.extend(enc.repair(1000, k / 2));
        for (esi, sym) in feed {
            if let Some(b) = dec.push(esi, &sym).unwrap() {
                out = Some(b);
                break;
            }
        }
        assert_eq!(out.unwrap(), block);
    }

    #[test]
    fn duplicates_do_not_count_and_too_few_symbols_do_not_decode() {
        let block = data(20_000);
        let enc = BlockEncoder::new(&block, 1368).unwrap();
        let src = enc.source();
        let mut dec = BlockDecoder::new(block.len(), 1368).unwrap();
        for _ in 0..5 {
            for (esi, sym) in src.iter().take(src.len() - 1) {
                assert!(dec.push(*esi, sym).unwrap().is_none());
            }
        }
        assert_eq!(dec.received(), enc.k() - 1);
    }

    #[test]
    fn bad_parameters_are_errors_not_panics() {
        assert_eq!(BlockEncoder::new(&[1; 10], 0).err(), Some(FecError::SymbolSize(0)));
        assert_eq!(BlockEncoder::new(&[1; 10], 1001).err(), Some(FecError::SymbolSize(1001)));
        assert_eq!(BlockEncoder::new(&[], 1368).err(), Some(FecError::Empty));
        assert!(matches!(BlockDecoder::new(64 << 20, 8), Err(FecError::BlockTooLarge { .. })));
        let mut dec = BlockDecoder::new(5000, 1368).unwrap();
        assert_eq!(dec.push(0, &[0; 10]), Err(FecError::SymbolLength { got: 10, want: 1368 }));
    }
}
