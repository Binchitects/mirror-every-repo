use crate::FormatError;
use mer_core::Digest;

pub const INDEX_RECORD_LEN: usize = 56;

/// What a payload blob is, in shipping order: metadata first, so the most
/// useful segments arrive first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BlobClass {
    /// Ops objects.
    Meta = 0,
    /// Records and evidence.
    Record = 1,
    Artifact = 2,
}

/// One payload blob. Offsets are the running sum of `len` in index order.
///
/// ```text
/// 0   32  sha256 of the content (the store key)
/// 32   8  raw_len    u64 BE
/// 40   8  stored_len u64 BE   (equal to raw_len: blobs are stored uncompressed)
/// 48   1  enc        0 raw
/// 49   1  class
/// 50   6  zero
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexRec {
    pub digest: Digest,
    pub len: u64,
    pub class: BlobClass,
}

pub fn encode_index(recs: &[IndexRec]) -> Vec<u8> {
    let mut out = Vec::with_capacity(recs.len() * INDEX_RECORD_LEN);
    for r in recs {
        out.extend_from_slice(&r.digest.0);
        out.extend_from_slice(&r.len.to_be_bytes());
        out.extend_from_slice(&r.len.to_be_bytes());
        out.push(0);
        out.push(r.class as u8);
        out.extend_from_slice(&[0; 6]);
    }
    out
}

/// Decode and check that the blobs exactly fill `payload_len` bytes.
pub fn decode_index(bytes: &[u8], payload_len: u64) -> Result<Vec<IndexRec>, FormatError> {
    if bytes.len() % INDEX_RECORD_LEN != 0 {
        return Err(FormatError::Integrity("blob index length".into()));
    }
    let mut out = Vec::with_capacity(bytes.len() / INDEX_RECORD_LEN);
    let mut total = 0u64;
    let mut seen = std::collections::HashSet::new();
    for rec in bytes.chunks_exact(INDEX_RECORD_LEN) {
        let digest = Digest::from_slice(&rec[0..32]).expect("32 bytes");
        let raw = u64::from_be_bytes(rec[32..40].try_into().unwrap());
        let stored = u64::from_be_bytes(rec[40..48].try_into().unwrap());
        if rec[48] != 0 || raw != stored {
            return Err(FormatError::Unsupported("blob encoding".into()));
        }
        let class = match rec[49] {
            0 => BlobClass::Meta,
            1 => BlobClass::Record,
            2 => BlobClass::Artifact,
            c => return Err(FormatError::Unsupported(format!("blob class {c}"))),
        };
        if rec[50..56] != [0; 6] {
            return Err(FormatError::Integrity("blob index padding".into()));
        }
        if !seen.insert(digest) {
            return Err(FormatError::Integrity("blob listed twice".into()));
        }
        total = total
            .checked_add(raw)
            .ok_or_else(|| FormatError::Integrity("blob index overflows".into()))?;
        out.push(IndexRec { digest, len: raw, class });
    }
    if total != payload_len {
        return Err(FormatError::Integrity(format!(
            "blob index covers {total} bytes, payload has {payload_len}"
        )));
    }
    Ok(out)
}
