//! Wire format (big-endian). Every packet ends with a 16-byte MAC over
//! everything before it.
//!
//! ```text
//! Common header, 32 bytes
//! 0   4  magic "MDD1"
//! 4   1  version (1)
//! 5   1  type: 1 ANNOUNCE, 2 DATA, 4 END_OF_PASS
//! 6   1  flags: bit0 = 1 (MAC present)
//! 7   1  header_len
//! 8   8  channel id
//! 16  4  session id (new for every sender start, never goes back)
//! 20  8  link_seq  (+1 per packet of the session)
//! 28  2  payload_len
//! 30  2  zero
//!
//! DATA, header_len 68            ANNOUNCE, header_len 40      END_OF_PASS, header_len 48
//! 32  4  epoch                   32  1  kind (1 descriptor)   32  4  epoch
//! 36  8  bundle seq              33  1  fragment index        36  8  bundle seq
//! 44  4  segment index           34  1  fragment count        44  2  pass
//! 48  4  symbol id (esi)         35  1  zero                  46  2  zero
//! 52  4  block length            36  4  sha256(object)[0..4]
//! 56  2  symbol size             40  .. fragment
//! 58  2  pass
//! 60  8  bundle_id[0..8]
//! 68  .. symbol
//! ```
use mer_core::ChannelId;
use mer_crypto::LinkKey;

pub const MAGIC: &[u8; 4] = b"MDD1";
pub const COMMON: usize = 32;
pub const MAC: usize = 16;
pub const T_ANNOUNCE: u8 = 1;
pub const T_DATA: u8 = 2;
pub const T_END: u8 = 4;
pub const DATA_HEADER: usize = 68;
pub const ANNOUNCE_HEADER: usize = 40;
pub const END_HEADER: usize = 48;
pub const MAX_FRAGMENTS: usize = 48;

pub fn build(
    ptype: u8,
    channel: &ChannelId,
    session: u32,
    link_seq: u64,
    extra: &[u8],
    payload: &[u8],
    key: &LinkKey,
) -> Vec<u8> {
    let header_len = COMMON + extra.len();
    let mut p = Vec::with_capacity(header_len + payload.len() + MAC);
    p.extend_from_slice(MAGIC);
    p.push(1);
    p.push(ptype);
    p.push(1);
    p.push(header_len as u8);
    p.extend_from_slice(&channel.0);
    p.extend_from_slice(&session.to_be_bytes());
    p.extend_from_slice(&link_seq.to_be_bytes());
    p.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(extra);
    p.extend_from_slice(payload);
    let tag = key.tag(&p);
    p.extend_from_slice(&tag);
    p
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    Malformed,
    BadMac,
}

pub struct Packet<'a> {
    pub ptype: u8,
    pub session: u32,
    pub link_seq: u64,
    /// Type-specific header fields.
    pub extra: &'a [u8],
    pub payload: &'a [u8],
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

/// Authenticate, then parse. Lengths inside the packet are used only after
/// the MAC has verified.
pub fn parse<'a>(pkt: &'a [u8], key: &LinkKey) -> Result<Packet<'a>, Refused> {
    if pkt.len() < COMMON + MAC || &pkt[0..4] != MAGIC || pkt[4] != 1 || pkt[6] & 1 != 1 {
        return Err(Refused::Malformed);
    }
    let (body, tag) = pkt.split_at(pkt.len() - MAC);
    if !key.verify(body, tag) {
        return Err(Refused::BadMac);
    }
    let ptype = body[5];
    let header_len = body[7] as usize;
    let expected = match ptype {
        T_DATA => DATA_HEADER,
        T_ANNOUNCE => ANNOUNCE_HEADER,
        T_END => END_HEADER,
        _ => return Err(Refused::Malformed),
    };
    let payload_len = u16::from_be_bytes([body[28], body[29]]) as usize;
    if header_len != expected || body.len() != header_len + payload_len {
        return Err(Refused::Malformed);
    }
    Ok(Packet {
        ptype,
        session: be32(body, 16),
        link_seq: u64::from_be_bytes(body[20..28].try_into().unwrap()),
        extra: &body[COMMON..header_len],
        payload: &body[header_len..],
    })
}

pub struct DataHeader {
    pub segment: u32,
    pub esi: u32,
    pub block_len: u32,
    pub symbol_size: u16,
    pub bundle_id8: [u8; 8],
}

pub fn data_extra(epoch: u32, seq: u64, h: &DataHeader, pass: u16) -> [u8; 36] {
    let mut e = [0u8; 36];
    e[0..4].copy_from_slice(&epoch.to_be_bytes());
    e[4..12].copy_from_slice(&seq.to_be_bytes());
    e[12..16].copy_from_slice(&h.segment.to_be_bytes());
    e[16..20].copy_from_slice(&h.esi.to_be_bytes());
    e[20..24].copy_from_slice(&h.block_len.to_be_bytes());
    e[24..26].copy_from_slice(&h.symbol_size.to_be_bytes());
    e[26..28].copy_from_slice(&pass.to_be_bytes());
    e[28..36].copy_from_slice(&h.bundle_id8);
    e
}

pub fn parse_data(extra: &[u8]) -> DataHeader {
    DataHeader {
        segment: be32(extra, 12),
        esi: be32(extra, 16),
        block_len: be32(extra, 20),
        symbol_size: u16::from_be_bytes([extra[24], extra[25]]),
        bundle_id8: extra[28..36].try_into().unwrap(),
    }
}

pub fn announce_extra(frag_idx: u8, frag_cnt: u8, tag: [u8; 4]) -> [u8; 8] {
    [1, frag_idx, frag_cnt, 0, tag[0], tag[1], tag[2], tag[3]]
}

pub fn end_extra(epoch: u32, seq: u64, pass: u16) -> [u8; 16] {
    let mut e = [0u8; 16];
    e[0..4].copy_from_slice(&epoch.to_be_bytes());
    e[4..12].copy_from_slice(&seq.to_be_bytes());
    e[12..14].copy_from_slice(&pass.to_be_bytes());
    e
}
