//! Facade over the blob store and the metadata database. Formats, bundle code
//! and the CLI reach storage only through this crate.
#[cfg(test)]
mod tests;

use mer_blob::{BlobReader, BlobStore, Expect, FsStore, StoreError};
use mer_core::{Digest, EntryRec, EntryRole, Hasher, SnapshotId};
use mer_meta::{Meta, MetaError, PublishKind, Published, Repo, SnapState, SnapshotRow};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Meta(#[from] MetaError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("repo {repo} would use {usage} bytes, quota is {quota}")]
    QuotaExceeded { repo: String, usage: u64, quota: u64 },
}

pub type Result<T> = std::result::Result<T, StorageError>;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Result of asking the published view of a repo for a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    Found(EntryRec),
    /// The repo is live but does not have this path. Recorded for the wishlist.
    Miss,
    /// Nothing has been published for this repo yet.
    NotPublished,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub orphaned: u64,
    pub deleted: u64,
    pub bytes_freed: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FsckReport {
    pub checked: u64,
    /// Rows whose bytes are missing from the store.
    pub missing: Vec<Digest>,
    /// Rows whose bytes no longer hash to their digest.
    pub corrupt: Vec<Digest>,
    /// Store files with no row (left by a crash; removed by GC after the grace period).
    pub unknown: Vec<Digest>,
}

impl FsckReport {
    pub fn is_clean(&self) -> bool {
        self.missing.is_empty() && self.corrupt.is_empty()
    }
}

pub struct Storage {
    root: PathBuf,
    blobs: Arc<dyn BlobStore>,
    meta: Meta,
}

impl Storage {
    /// Open (or create) a data directory: `<dir>/store` and `<dir>/meta.db`.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let root = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        let blobs = Arc::new(FsStore::open(root.join("store"))?);
        let meta = Meta::open(root.join("meta.db"))?;
        let st = Storage { root, blobs, meta };
        st.recover()?;
        Ok(st)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn meta(&self) -> &Meta {
        &self.meta
    }
    pub fn blobs(&self) -> &dyn BlobStore {
        self.blobs.as_ref()
    }

