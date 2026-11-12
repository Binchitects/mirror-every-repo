//! Collector side: compute deltas with no feedback from the vault, seal and sign.
use crate::{BundleError, Result};
use mer_bundle_format::{
    encode_block, encode_index, encode_ops, leaf_hash, params_of, sign_descriptor, to_cbor, BlobClass,
    BundleParams, FormatError, IndexRec, MerkleTree, SegmentSource,
};
use mer_core::{
    BundleId, ChannelId, Descriptor, Digest, EntryOp, EntryRole, Hasher, Manifest, RepoDelta, SnapshotId,
    Step, FORMAT_VERSION,
};
use mer_crypto::KeyPair;
use mer_meta::StepRow;
use mer_storage::{now_ms, Storage};
use std::collections::HashSet;
use std::io::Read;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct SealOptions {
    /// Stream is cut into pieces of this size (power of two, >= 4096).
    pub segment_size: u32,
    /// Each bundle repeats the (small) change lists of this many recent bundles,
    /// so a vault that missed some can still catch up.
    pub step_window: u64,
    pub collector: String,
}

impl Default for SealOptions {
    fn default() -> Self {
        SealOptions { segment_size: 4 * 1024 * 1024, step_window: 8, collector: "collector".into() }
    }
}

fn step_of(row: &StepRow) -> Step {
    Step {
        seq: row.bundle_seq,
        prev_seq: row.prev_bundle_seq,
        prev_root: row.prev_root,
        ops: row.ops,
        op_count: row.op_count,
        added_bytes: row.added_bytes,
        root: row.root,
    }
}

/// Where each byte of the bundle stream comes from. The stream is never
/// materialised: payload bytes are read from the blob store on demand.
struct Plan {
    manifest: Vec<u8>,
    index: Vec<u8>,
    blobs: Vec<IndexRec>,
    /// Stream offset of each blob.
    offsets: Vec<u64>,
    stream_len: u64,
}

impl Plan {
    fn new(manifest: Vec<u8>, blobs: Vec<IndexRec>) -> Self {
        let index = encode_index(&blobs);
        let mut at = (manifest.len() + index.len()) as u64;
        let mut offsets = Vec::with_capacity(blobs.len());
        for b in &blobs {
            offsets.push(at);
            at += b.len;
        }
        Plan { manifest, index, blobs, offsets, stream_len: at }
    }

    fn read(&self, st: &Storage, range: Range<u64>) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity((range.end - range.start) as usize);
        let m = self.manifest.len() as u64;
        let mi = m + self.index.len() as u64;
        let mut pos = range.start;
        while pos < range.end {
            if pos < m {
                let end = range.end.min(m);
                out.extend_from_slice(&self.manifest[pos as usize..end as usize]);
                pos = end;
            } else if pos < mi {
                let end = range.end.min(mi);
                out.extend_from_slice(&self.index[(pos - m) as usize..(end - m) as usize]);
                pos = end;
            } else {
                // Last blob starting at or before pos (empty blobs share an offset).
                let i = self.offsets.partition_point(|&o| o <= pos) - 1;
                let blob = &self.blobs[i];
                let off = pos - self.offsets[i];
                let n = (blob.len - off).min(range.end - pos);
                if n == 0 {
                    return Err(BundleError::Format(FormatError::Integrity("stream plan gap".into())));
                }
                let before = out.len();
                st.open_blob(&blob.digest, Some((off, n)))?.read_to_end(&mut out)?;
                if (out.len() - before) as u64 != n {
                    return Err(BundleError::Format(FormatError::Integrity(format!(
                        "blob {} is shorter than recorded",
                        blob.digest
                    ))));
                }
                pos += n;
            }
        }
        Ok(out)
    }
}

