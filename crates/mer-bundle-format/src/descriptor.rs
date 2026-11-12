use crate::{block_header_len, depth_for, from_cbor, to_cbor, FormatError};
use mer_core::{limits, BundleId, Descriptor, FORMAT_VERSION};
use mer_crypto::{open_envelope, seal_envelope, KeyPair, TrustStore};

pub const DESCRIPTOR_MAGIC: &[u8; 4] = b"MSD1";
pub const DESCRIPTOR_DOMAIN: &[u8] = b"mer/descriptor/v1\0";

/// Geometry of a bundle, derived only from a verified descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BundleParams {
    pub stream_len: u64,
    pub segment_size: u32,
    pub segment_count: u32,
    pub depth: u8,
    pub slot_size: u32,
}

impl BundleParams {
    pub fn new(stream_len: u64, segment_size: u32) -> Result<Self, FormatError> {
        if !(limits::MIN_SEGMENT_SIZE..=limits::MAX_SEGMENT_SIZE).contains(&segment_size)
            || !segment_size.is_power_of_two()
        {
            return Err(FormatError::Limit(format!("segment size {segment_size}")));
        }
        if stream_len == 0 {
            return Err(FormatError::Integrity("empty stream".into()));
        }
        let count = stream_len.div_ceil(segment_size as u64);
        let segment_count =
            u32::try_from(count).map_err(|_| FormatError::Limit("segment count".into()))?;
        let depth = depth_for(segment_count);
        let raw = block_header_len(depth) as u64 + segment_size as u64 + 4;
        let slot_size = raw.div_ceil(4096) * 4096;
        Ok(BundleParams { stream_len, segment_size, segment_count, depth, slot_size: slot_size as u32 })
    }

    /// Data bytes in segment `index` (only the last one may be short).
    pub fn segment_len(&self, index: u32) -> u32 {
        let start = index as u64 * self.segment_size as u64;
        (self.stream_len - start).min(self.segment_size as u64) as u32
    }

    /// Length of the block (header + proof + data) of segment `index`.
    pub fn block_len(&self, index: u32) -> usize {
        block_header_len(self.depth) + self.segment_len(index) as usize
    }

    /// Stream byte range covered by segment `index`.
    pub fn segment_range(&self, index: u32) -> std::ops::Range<u64> {
        let start = index as u64 * self.segment_size as u64;
        start..start + self.segment_len(index) as u64
    }
}

pub fn params_of(desc: &Descriptor) -> Result<BundleParams, FormatError> {
    let p = BundleParams::new(desc.stream_len, desc.segment_size)?;
    if p.segment_count != desc.segment_count {
        return Err(FormatError::Integrity("descriptor segment count".into()));
    }
    Ok(p)
}

pub fn sign_descriptor(desc: &Descriptor, signers: &[&KeyPair]) -> Vec<u8> {
    seal_envelope(DESCRIPTOR_MAGIC, DESCRIPTOR_DOMAIN, &to_cbor(desc), signers)
}

/// Verify the signature first, then decode, then check internal consistency.
pub fn open_descriptor(envelope: &[u8], trust: &TrustStore) -> Result<Descriptor, FormatError> {
    let body = open_envelope(
        DESCRIPTOR_MAGIC,
        DESCRIPTOR_DOMAIN,
        limits::MAX_DESCRIPTOR_BODY,
        envelope,
        trust,
    )?;
    let desc: Descriptor = from_cbor(body)?;
    if desc.format_version != FORMAT_VERSION {
        return Err(FormatError::Unsupported(format!(
            "bundle format {} (this build reads {FORMAT_VERSION}); upgrade the vault",
            desc.format_version
        )));
    }
    params_of(&desc)?;
    if desc.seq == 0 {
        return Err(FormatError::Integrity("bundle seq 0".into()));
    }
    if desc.manifest_len == 0
        || desc.manifest_len > limits::MAX_MANIFEST_BYTES
        || desc.manifest_len.checked_add(desc.index_len).is_none_or(|n| n > desc.stream_len)
    {
        return Err(FormatError::Integrity("descriptor lengths".into()));
    }
    if desc.bundle_id != BundleId::compute(&desc.manifest_sha256, &desc.index_sha256) {
        return Err(FormatError::Integrity("bundle id".into()));
    }
    Ok(desc)
}
