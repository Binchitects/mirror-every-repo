//! Vault side: collect verified slots from any transport, then import.
use crate::{BundleError, Result};
use mer_blob::Expect;
use mer_bundle_format::{
    decode_index, decode_manifest, decode_ops, open_descriptor, pad_slot, params_of, unpad_slot,
    verify_block, BundleParams, FormatError, SegmentSink, SlotOutcome,
};
use mer_core::{limits, BundleId, ChannelId, Descriptor, Digest, EntryOp, Manifest, RepoDelta, SnapshotId, Step};
use mer_crypto::TrustStore;
use mer_meta::RepoImport;
use mer_storage::{now_ms, Storage, StorageError};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Default)]
pub struct ImportPolicy {
    /// Approve and publish imported snapshots without waiting for a person.
    /// Off by default: imported content waits in quarantine.
    pub auto_publish: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyState {
    /// New steps were applied and a snapshot was sealed.
    Applied,
    /// The repo was already at or beyond everything this bundle carries.
    UpToDate,
    /// Some content has not arrived yet (an earlier bundle is missing).
    Incomplete,
    /// The vault is further behind than the bundle's step window reaches.
    WaitingBase,
    /// The vault's history does not match the collector's. Never auto-repaired.
    Diverged,
    QuotaBlocked,
}

impl ApplyState {
    fn as_str(self) -> &'static str {
        match self {
            ApplyState::Applied | ApplyState::UpToDate => "ok",
            ApplyState::Incomplete => "incomplete",
            ApplyState::WaitingBase => "waiting_base",
            ApplyState::Diverged => "diverged",
            ApplyState::QuotaBlocked => "quota_blocked",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoApply {
    pub repo: String,
    pub state: ApplyState,
    /// Bundle seq the repo is now at.
    pub applied_seq: u64,
    /// Snapshot created by this apply, if any.
    pub snapshot: Option<u64>,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct ImportReport {
    pub channel: ChannelId,
    pub seq: u64,
    pub bundle_id: BundleId,
    pub blobs_total: u64,
    pub blobs_new: u64,
    pub bytes_new: u64,
    pub repos: Vec<RepoApply>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedInfo {
    pub channel: ChannelId,
    pub seq: u64,
    pub bundle_id: BundleId,
    pub segments_have: u32,
    pub segments_total: u32,
}

struct Staged {
    desc: Descriptor,
    params: BundleParams,
    dir: PathBuf,
    file: File,
    have: Vec<bool>,
}

impl Staged {
    fn have_count(&self) -> u32 {
        self.have.iter().filter(|h| **h).count() as u32
    }
    fn complete(&self) -> bool {
        self.have.iter().all(|h| *h)
    }
}

#[derive(Default)]
struct Inner {
    staged: HashMap<BundleId, Staged>,
    /// Bundles already imported: transports are told "have everything".
    done: HashMap<BundleId, BundleParams>,
}

pub struct Vault {
    st: Arc<Storage>,
    trust: TrustStore,
    policy: ImportPolicy,
    inner: Mutex<Inner>,
}

fn staging_dir(root: &Path, d: &Descriptor) -> PathBuf {
    root.join("staging").join(d.channel.to_hex()).join(format!(
        "e{}-s{:010}-{}",
        d.epoch,
        d.seq,
        &d.bundle_id.to_hex()[..16]
    ))
}

fn manifest_key(channel: &ChannelId, seq: u64) -> String {
    format!("manifest:{}:{seq}", channel.to_hex())
}

fn integrity(msg: impl Into<String>) -> FormatError {
    FormatError::Integrity(msg.into())
}

impl Vault {
    /// Open the vault side of a data directory and resume any partly received bundles.
    pub fn new(st: Arc<Storage>, trust: TrustStore, policy: ImportPolicy) -> Result<Self> {
        let vault = Vault { st, trust, policy, inner: Mutex::default() };
        vault.resume()?;
        Ok(vault)
    }

    pub fn storage(&self) -> &Arc<Storage> {
        &self.st
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Rebuild the receive state from the staging directory. Nothing on disk is
    /// trusted: the descriptor signature and every stored slot are re-verified.
    fn resume(&self) -> Result<()> {
        let root = self.st.root().join("staging");
        if !root.is_dir() {
            return Ok(());
        }
        for chan in std::fs::read_dir(&root)? {
            let chan = chan?.path();
            if !chan.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&chan)? {
                let dir = entry?.path();
                let staged = std::fs::read(dir.join("descriptor.bin"))
                    .ok()
                    .and_then(|env| open_descriptor(&env, &self.trust).ok())
                    .and_then(|desc| self.open_staging(&dir, desc).ok());
                match staged {
                    Some(s) => {
                        self.lock().staged.insert(s.desc.bundle_id, s);
                    }
                    None => {
                        let _ = std::fs::remove_dir_all(&dir);
                    }
                }
            }
        }
        Ok(())
    }

    fn open_staging(&self, dir: &Path, desc: Descriptor) -> Result<Staged> {
        let params = params_of(&desc)?;
        let mut file =
            std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join("slots.part"))?;
        let full = params.slot_size as u64 * params.segment_count as u64;
        let existing = file.metadata()?.len();
        if existing != full {
            file.set_len(full)?; // sparse: unreceived slots cost no disk space
        }
        let mut have = vec![false; params.segment_count as usize];
        if existing > 0 {
            let mut slot = vec![0u8; params.slot_size as usize];
            file.seek(SeekFrom::Start(0))?;
            for i in 0..params.segment_count {
                file.read_exact(&mut slot)?;
                have[i as usize] = unpad_slot(&slot, &params, i)
                    .and_then(|block| verify_block(&desc, &params, i, block))
                    .is_ok();
            }
        }
        Ok(Staged { desc, params, dir: dir.to_path_buf(), file, have })
    }

    /// Bundles currently being received.
    pub fn staged(&self) -> Vec<StagedInfo> {
        let inner = self.lock();
        let mut out: Vec<StagedInfo> = inner
            .staged
            .values()
            .map(|s| StagedInfo {
                channel: s.desc.channel,
                seq: s.desc.seq,
                bundle_id: s.desc.bundle_id,
                segments_have: s.have_count(),
                segments_total: s.params.segment_count,
            })
            .collect();
        out.sort_by_key(|s| (s.channel, s.seq));
        out
    }

    /// Import every bundle whose segments have all arrived, oldest first.
    pub fn import_ready(&self) -> Result<Vec<ImportReport>> {
        let mut ready: Vec<(u64, BundleId)> = self
            .lock()
            .staged
            .values()
            .filter(|s| s.complete())
            .map(|s| (s.desc.seq, s.desc.bundle_id))
            .collect();
        ready.sort();
        let mut reports = Vec::new();
        for (_, id) in ready {
            let Some(mut staged) = self.lock().staged.remove(&id) else { continue };
            let result = self.import_staged(&mut staged);
            let _ = std::fs::remove_dir_all(&staged.dir);
            match result {
                Ok(report) => {
                    self.lock().done.insert(id, staged.params);
                    reports.push(report);
                }
                Err(e) => {
                    let d = &staged.desc;
                    self.st.meta().write(|t| {
                        t.bundle_import_set(&d.channel, d.seq, &d.bundle_id, "failed", now_ms())?;
                        t.audit(now_ms(), "vault", "bundle.failed", &format!("seq {} id {}: {e}", d.seq, d.bundle_id))?;
                        Ok(())
                    })?;
                    return Err(e);
                }
            }
        }
        Ok(reports)
    }

    fn import_staged(&self, staged: &mut Staged) -> Result<ImportReport> {
        staged.file.sync_all()?;
        let desc = staged.desc.clone();
        let mut reader = StagedReader {
            file: &mut staged.file,
            desc: &desc,
            params: staged.params,
            next: 0,
            buf: Vec::new(),
            pos: 0,
        };

        // Manifest and blob index: checked against the signed descriptor before use.
        let mut manifest_bytes = vec![0u8; desc.manifest_len as usize];
        reader.read_exact(&mut manifest_bytes)?;
        if Digest::of(&manifest_bytes) != desc.manifest_sha256 {
            return Err(BundleError::Rejected("manifest does not match the signed descriptor".into()));
        }
        let manifest = decode_manifest(&manifest_bytes)?;
        if manifest.channel != desc.channel || manifest.epoch != desc.epoch || manifest.seq != desc.seq {
            return Err(BundleError::Rejected("manifest and descriptor disagree".into()));
        }
        let mut index_bytes = vec![0u8; desc.index_len as usize];
        reader.read_exact(&mut index_bytes)?;
        if Digest::of(&index_bytes) != desc.index_sha256 {
            return Err(BundleError::Rejected("blob index does not match the signed descriptor".into()));
        }
        let index = decode_index(&index_bytes, desc.stream_len - desc.manifest_len - desc.index_len)?;

        // Blobs: each is hashed while it streams into the store.
        let (mut blobs_new, mut bytes_new) = (0u64, 0u64);
        for rec in &index {
            let was_there = self.st.has_blob(&rec.digest)?;
            let mut part = (&mut reader).take(rec.len);
            match self.st.ingest(&mut part, Expect::exact(rec.digest, rec.len)) {
                Ok(_) => {}
                Err(StorageError::Store(e)) => {
                    return Err(BundleError::Rejected(format!("blob {}: {e}", rec.digest)))
                }
                Err(e) => return Err(e.into()),
            }
            if !was_there {
                blobs_new += 1;
                bytes_new += rec.len;
            }
        }

        // Record the import. The channel position moves even if some repo
        // cannot be applied yet: one stuck repo must not stall the others.
        let (manifest_digest, _) = self.st.ingest_bytes(&manifest_bytes)?;
        let chan = desc.channel.to_hex();
        let key = manifest_key(&desc.channel, desc.seq);
        self.st.meta().write(|t| {
            // The manifest and its change lists stay pinned until every repo
            // in it has been applied.
            t.blob_ref_add(&key, &manifest_digest)?;
            for repo in &manifest.repos {
                for step in &repo.steps {
                    t.blob_ref_add(&key, &step.ops)?;
                }
            }
            t.kv_set(&key, &manifest_digest.to_hex())?;
            t.bundle_import_set(&desc.channel, desc.seq, &desc.bundle_id, "imported", now_ms())?;
            t.channel_advance_imported(&desc.channel, desc.seq)?;
            t.audit(
                now_ms(),
                "vault",
                "bundle.import",
                &format!("channel {chan} seq {} id {} new blobs {blobs_new}", desc.seq, desc.bundle_id),
            )?;
            Ok(())
        })?;

        let repos = self.apply_pending(&desc.channel)?;
        Ok(ImportReport {
            channel: desc.channel,
            seq: desc.seq,
            bundle_id: desc.bundle_id,
            blobs_total: index.len() as u64,
            blobs_new,
            bytes_new,
            repos,
        })
    }

    /// Bring every repo of a channel as far forward as the content received so
    /// far allows. Safe to call at any time and any number of times.
    pub fn apply_pending(&self, channel: &ChannelId) -> Result<Vec<RepoApply>> {
        let imported = self.st.meta().read(|t| t.bundle_import_list(channel))?;
        let mut latest: std::collections::BTreeMap<String, RepoApply> = Default::default();
        // Oldest manifest first, so each repo walks its chain in order.
        for imp in imported.iter().filter(|b| b.state == "imported") {
            let key = manifest_key(channel, imp.seq);
            let Some(hex) = self.st.meta().read(|t| t.kv_get(&key))? else { continue };
            let digest = Digest::from_hex(&hex).map_err(mer_meta::MetaError::from)?;
            let manifest: Manifest = decode_manifest(&self.st.read_blob(&digest)?)?;
            let mut finished = true;
            for delta in &manifest.repos {
                let out = self.apply_repo(channel, delta)?;
                let newest_step = delta.steps.last().map_or(0, |s| s.seq);
                finished &= out.applied_seq >= newest_step;
                if out.state != ApplyState::UpToDate || !latest.contains_key(&out.repo) {
                    latest.insert(out.repo.clone(), out);
                }
            }
            if finished {
                self.st.meta().write(|t| {
                    t.kv_delete(&key)?;
                    t.blob_ref_clear(&key)
                })?;
            }
        }
        Ok(latest.into_values().collect())
    }

    fn apply_repo(&self, channel: &ChannelId, delta: &RepoDelta) -> Result<RepoApply> {
        let name = delta.repo.as_str();
        let repo = match self.st.meta().read(|t| t.repo_get(name))? {
            Some(r) => r,
            None => self.st.create_repo(name, &delta.format)?,
        };
        let cur = self.st.meta().read(|t| t.repo_import_get(channel, repo.id))?.unwrap_or(RepoImport {
            applied_seq: 0,
            chain_root: SnapshotId::genesis(),
            state: "ok".into(),
            detail: String::new(),
        });
        let outcome = |state: ApplyState, applied_seq: u64, snapshot: Option<u64>, detail: String| RepoApply {
            repo: name.to_string(),
            state,
            applied_seq,
            snapshot,
            detail,
        };
        // Keeps position and root, only records why the repo is not moving.
        let stuck = |state: ApplyState, detail: String| -> Result<RepoApply> {
            let row = RepoImport { state: state.as_str().into(), detail: detail.clone(), ..cur.clone() };
            self.st.meta().write(|t| t.repo_import_set(channel, repo.id, &row))?;
            Ok(outcome(state, cur.applied_seq, None, detail))
        };

        if repo.format != delta.format {
            return stuck(
                ApplyState::Diverged,
                format!("repo is format {} here, {} in the bundle", repo.format, delta.format),
            );
        }
        let todo: Vec<&Step> = delta.steps.iter().filter(|s| s.seq > cur.applied_seq).collect();
        let Some(first) = todo.first() else {
            return Ok(outcome(ApplyState::UpToDate, cur.applied_seq, None, String::new()));
        };
        if first.prev_root != cur.chain_root {
            return if cur.applied_seq < first.prev_seq {
                stuck(
                    ApplyState::WaitingBase,
                    format!(
                        "vault is at bundle {}, the oldest change in this bundle builds on bundle {}",
                        cur.applied_seq, first.prev_seq
                    ),
                )
            } else {
                stuck(ApplyState::Diverged, "history differs from the collector's".into())
            };
        }

        // Take the longest run of leading steps whose content is all here.
        let mut ready: Vec<(&Step, Vec<EntryOp>)> = Vec::new();
        let mut blocked = String::new();
        for step in &todo {
            if !self.st.has_blob(&step.ops)? {
                blocked = format!("change list of bundle {} has not arrived", step.seq);
                break;
            }
            let ops = decode_ops(&self.st.read_blob(&step.ops)?)?;
            if ops.len() != step.op_count as usize {
                return stuck(ApplyState::Diverged, format!("bundle {}: op count mismatch", step.seq));
            }
            let mut missing = 0usize;
            for op in &ops {
                if let EntryOp::Put(e) = op {
                    match self.st.meta().read(|t| t.blob_get(&e.digest))? {
                        Some(b) if b.size == e.size && b.state == mer_meta::BlobState::Present => {}
                        Some(b) if b.size != e.size => {
                            return stuck(
                                ApplyState::Diverged,
                                format!("bundle {}: size of {} disagrees", step.seq, e.path),
                            )
                        }
                        _ => missing += 1,
                    }
                }
            }
            if missing > 0 {
                blocked = format!("bundle {}: {missing} blobs have not arrived", step.seq);
                break;
            }
            ready.push((step, ops));
        }
        let Some((last, _)) = ready.last() else {
            return stuck(ApplyState::Incomplete, blocked);
        };
        let last: &Step = last;

        let mut writer = self.st.begin(name)?;
        for (_, ops) in &ready {
            for op in ops {
                match op {
                    EntryOp::Put(e) => writer.put(&e.path, e.role, &e.digest)?,
                    EntryOp::Del { path } => writer.remove(path)?,
                }
            }
        }
        let sealed = match writer.seal(Some(last.root)) {
            Ok(s) => s,
            Err(e @ StorageError::QuotaExceeded { .. }) => {
                return stuck(ApplyState::QuotaBlocked, e.to_string())
            }
            Err(e) => return Err(e.into()),
        };
        let state = if ready.len() < todo.len() { ApplyState::Incomplete } else { ApplyState::Applied };
        let row = RepoImport {
            applied_seq: last.seq,
            chain_root: last.root,
            state: state.as_str().into(),
            detail: blocked.clone(),
        };
        self.st.meta().write(|t| t.repo_import_set(channel, repo.id, &row))?;

        let snapshot = sealed.map(|s| s.seq);
        if let (true, Some(seq)) = (self.policy.auto_publish, snapshot) {
            self.st.decide(name, seq, true, "policy:auto-publish")?;
            self.st.publish(name, seq, "policy:auto-publish")?;
        }
        Ok(outcome(state, last.seq, snapshot, blocked))
    }
}

/// Sequential reader over the staged stream. Every slot is checked again
/// (checksum and Merkle proof) as it is read back from disk.
struct StagedReader<'a> {
    file: &'a mut File,
    desc: &'a Descriptor,
    params: BundleParams,
    next: u32,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for StagedReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos == self.buf.len() {
            if self.next == self.params.segment_count {
                return Ok(0);
            }
            let mut slot = vec![0u8; self.params.slot_size as usize];
            self.file.seek(SeekFrom::Start(self.next as u64 * self.params.slot_size as u64))?;
            self.file.read_exact(&mut slot)?;
            let data = unpad_slot(&slot, &self.params, self.next)
                .and_then(|block| verify_block(self.desc, &self.params, self.next, block))
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
            self.buf = data.to_vec();
            self.pos = 0;
            self.next += 1;
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl SegmentSink for Vault {
    fn offer_descriptor(&self, envelope: &[u8]) -> std::result::Result<BundleId, FormatError> {
        // Signature first. Nothing below runs for an unsigned or untrusted bundle.
        let desc = open_descriptor(envelope, &self.trust)?;
        let params = params_of(&desc)?;
        let id = desc.bundle_id;
        {
            let inner = self.lock();
            if inner.staged.contains_key(&id) || inner.done.contains_key(&id) {
                return Ok(id);
            }
        }
        let io = |e: mer_meta::MetaError| FormatError::Io(e.to_string());
        let meta = self.st.meta();
        let ch = meta.write(|t| t.channel_ensure(&desc.channel, desc.epoch)).map_err(io)?;
        if ch.epoch != desc.epoch {
            return Err(integrity(format!("bundle epoch {} but channel is at epoch {}", desc.epoch, ch.epoch)));
        }
        match meta.read(|t| t.bundle_import_get(&desc.channel, desc.seq)).map_err(io)? {
            Some(known) if known.bundle_id != id => {
                // Two different signed bundles for one position: never accept the second.
                meta.write(|t| {
                    t.audit(
                        now_ms(),
                        "vault",
                        "bundle.fork",
                        &format!("seq {} known {} offered {}", desc.seq, known.bundle_id, id),
                    )
                })
                .map_err(io)?;
                return Err(integrity(format!(
                    "a different bundle is already recorded for seq {} (possible fork or replay)",
                    desc.seq
                )));
            }
            Some(known) if known.state == "imported" => {
                self.lock().done.insert(id, params);
                return Ok(id);
            }
            Some(_) => {}
            None => {
                if desc.seq > ch.imported_seq + limits::ACCEPT_WINDOW {
                    return Err(integrity(format!(
                        "seq {} is too far ahead of the vault (at {})",
                        desc.seq, ch.imported_seq
                    )));
                }
                if desc.seq + limits::ACCEPT_WINDOW <= ch.imported_seq {
                    return Err(integrity(format!("seq {} is too old (vault is at {})", desc.seq, ch.imported_seq)));
                }
            }
        }
        let dir = staging_dir(self.st.root(), &desc);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("descriptor.bin"), envelope)?;
        let staged = self.open_staging(&dir, desc.clone()).map_err(|e| FormatError::Io(e.to_string()))?;
        meta.write(|t| t.bundle_import_set(&desc.channel, desc.seq, &id, "receiving", now_ms())).map_err(io)?;
        self.lock().staged.insert(id, staged);
        Ok(id)
    }

    fn params(&self, bundle: &BundleId) -> Option<BundleParams> {
        let inner = self.lock();
        inner.staged.get(bundle).map(|s| s.params).or_else(|| inner.done.get(bundle).copied())
    }

    fn has(&self, bundle: &BundleId, index: u32) -> bool {
        let inner = self.lock();
        match inner.staged.get(bundle) {
            Some(s) => s.have.get(index as usize).copied().unwrap_or(false),
            None => inner.done.contains_key(bundle),
        }
    }

    fn offer_block(&self, bundle: &BundleId, index: u32, block: &[u8]) -> SlotOutcome {
        let mut inner = self.lock();
        if inner.done.contains_key(bundle) {
            return SlotOutcome::Duplicate;
        }
        let Some(s) = inner.staged.get_mut(bundle) else {
            return SlotOutcome::Rejected("unknown bundle: its descriptor has not been accepted".into());
        };
        if s.have.get(index as usize).copied().unwrap_or(false) {
            return SlotOutcome::Duplicate;
        }
        if let Err(e) = verify_block(&s.desc, &s.params, index, block) {
            return SlotOutcome::Rejected(e.to_string());
        }
        let slot = pad_slot(block, s.params.slot_size);
        let at = index as u64 * s.params.slot_size as u64;
        if let Err(e) = s.file.seek(SeekFrom::Start(at)).and_then(|_| s.file.write_all(&slot)) {
            return SlotOutcome::Rejected(format!("staging write: {e}"));
        }
        s.have[index as usize] = true;
        SlotOutcome::Accepted
    }

    fn missing(&self, bundle: &BundleId) -> Vec<Range<u32>> {
        let inner = self.lock();
        let Some(s) = inner.staged.get(bundle) else { return Vec::new() };
        let mut out: Vec<Range<u32>> = Vec::new();
        for (i, have) in s.have.iter().enumerate() {
            if *have {
                continue;
            }
            let i = i as u32;
            match out.last_mut() {
                Some(r) if r.end == i => r.end = i + 1,
                _ => out.push(i..i + 1),
            }
        }
        out
    }
}