/// Re-hash a blob before it is put under a signature, so bit rot in the
/// collector's store is caught here and not on the far side of the gap.
fn verify_blob(st: &Storage, d: &Digest, len: u64) -> Result<()> {
    let mut h = Hasher::new();
    let mut r = st.open_blob(d, None)?;
    let mut buf = vec![0u8; 128 * 1024];
    let mut total = 0u64;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        total += n as u64;
    }
    if total != len || h.finish() != *d {
        return Err(BundleError::Format(FormatError::Integrity(format!(
            "blob {d} is corrupt in the collector store; run fsck"
        ))));
    }
    Ok(())
}

fn spool_dir(st: &Storage, channel: &ChannelId, epoch: u32, seq: u64) -> PathBuf {
    st.root().join("sealed").join(channel.to_hex()).join(format!("e{epoch}")).join(format!("s{seq:010}"))
}

/// Seal the next bundle of `channel`: everything that changed in any repo
/// since the previous bundle. Returns `None` when nothing changed.
///
/// The collector never asks the vault anything. It keeps its own ledger of
/// what it has already shipped and assumes the vault has it.
pub fn seal_bundle(
    st: &Arc<Storage>,
    channel: ChannelId,
    signers: &[&KeyPair],
    opts: &SealOptions,
) -> Result<Option<SealedBundle>> {
    let ch = st.meta().write(|t| t.channel_ensure(&channel, 1))?;
    let seq = ch.head_seq + 1;
    let window_start = seq.saturating_sub(opts.step_window);

    // 1. One new step per repo that changed since its last shipped step.
    struct NewStep {
        row: StepRow,
        ops: Vec<EntryOp>,
    }
    let mut new_steps: Vec<NewStep> = Vec::new();
    let repos = st.meta().read(|t| t.repo_list())?;
    for repo in &repos {
        let (head, last) =
            st.meta().read(|t| Ok((t.snapshot_head(repo.id)?, t.step_last(&channel, repo.id)?)))?;
        let Some(head) = head else { continue };
        let from = last.as_ref().map_or(0, |l| l.snapshot_seq);
        if head.seq <= from {
            continue;
        }
        let ops = st.meta().read(|t| t.diff(repo.id, from, head.seq))?;
        if ops.is_empty() {
            continue;
        }
        let (ops_digest, _) = st.ingest_bytes(&encode_ops(&ops))?;
        let prev_root = last.as_ref().map_or(SnapshotId::genesis(), |l| l.root);
        let added_bytes = ops
            .iter()
            .map(|op| match op {
                EntryOp::Put(e) => e.size,
                EntryOp::Del { .. } => 0,
            })
            .sum();
        new_steps.push(NewStep {
            row: StepRow {
                repo_id: repo.id,
                bundle_seq: seq,
                snapshot_seq: head.seq,
                prev_bundle_seq: last.as_ref().map_or(0, |l| l.bundle_seq),
                prev_root,
                root: SnapshotId::step(&prev_root, &ops_digest),
                ops: ops_digest,
                op_count: ops.len() as u32,
                added_bytes,
            },
            ops,
        });
    }
    if new_steps.is_empty() {
        return Ok(None);
    }

    // 2. Manifest: for every repo, all steps inside the window, oldest first.
    let mut deltas = Vec::new();
    let mut blobs: Vec<IndexRec> = Vec::new();
    let mut listed: HashSet<Digest> = HashSet::new();
    for repo in &repos {
        let mut steps: Vec<Step> = st
            .meta()
            .read(|t| t.steps_after(&channel, repo.id, window_start))?
            .iter()
            .map(step_of)
            .collect();
        if let Some(n) = new_steps.iter().find(|n| n.row.repo_id == repo.id) {
            steps.push(step_of(&n.row));
        }
        if steps.is_empty() {
            continue;
        }
        // Change lists are small and always shipped for the whole window.
        for s in &steps {
            if listed.insert(s.ops) {
                let len = st.blobs().stat(&s.ops)?.map(|b| b.size).ok_or_else(|| {
                    BundleError::NotFound(format!("ops object {} of an earlier bundle", s.ops))
                })?;
                blobs.push(IndexRec { digest: s.ops, len, class: BlobClass::Meta });
            }
        }
        deltas.push(RepoDelta { repo: repo.name.clone(), format: repo.format.clone(), steps });
    }

    // 3. Content: only what this channel has never carried before.
    let mut fresh: Vec<Digest> = Vec::new();
    for n in &new_steps {
        for op in &n.ops {
            let EntryOp::Put(e) = op else { continue };
            if listed.contains(&e.digest) || st.meta().read(|t| t.channel_blob_has(&channel, &e.digest))? {
                continue;
            }
            listed.insert(e.digest);
            fresh.push(e.digest);
            let class = if e.role == EntryRole::Artifact { BlobClass::Artifact } else { BlobClass::Record };
            blobs.push(IndexRec { digest: e.digest, len: e.size, class });
        }
    }
    blobs.sort_by_key(|b| b.class); // stable: metadata first
    for b in &blobs {
        verify_blob(st, &b.digest, b.len)?;
    }

    let manifest = Manifest {
        v: FORMAT_VERSION,
        channel,
        epoch: ch.epoch,
        seq,
        created_at: now_ms(),
        collector: opts.collector.clone(),
        repos: deltas,
        blob_count: blobs.len() as u64,
        payload_bytes: blobs.iter().map(|b| b.len).sum(),
    };
    let plan = Plan::new(to_cbor(&manifest), blobs);

    // 4. Merkle tree over the segments, then the one signed object.
    let params = BundleParams::new(plan.stream_len, opts.segment_size)?;
    let mut leaves = Vec::with_capacity(params.segment_count as usize);
    for i in 0..params.segment_count {
        leaves.push(leaf_hash(i, &plan.read(st, params.segment_range(i))?));
    }
    let tree = MerkleTree::from_leaves(leaves.clone());
    let manifest_sha256 = Digest::of(&plan.manifest);
    let index_sha256 = Digest::of(&plan.index);
    let desc = Descriptor {
        format_version: FORMAT_VERSION,
        channel,
        epoch: ch.epoch,
        seq,
        bundle_id: BundleId::compute(&manifest_sha256, &index_sha256),
        created_at: manifest.created_at,
        stream_len: plan.stream_len,
        segment_size: opts.segment_size,
        segment_count: params.segment_count,
        merkle_root: tree.root(),
        manifest_len: plan.manifest.len() as u64,
        manifest_sha256,
        index_len: plan.index.len() as u64,
        index_sha256,
    };
    let envelope = sign_descriptor(&desc, signers);

    // 5. Spool the small files, then commit the ledger in one transaction.
    let dir = spool_dir(st, &channel, ch.epoch, seq);
    let tmp = dir.with_extension("tmp");
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&dir); // left by a crash before the commit below
    std::fs::create_dir_all(&tmp)?;
    std::fs::write(tmp.join("manifest.bin"), &plan.manifest)?;
    std::fs::write(tmp.join("index.bin"), &plan.index)?;
    std::fs::write(tmp.join("leaves.bin"), leaves.iter().flat_map(|l| l.0).collect::<Vec<u8>>())?;
    std::fs::write(tmp.join("descriptor.bin"), &envelope)?;
    std::fs::rename(&tmp, &dir)?;

    st.meta().write(|t| {
        t.bundle_insert(&channel, seq, &desc.bundle_id, desc.created_at)?;
        for n in &new_steps {
            t.step_insert(&channel, &n.row)?;
            t.blob_ref_add(&format!("ops:{}:{}", channel.to_hex(), seq), &n.row.ops)?;
        }
        for d in &fresh {
            t.channel_blob_add(&channel, d, seq)?;
        }
        t.channel_set_head(&channel, seq)?;
        t.audit(
            now_ms(),
            "collector",
            "bundle.seal",
            &format!("channel {} seq {seq} id {} bytes {}", channel, desc.bundle_id, plan.stream_len),
        )?;
        Ok(())
    })?;

    Ok(Some(SealedBundle { st: st.clone(), desc, envelope, params, plan, tree }))
}

