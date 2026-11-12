use crate::{leaf_hash, verify_proof, BundleParams, FormatError};
use mer_core::{BundleId, Descriptor, Digest};

pub const BLOCK_MAGIC: &[u8; 4] = b"MSG1";
const FIXED_HEADER: usize = 32;

/// Bytes before the segment data in a block.
pub fn block_header_len(depth: u8) -> usize {
    FIXED_HEADER + 32 * depth as usize
}

/// Build a block (big-endian):
///
/// ```text
/// 0    4    magic "MSG1"
/// 4    1    kind (0 data)
/// 5    1    depth d
/// 6    2    zero
/// 8    4    segment index
/// 12   4    data_len
/// 16   16   bundle_id[0..16]
/// 32   32d  Merkle proof, leaf sibling first
/// ..   data
/// ```
pub fn encode_block(index: u32, bundle_id: &BundleId, proof: &[Digest], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(block_header_len(proof.len() as u8) + data.len());
    out.extend_from_slice(BLOCK_MAGIC);
    out.push(0);
    out.push(proof.len() as u8);
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&index.to_be_bytes());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(&bundle_id.0[..16]);
    for p in proof {
        out.extend_from_slice(&p.0);
    }
    out.extend_from_slice(data);
    out
}

/// Check a block against a verified descriptor and return its segment data.
/// This is the authority for accepting data from any transport.
pub fn verify_block<'a>(
    desc: &Descriptor,
    params: &BundleParams,
    index: u32,
    block: &'a [u8],
) -> Result<&'a [u8], FormatError> {
    let bad = |what: &str| FormatError::Integrity(format!("block {index}: {what}"));
    if index >= params.segment_count {
        return Err(bad("index out of range"));
    }
    if block.len() != params.block_len(index) {
        return Err(bad("wrong length"));
    }
    if &block[0..4] != BLOCK_MAGIC || block[4] != 0 || block[6..8] != [0, 0] {
        return Err(bad("bad header"));
    }
    if block[5] != params.depth {
        return Err(bad("wrong proof depth"));
    }
    if u32::from_be_bytes(block[8..12].try_into().unwrap()) != index {
        return Err(bad("index field mismatch"));
    }
    if u32::from_be_bytes(block[12..16].try_into().unwrap()) != params.segment_len(index) {
        return Err(bad("data length mismatch"));
    }
    if block[16..32] != desc.bundle_id.0[..16] {
        return Err(bad("belongs to another bundle"));
    }
    let header = block_header_len(params.depth);
    let proof: Vec<Digest> =
        block[FIXED_HEADER..header].as_chunks::<32>().0.iter().map(|c| Digest::from_slice(c).unwrap()).collect();
    let data = &block[header..];
    if !verify_proof(&desc.merkle_root, index, leaf_hash(index, data), &proof) {
        return Err(bad("Merkle proof does not match the signed root"));
    }
    Ok(data)
}

/// Pad a block to the fixed slot size and append a crc32c. The checksum is
/// only a cheap pre-filter for damaged media; the Merkle proof decides.
pub fn pad_slot(block: &[u8], slot_size: u32) -> Vec<u8> {
    let size = slot_size as usize;
    assert!(block.len() + 4 <= size);
    let mut out = vec![0u8; size];
    out[..block.len()].copy_from_slice(block);
    let crc = crc32c::crc32c(&out[..size - 4]);
    out[size - 4..].copy_from_slice(&crc.to_be_bytes());
    out
}

/// Check the crc of a stored slot and cut the block back out of it.
pub fn unpad_slot<'a>(slot: &'a [u8], params: &BundleParams, index: u32) -> Result<&'a [u8], FormatError> {
    let size = params.slot_size as usize;
    if slot.len() != size || index >= params.segment_count {
        return Err(FormatError::Integrity(format!("slot {index}: wrong size")));
    }
    let want = u32::from_be_bytes(slot[size - 4..].try_into().unwrap());
    if crc32c::crc32c(&slot[..size - 4]) != want {
        return Err(FormatError::Integrity(format!("slot {index}: checksum")));
    }
    Ok(&slot[..params.block_len(index)])
}
