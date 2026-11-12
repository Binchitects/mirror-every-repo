//! Structures that cross the air gap. Field names are wire format (CBOR maps).
use crate::{BundleId, ChannelId, Digest, SnapshotId};
use serde::{Deserialize, Serialize};

pub const FORMAT_VERSION: u16 = 1;

/// The only signed object per bundle. Everything else chains to it by hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    pub format_version: u16,
    pub channel: ChannelId,
    pub epoch: u32,
    pub seq: u64,
    pub bundle_id: BundleId,
    pub created_at: i64,
    pub stream_len: u64,
    pub segment_size: u32,
    pub segment_count: u32,
    pub merkle_root: Digest,
    pub manifest_len: u64,
    pub manifest_sha256: Digest,
    pub index_len: u64,
    pub index_sha256: Digest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub v: u16,
    pub channel: ChannelId,
    pub epoch: u32,
    pub seq: u64,
    pub created_at: i64,
    pub collector: String,
    /// Only repos that have a step inside the step window.
    pub repos: Vec<RepoDelta>,
    pub blob_count: u64,
    pub payload_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDelta {
    pub repo: String,
    pub format: String,
    /// Chained steps, oldest first. A vault applies only those beyond its head.
    pub steps: Vec<Step>,
}

/// One change set of one repo, relative to the previous step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// Bundle seq in which this step was first shipped.
    pub seq: u64,
    /// Bundle seq of the previous step of this repo (0 = none: chain starts at genesis).
    pub prev_seq: u64,
    pub prev_root: SnapshotId,
    /// Digest of the ops object (a blob in the bundle).
    pub ops: Digest,
    pub op_count: u32,
    pub added_bytes: u64,
    /// Must equal `SnapshotId::step(prev_root, ops)`.
    pub root: SnapshotId,
}

impl BundleId {
    pub fn compute(manifest_sha256: &Digest, index_sha256: &Digest) -> Self {
        let mut h = crate::Hasher::new();
        h.update(b"mer/bundle-id/v1\0");
        h.update(&manifest_sha256.0);
        h.update(&index_sha256.0);
        BundleId(h.finish().0)
    }
}
