//! The bundle encoding shared by every transport. Pure byte work: no
//! filesystem, no network.
//!
//! ```text
//! stream  = manifest (CBOR) || blob index (56-byte records) || payload (blobs, concatenated)
//! segment = fixed-size piece of the stream
//! block   = header || Merkle proof || segment     (verifiable on its own)
//! slot    = block padded to a fixed size + crc32c (what media and staging store)
//! ```
//! One signed descriptor carries the Merkle root, so any single block can be
//! checked against the signature no matter how or in which order it arrived.
mod cbor;
mod descriptor;
mod index;
mod merkle;
mod slot;
#[cfg(test)]
mod tests;

pub use cbor::*;
pub use descriptor::*;
pub use index::*;
pub use merkle::*;
pub use slot::*;

use mer_core::{BundleId, Descriptor};
use mer_crypto::CryptoError;
use std::ops::Range;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FormatError {
    #[error("cbor: {0}")]
    Cbor(String),
    #[error("integrity: {0}")]
    Integrity(String),
    #[error("limit exceeded: {0}")]
    Limit(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    #[error("io: {0}")]
    Io(String),
}

impl From<std::io::Error> for FormatError {
    fn from(e: std::io::Error) -> Self {
        FormatError::Io(e.to_string())
    }
}

/// A sealed bundle: random access to its blocks. Implemented by the collector.
pub trait SegmentSource: Send + Sync {
    fn descriptor_envelope(&self) -> &[u8];
    fn descriptor(&self) -> &Descriptor;
    fn params(&self) -> BundleParams;
    /// Header + proof + data of segment `index`.
    fn read_block(&self, index: u32) -> Result<Vec<u8>, FormatError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlotOutcome {
    Accepted,
    /// Already held and verified; nothing was written.
    Duplicate,
    Rejected(String),
}

/// Where transports deliver what they receive. Implemented by the vault.
/// Everything offered is untrusted; there is no return channel.
pub trait SegmentSink: Send + Sync {
    /// Verify a signed descriptor and start (or continue) receiving its bundle.
    fn offer_descriptor(&self, envelope: &[u8]) -> Result<BundleId, FormatError>;
    fn params(&self, bundle: &BundleId) -> Option<BundleParams>;
    fn has(&self, bundle: &BundleId, index: u32) -> bool;
    fn offer_block(&self, bundle: &BundleId, index: u32, block: &[u8]) -> SlotOutcome;
    /// Segment ranges still missing; empty when the bundle is complete.
    fn missing(&self, bundle: &BundleId) -> Vec<Range<u32>>;
}