/// A sealed bundle, ready to be handed to any transport, any number of times.
pub struct SealedBundle {
    st: Arc<Storage>,
    desc: Descriptor,
    envelope: Vec<u8>,
    params: BundleParams,
    plan: Plan,
    tree: MerkleTree,
}

impl SealedBundle {
    /// Re-open a bundle sealed earlier (for re-export or repeat transmission).
    pub fn open(st: &Arc<Storage>, channel: ChannelId, seq: u64) -> Result<Self> {
        let ch = st
            .meta()
            .read(|t| t.channel_get(&channel))?
            .ok_or_else(|| BundleError::NotFound(format!("channel {channel}")))?;
        let dir = spool_dir(st, &channel, ch.epoch, seq);
        if !dir.is_dir() {
            return Err(BundleError::NotFound(format!("sealed bundle {seq} of channel {channel}")));
        }
        let envelope = std::fs::read(dir.join("descriptor.bin"))?;
        // Our own spool: decode without a trust check, but still validate shape.
        let body_len = u32::from_be_bytes(
            envelope.get(4..8).and_then(|b| b.try_into().ok()).ok_or_else(|| {
                BundleError::Format(FormatError::Integrity("spooled descriptor".into()))
            })?,
        ) as usize;
        let body = envelope
            .get(8..8 + body_len)
            .ok_or_else(|| BundleError::Format(FormatError::Integrity("spooled descriptor".into())))?;
        let desc: Descriptor = mer_bundle_format::from_cbor(body)?;
        let params = params_of(&desc)?;
        let manifest = std::fs::read(dir.join("manifest.bin"))?;
        let index_bytes = std::fs::read(dir.join("index.bin"))?;
        let payload_len = desc.stream_len - desc.manifest_len - desc.index_len;
        let blobs = mer_bundle_format::decode_index(&index_bytes, payload_len)?;
        let leaves: Vec<Digest> = std::fs::read(dir.join("leaves.bin"))?
            .chunks_exact(32)
            .map(|c| Digest::from_slice(c).expect("32 bytes"))
            .collect();
        let tree = MerkleTree::from_leaves(leaves);
        if tree.root() != desc.merkle_root || Digest::of(&manifest) != desc.manifest_sha256 {
            return Err(BundleError::Format(FormatError::Integrity("spooled bundle is damaged".into())));
        }
        let plan = Plan::new(manifest, blobs);
        Ok(SealedBundle { st: st.clone(), desc, envelope, params, plan, tree })
    }

    pub fn seq(&self) -> u64 {
        self.desc.seq
    }
    pub fn id(&self) -> BundleId {
        self.desc.bundle_id
    }
    pub fn stream_len(&self) -> u64 {
        self.desc.stream_len
    }
    pub fn blob_count(&self) -> usize {
        self.plan.blobs.len()
    }
    pub fn payload_bytes(&self) -> u64 {
        self.plan.blobs.iter().map(|b| b.len).sum()
    }
}

impl SegmentSource for SealedBundle {
    fn descriptor_envelope(&self) -> &[u8] {
        &self.envelope
    }
    fn descriptor(&self) -> &Descriptor {
        &self.desc
    }
    fn params(&self) -> BundleParams {
        self.params
    }
    fn read_block(&self, index: u32) -> std::result::Result<Vec<u8>, FormatError> {
        if index >= self.params.segment_count {
            return Err(FormatError::Integrity(format!("segment {index} out of range")));
        }
        let data = self
            .plan
            .read(&self.st, self.params.segment_range(index))
            .map_err(|e| FormatError::Io(e.to_string()))?;
        Ok(encode_block(index, &self.desc.bundle_id, &self.tree.proof(index), &data))
    }
}