    /// Finish what a crash interrupted.
    fn recover(&self) -> Result<()> {
        self.blobs.sweep_tmp()?;
        for d in self.meta.read(|t| t.gc_deleting())? {
            self.blobs.delete(&d)?;
            self.meta.write(|t| t.gc_finish(&d))?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- blobs

    /// Store content. Bytes become durable first, then the row says so.
    pub fn ingest(&self, src: &mut dyn Read, expect: Expect) -> Result<(Digest, u64)> {
        let out = self.blobs.put(src, expect)?;
        let mut tries = 0;
        loop {
            match self.meta.write(|t| t.blob_present(&out.digest, out.size, now_ms())) {
                Ok(_) => return Ok((out.digest, out.size)),
                // GC is deleting this very blob. Let it finish, then the bytes
                // have to be written again, which only the caller can do.
                Err(MetaError::Busy(d)) if tries < 50 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    if self.meta.read(|t| t.blob_get(&d))?.is_none() && self.blobs.stat(&d)?.is_none() {
                        return Err(MetaError::Busy(d).into());
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn ingest_bytes(&self, data: &[u8]) -> Result<(Digest, u64)> {
        let mut r = data;
        let mut last = None;
        for _ in 0..3 {
            match self.ingest(&mut r, Expect::default()) {
                Err(StorageError::Meta(MetaError::Busy(d))) => {
                    last = Some(d);
                    r = data;
                }
                other => return other,
            }
        }
        Err(MetaError::Busy(last.unwrap_or(Digest::ZERO)).into())
    }

    pub fn has_blob(&self, d: &Digest) -> Result<bool> {
        Ok(self.meta.read(|t| t.blob_is_present(d))?)
    }

    pub fn open_blob(&self, d: &Digest, range: Option<(u64, u64)>) -> Result<BlobReader> {
        Ok(self.blobs.open(d, range)?)
    }

    pub fn read_blob(&self, d: &Digest) -> Result<Vec<u8>> {
        Ok(self.blobs.read_all(d)?)
    }

    // ---------------------------------------------------------------- repos

    pub fn create_repo(&self, name: &str, format: &str) -> Result<Repo> {
        Ok(self.meta.write(|t| {
            let r = t.repo_create(name, format)?;
            t.audit(now_ms(), "system", "repo.create", name)?;
            Ok(r)
        })?)
    }

    pub fn repo(&self, name: &str) -> Result<Repo> {
        Ok(self.meta.read(|t| t.repo_require(name))?)
    }

    /// Start a new snapshot on top of the current head of `repo`.
    pub fn begin(&self, repo: &str) -> Result<SnapshotWriter<'_>> {
        let (repo, seq) = self.meta.write(|t| {
            let r = t.repo_require(repo)?;
            // A building snapshot left by a crashed writer is rolled back first.
            if let Some(stale) = t.snapshot_building(r.id)? {
                t.snapshot_abort(r.id, stale)?;
            }
            let seq = t.snapshot_begin(r.id, now_ms())?;
            Ok((r, seq))
        })?;
        Ok(SnapshotWriter { st: self, repo, seq, pending: Vec::new(), done: false })
    }

    pub fn head(&self, repo: &str) -> Result<Option<SnapshotRow>> {
        Ok(self.meta.read(|t| {
            let r = t.repo_require(repo)?;
            t.snapshot_head(r.id)
        })?)
    }

    // ---------------------------------------------------- gate and publish

    /// The approval gate: a sealed snapshot is approved or rejected by a person.
    pub fn decide(&self, repo: &str, seq: u64, approve: bool, actor: &str) -> Result<()> {
        Ok(self.meta.write(|t| {
            let r = t.repo_require(repo)?;
            t.snapshot_decide(r.id, seq, approve)?;
            let action = if approve { "snapshot.approve" } else { "snapshot.reject" };
            t.audit(now_ms(), actor, action, &format!("{repo}@{seq}"))?;
            Ok(())
        })?)
    }

    pub fn publish(&self, repo: &str, seq: u64, actor: &str) -> Result<Published> {
        self.move_pointer(repo, seq, PublishKind::Publish, actor)
    }

    pub fn rollback(&self, repo: &str, seq: u64, actor: &str) -> Result<Published> {
        self.move_pointer(repo, seq, PublishKind::Rollback, actor)
    }

    fn move_pointer(&self, repo: &str, seq: u64, kind: PublishKind, actor: &str) -> Result<Published> {
        Ok(self.meta.write(|t| {
            let r = t.repo_require(repo)?;
            let p = t.publish(r.id, seq, kind, now_ms())?;
            t.audit(now_ms(), actor, &format!("repo.{}", p.kind), &format!("{repo}@{seq}"))?;
            Ok(p)
        })?)
    }

    pub fn live(&self, repo: &str) -> Result<Option<Published>> {
        Ok(self.meta.read(|t| {
            let r = t.repo_require(repo)?;
            t.live(r.id)
        })?)
    }

    // --------------------------------------------------------------- lookup

    /// What package clients see: the published snapshot only.
    pub fn get(&self, repo: &str, path: &str) -> Result<Lookup> {
        self.lookup_published(repo, path, true)
    }

    /// Like `get`, with control over whether a miss goes on the wishlist.
    /// Some clients probe for optional files; those probes are not wishes.
    pub fn lookup_published(&self, repo: &str, path: &str, record_miss: bool) -> Result<Lookup> {
        let found = self.meta.read(|t| {
            let r = t.repo_require(repo)?;
            match t.live(r.id)? {
                None => Ok(Lookup::NotPublished),
                Some(p) => Ok(t.lookup(r.id, p.seq, path)?.map_or(Lookup::Miss, Lookup::Found)),
            }
        })?;
        if record_miss && found == Lookup::Miss && mer_core::validate_path(path).is_ok() {
            self.meta.write(|t| t.miss_record(repo, path, now_ms()))?;
        }
        Ok(found)
    }

    /// Look inside a specific snapshot that is still retained (for preview and tests).
    pub fn get_at(&self, repo: &str, seq: u64, path: &str) -> Result<Option<EntryRec>> {
        Ok(self.meta.read(|t| {
            let r = t.repo_require(repo)?;
            match t.snapshot_get(r.id, seq)? {
                Some(s) if matches!(s.state, SnapState::Sealed | SnapState::Approved) => {
                    t.lookup(r.id, seq, path)
                }
                Some(s) => Err(MetaError::Conflict(format!("snapshot {seq} is {}", s.state.as_str()))),
                None => Err(MetaError::NotFound(format!("snapshot {seq}"))),
            }
        })?)
    }

    // ---------------------------------------------------------- maintenance

    /// Mark and sweep. A blob is deleted only when nothing has referred to it
    /// for at least `grace_ms`; that window protects content that was ingested
    /// but not yet added to a snapshot.
    pub fn gc(&self, grace_ms: i64) -> Result<GcReport> {
        let now = now_ms();
        let mut report = GcReport { orphaned: self.meta.write(|t| t.gc_mark(now))?, ..Default::default() };
        loop {
            // Order matters: the row turns `deleting` durably before the bytes
            // go, so a crash can never leave a `present` row without bytes.
            let batch = self.meta.write(|t| t.gc_claim(now - grace_ms, 500))?;
            if batch.is_empty() {
                break;
            }
            for d in batch {
                if let Some(stat) = self.blobs.stat(&d)? {
                    report.bytes_freed += stat.size;
                }
                self.blobs.delete(&d)?;
                self.meta.write(|t| t.gc_finish(&d))?;
                report.deleted += 1;
            }
        }
        if report.deleted > 0 {
            self.meta.write(|t| {
                t.audit(now_ms(), "system", "gc", &format!("{} blobs, {} bytes", report.deleted, report.bytes_freed))
            })?;
        }
        Ok(report)
    }

    /// Retention: forget history of `repo` older than snapshot `keep_from`.
    pub fn compact(&self, repo: &str, keep_from: u64, actor: &str) -> Result<u64> {
        Ok(self.meta.write(|t| {
            let r = t.repo_require(repo)?;
            let n = t.compact(r.id, keep_from)?;
            t.audit(now_ms(), actor, "repo.compact", &format!("{repo} keep_from {keep_from}"))?;
            Ok(n)
        })?)
    }

    /// Compare the database with the store. `deep` re-hashes every blob.
    pub fn fsck(&self, deep: bool) -> Result<FsckReport> {
        let mut report = FsckReport::default();
        for stat in self.blobs.list()? {
            match self.meta.read(|t| t.blob_get(&stat.digest))? {
                None => report.unknown.push(stat.digest),
                Some(row) => {
                    report.checked += 1;
                    if row.size != stat.size {
                        report.corrupt.push(stat.digest);
                    } else if deep {
                        let mut h = Hasher::new();
                        let mut r = self.blobs.open(&stat.digest, None)?;
                        let mut buf = vec![0u8; 128 * 1024];
                        loop {
                            let n = r.read(&mut buf)?;
                            if n == 0 {
                                break;
                            }
                            h.update(&buf[..n]);
                        }
                        if h.finish() != stat.digest {
                            report.corrupt.push(stat.digest);
                        }
                    }
                }
            }
        }
        // Rows whose bytes are gone: walk the snapshots' view of the world.
        let repos = self.meta.read(|t| t.repo_list())?;
        let mut seen = std::collections::BTreeSet::new();
        for repo in repos {
            if let Some(head) = self.meta.read(|t| t.snapshot_head(repo.id))? {
                for e in self.meta.read(|t| t.entries_at(repo.id, head.seq))? {
                    if seen.insert(e.digest) && self.blobs.stat(&e.digest)?.is_none() {
                        report.missing.push(e.digest);
                    }
                }
            }
        }
        Ok(report)
    }
}

/// Builds exactly one snapshot. Changes are buffered and written in batches,
/// so a million entries cost a few thousand transactions, not a million.
pub struct SnapshotWriter<'a> {
    st: &'a Storage,
    repo: Repo,
    seq: u64,
    pending: Vec<(String, Option<(EntryRole, Digest)>)>,
    done: bool,
}

const BATCH: usize = 1000;

impl SnapshotWriter<'_> {
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn put(&mut self, path: &str, role: EntryRole, digest: &Digest) -> Result<()> {
        mer_core::validate_path(path).map_err(MetaError::from)?;
        self.pending.push((path.to_string(), Some((role, *digest))));
        if self.pending.len() >= BATCH {
            self.flush()?;
        }
        Ok(())
    }

    pub fn remove(&mut self, path: &str) -> Result<()> {
        self.pending.push((path.to_string(), None));
        if self.pending.len() >= BATCH {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let (repo_id, seq) = (self.repo.id, self.seq);
        let batch = std::mem::take(&mut self.pending);
        self.st.meta.write(|t| {
            for (path, change) in &batch {
                match change {
                    Some((role, digest)) => {
                        t.put_entry(repo_id, seq, path, *role, digest)?;
                    }
                    None => {
                        t.remove_entry(repo_id, seq, path)?;
                    }
                }
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Make the snapshot immutable. Returns `None` (and leaves no snapshot
    /// behind) when it would be identical to the previous one.
    pub fn seal(mut self, chain_root: Option<SnapshotId>) -> Result<Option<SnapshotRow>> {
        self.flush()?;
        self.done = true;
        let (repo, seq) = (self.repo.clone(), self.seq);
        let out = self.st.meta.write(|t| {
            if t.snapshot_mutations(repo.id, seq)? == 0 {
                t.snapshot_abort(repo.id, seq)?;
                return Ok(Ok(None));
            }
            if let Some(quota) = repo.quota_bytes {
                let usage = t.repo_usage(repo.id)?;
                if usage > quota {
                    t.snapshot_abort(repo.id, seq)?;
                    return Ok(Err((usage, quota)));
                }
            }
            let row = t.snapshot_seal(repo.id, seq, chain_root, now_ms())?;
            t.audit(now_ms(), "system", "snapshot.seal", &format!("{}@{}", repo.name, seq))?;
            Ok(Ok(Some(row)))
        })?;
        out.map_err(|(usage, quota)| StorageError::QuotaExceeded { repo: repo.name, usage, quota })
    }

    pub fn abort(mut self) -> Result<()> {
        self.done = true;
        self.pending.clear();
        let (repo_id, seq) = (self.repo.id, self.seq);
        Ok(self.st.meta.write(|t| t.snapshot_abort(repo_id, seq))?)
    }
}

impl Drop for SnapshotWriter<'_> {
    fn drop(&mut self) {
        if !self.done {
            let (repo_id, seq) = (self.repo.id, self.seq);
            let _ = self.st.meta.write(|t| t.snapshot_abort(repo_id, seq));
        }
    }
}
