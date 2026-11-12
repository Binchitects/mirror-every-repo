//! Bundle channels (collector and vault side), misses, audit log, key-value.
use crate::{dig, snap_id, MetaError, Result, Tx};
use mer_core::{BundleId, ChannelId, Digest, Hasher, SnapshotId};
use rusqlite::{params, OptionalExtension};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelRow {
    pub id: ChannelId,
    pub epoch: u32,
    pub head_seq: u64,
    pub imported_seq: u64,
}

/// One shipped change set of one repo (collector ledger).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepRow {
    pub repo_id: i64,
    pub bundle_seq: u64,
    pub snapshot_seq: u64,
    pub prev_bundle_seq: u64,
    pub prev_root: SnapshotId,
    pub root: SnapshotId,
    pub ops: Digest,
    pub op_count: u32,
    pub added_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleImport {
    pub seq: u64,
    pub bundle_id: BundleId,
    pub state: String,
}

/// How far a repo has been applied on the vault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoImport {
    pub applied_seq: u64,
    pub chain_root: SnapshotId,
    pub state: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Miss {
    pub repo: String,
    pub path: String,
    pub count: u64,
    pub last_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRow {
    pub seq: u64,
    pub at: i64,
    pub actor: String,
    pub action: String,
    pub detail: String,
}

type RawStep = (i64, i64, i64, i64, Vec<u8>, Vec<u8>, Vec<u8>, i64, i64);

const STEP_COLS: &str =
    "repo_id, bundle_seq, snapshot_seq, prev_bundle_seq, prev_root, root, ops, op_count, added_bytes";

fn raw_step(r: &rusqlite::Row) -> rusqlite::Result<RawStep> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))
}

fn step_row(raw: RawStep) -> Result<StepRow> {
    Ok(StepRow {
        repo_id: raw.0,
        bundle_seq: raw.1 as u64,
        snapshot_seq: raw.2 as u64,
        prev_bundle_seq: raw.3 as u64,
        prev_root: snap_id(raw.4)?,
        root: snap_id(raw.5)?,
        ops: dig(raw.6)?,
        op_count: raw.7 as u32,
        added_bytes: raw.8 as u64,
    })
}

fn audit_hash(prev: &Digest, seq: i64, at: i64, actor: &str, action: &str, detail: &str) -> Digest {
    let mut h = Hasher::new();
    h.update(b"mer/audit/v1\0");
    h.update(&prev.0);
    h.update(&seq.to_be_bytes());
    h.update(&at.to_be_bytes());
    for s in [actor, action, detail] {
        h.update(&(s.len() as u64).to_be_bytes());
        h.update(s.as_bytes());
    }
    h.finish()
}

impl Tx<'_> {
    // ------------------------------------------------------------- channels

    /// Create the channel if needed. The epoch of an existing channel never changes here.
    pub fn channel_ensure(&self, id: &ChannelId, epoch: u32) -> Result<ChannelRow> {
        self.c.execute(
            "INSERT OR IGNORE INTO channel(id, epoch) VALUES (?1, ?2)",
            params![id.as_bytes(), epoch],
        )?;
        self.channel_get(id)?.ok_or(MetaError::Corrupt("channel vanished"))
    }

    pub fn channel_get(&self, id: &ChannelId) -> Result<Option<ChannelRow>> {
        Ok(self
            .c
            .query_row(
                "SELECT epoch, head_seq, imported_seq FROM channel WHERE id = ?1",
                params![id.as_bytes()],
                |r| {
                    Ok(ChannelRow {
                        id: *id,
                        epoch: r.get(0)?,
                        head_seq: r.get::<_, i64>(1)? as u64,
                        imported_seq: r.get::<_, i64>(2)? as u64,
                    })
                },
            )
            .optional()?)
    }

    pub fn channel_list(&self) -> Result<Vec<ChannelRow>> {
        let mut st = self.c.prepare("SELECT id, epoch, head_seq, imported_seq FROM channel ORDER BY id")?;
        let raws: Vec<(Vec<u8>, u32, i64, i64)> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        raws.into_iter()
            .map(|(id, epoch, head, imp)| {
                Ok(ChannelRow {
                    id: ChannelId::from_slice(&id).map_err(|_| MetaError::Corrupt("channel id"))?,
                    epoch,
                    head_seq: head as u64,
                    imported_seq: imp as u64,
                })
            })
            .collect()
    }

    pub fn channel_set_head(&self, id: &ChannelId, seq: u64) -> Result<()> {
        self.c.execute(
            "UPDATE channel SET head_seq = ?2 WHERE id = ?1",
            params![id.as_bytes(), seq as i64],
        )?;
        Ok(())
    }

    /// The imported position only ever moves forward.
    pub fn channel_advance_imported(&self, id: &ChannelId, seq: u64) -> Result<()> {
        self.c.execute(
            "UPDATE channel SET imported_seq = MAX(imported_seq, ?2) WHERE id = ?1",
            params![id.as_bytes(), seq as i64],
        )?;
        Ok(())
    }

    // ------------------------------------------------------- collector side

    pub fn bundle_insert(&self, ch: &ChannelId, seq: u64, id: &BundleId, now: i64) -> Result<()> {
        self.c.execute(
            "INSERT INTO bundle(channel, seq, bundle_id, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![ch.as_bytes(), seq as i64, id.as_bytes(), now],
        )?;
        Ok(())
    }

    pub fn step_insert(&self, ch: &ChannelId, s: &StepRow) -> Result<()> {
        self.c.execute(
            &format!("INSERT INTO bundle_step(channel, {STEP_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)"),
            params![
                ch.as_bytes(),
                s.repo_id,
                s.bundle_seq as i64,
                s.snapshot_seq as i64,
                s.prev_bundle_seq as i64,
                s.prev_root.as_bytes(),
                s.root.as_bytes(),
                s.ops.as_bytes(),
                s.op_count,
                s.added_bytes as i64
            ],
        )?;
        Ok(())
    }

    pub fn step_last(&self, ch: &ChannelId, repo_id: i64) -> Result<Option<StepRow>> {
        self.c
            .query_row(
                &format!(
                    "SELECT {STEP_COLS} FROM bundle_step WHERE channel = ?1 AND repo_id = ?2
                      ORDER BY bundle_seq DESC LIMIT 1"
                ),
                params![ch.as_bytes(), repo_id],
                raw_step,
            )
            .optional()?
            .map(step_row)
            .transpose()
    }

    /// Steps of a repo with bundle_seq > `after`, oldest first.
    pub fn steps_after(&self, ch: &ChannelId, repo_id: i64, after: u64) -> Result<Vec<StepRow>> {
        let mut st = self.c.prepare(&format!(
            "SELECT {STEP_COLS} FROM bundle_step
              WHERE channel = ?1 AND repo_id = ?2 AND bundle_seq > ?3 ORDER BY bundle_seq"
        ))?;
        let raws: Vec<RawStep> = st
            .query_map(params![ch.as_bytes(), repo_id, after as i64], raw_step)?
            .collect::<rusqlite::Result<_>>()?;
        raws.into_iter().map(step_row).collect()
    }

    pub fn channel_blob_has(&self, ch: &ChannelId, digest: &Digest) -> Result<bool> {
        Ok(self
            .c
            .query_row(
                "SELECT 1 FROM channel_blob WHERE channel = ?1 AND digest = ?2",
                params![ch.as_bytes(), digest.as_bytes()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn channel_blob_add(&self, ch: &ChannelId, digest: &Digest, seq: u64) -> Result<()> {
        self.c.execute(
            "INSERT OR IGNORE INTO channel_blob(channel, digest, added_seq) VALUES (?1, ?2, ?3)",
            params![ch.as_bytes(), digest.as_bytes(), seq as i64],
        )?;
        Ok(())
    }

    // ----------------------------------------------------------- vault side

    pub fn bundle_import_get(&self, ch: &ChannelId, seq: u64) -> Result<Option<BundleImport>> {
        let raw: Option<(Vec<u8>, String)> = self
            .c
            .query_row(
                "SELECT bundle_id, state FROM bundle_import WHERE channel = ?1 AND seq = ?2",
                params![ch.as_bytes(), seq as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        raw.map(|(id, state)| {
            Ok(BundleImport {
                seq,
                bundle_id: BundleId::from_slice(&id).map_err(|_| MetaError::Corrupt("bundle id"))?,
                state,
            })
        })
        .transpose()
    }

    pub fn bundle_import_set(
        &self,
        ch: &ChannelId,
        seq: u64,
        id: &BundleId,
        state: &str,
        now: i64,
    ) -> Result<()> {
        self.c.execute(
            "INSERT INTO bundle_import(channel, seq, bundle_id, state, updated_at) VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(channel, seq) DO UPDATE SET state = excluded.state, updated_at = excluded.updated_at",
            params![ch.as_bytes(), seq as i64, id.as_bytes(), state, now],
        )?;
        Ok(())
    }

    pub fn bundle_import_list(&self, ch: &ChannelId) -> Result<Vec<BundleImport>> {
        let mut st = self
            .c
            .prepare("SELECT seq, bundle_id, state FROM bundle_import WHERE channel = ?1 ORDER BY seq")?;
        let raws: Vec<(i64, Vec<u8>, String)> = st
            .query_map(params![ch.as_bytes()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        raws.into_iter()
            .map(|(seq, id, state)| {
                Ok(BundleImport {
                    seq: seq as u64,
                    bundle_id: BundleId::from_slice(&id).map_err(|_| MetaError::Corrupt("bundle id"))?,
                    state,
                })
            })
            .collect()
    }

    pub fn repo_import_get(&self, ch: &ChannelId, repo_id: i64) -> Result<Option<RepoImport>> {
        let raw: Option<(i64, Vec<u8>, String, String)> = self
            .c
            .query_row(
                "SELECT applied_seq, chain_root, state, detail FROM repo_import
                  WHERE channel = ?1 AND repo_id = ?2",
                params![ch.as_bytes(), repo_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        raw.map(|(seq, root, state, detail)| {
            Ok(RepoImport { applied_seq: seq as u64, chain_root: snap_id(root)?, state, detail })
        })
        .transpose()
    }

    pub fn repo_import_set(&self, ch: &ChannelId, repo_id: i64, r: &RepoImport) -> Result<()> {
        self.c.execute(
            "INSERT INTO repo_import(channel, repo_id, applied_seq, chain_root, state, detail)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(channel, repo_id) DO UPDATE SET applied_seq = excluded.applied_seq,
               chain_root = excluded.chain_root, state = excluded.state, detail = excluded.detail",
            params![ch.as_bytes(), repo_id, r.applied_seq as i64, r.chain_root.as_bytes(), r.state, r.detail],
        )?;
        Ok(())
    }

    // --------------------------------------------------------------- misses

    pub fn miss_record(&self, repo: &str, path: &str, now: i64) -> Result<()> {
        self.c.execute(
            "INSERT INTO miss(repo, path, count, first_at, last_at) VALUES (?1, ?2, 1, ?3, ?3)
             ON CONFLICT(repo, path) DO UPDATE SET count = count + 1, last_at = excluded.last_at",
            params![repo, path, now],
        )?;
        self.history_add("misses", repo, crate::hour_of(now), 1)?;
        Ok(())
    }

    /// Forget one wish. Returns whether it existed.
    pub fn miss_delete(&self, repo: &str, path: &str) -> Result<bool> {
        Ok(self.c.execute("DELETE FROM miss WHERE repo = ?1 AND path = ?2", params![repo, path])? > 0)
    }

    /// Forget every wish of one repository, or of all. Returns how many.
    pub fn miss_clear_all(&self, repo: Option<&str>) -> Result<usize> {
        Ok(match repo {
            Some(r) => self.c.execute("DELETE FROM miss WHERE repo = ?1", params![r])?,
            None => self.c.execute("DELETE FROM miss", [])?,
        })
    }

    /// Most requested first.
    pub fn miss_list(&self, limit: u32) -> Result<Vec<Miss>> {
        let mut st = self.c.prepare(
            "SELECT repo, path, count, last_at FROM miss ORDER BY count DESC, repo, path LIMIT ?1",
        )?;
        let rows = st.query_map(params![limit], |r| {
            Ok(Miss {
                repo: r.get(0)?,
                path: r.get(1)?,
                count: r.get::<_, i64>(2)? as u64,
                last_at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn miss_clear(&self, repo: &str, path: &str) -> Result<()> {
        self.c.execute("DELETE FROM miss WHERE repo = ?1 AND path = ?2", params![repo, path])?;
        Ok(())
    }

    // ---------------------------------------------------------------- audit

    /// Append to the hash-chained audit log, inside the caller's transaction.
    pub fn audit(&self, now: i64, actor: &str, action: &str, detail: &str) -> Result<u64> {
        let last: Option<(i64, Vec<u8>)> = self
            .c
            .query_row("SELECT seq, hash FROM audit_log ORDER BY seq DESC LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let (seq, prev) = match last {
            Some((s, h)) => (s + 1, dig(h)?),
            None => (1, Digest::ZERO),
        };
        let hash = audit_hash(&prev, seq, now, actor, action, detail);
        self.c.execute(
            "INSERT INTO audit_log(seq, at, actor, action, detail, prev_hash, hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![seq, now, actor, action, detail, prev.as_bytes(), hash.as_bytes()],
        )?;
        Ok(seq as u64)
    }

    pub fn audit_list(&self, limit: u32) -> Result<Vec<AuditRow>> {
        let mut st = self
            .c
            .prepare("SELECT seq, at, actor, action, detail FROM audit_log ORDER BY seq DESC LIMIT ?1")?;
        let rows = st.query_map(params![limit], |r| {
            Ok(AuditRow {
                seq: r.get::<_, i64>(0)? as u64,
                at: r.get(1)?,
                actor: r.get(2)?,
                action: r.get(3)?,
                detail: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Recompute the whole chain. Returns the number of rows, or the first bad seq.
    pub fn audit_verify(&self) -> Result<std::result::Result<u64, u64>> {
        let mut st = self.c.prepare(
            "SELECT seq, at, actor, action, detail, prev_hash, hash FROM audit_log ORDER BY seq",
        )?;
        let mut rows = st.query([])?;
        let mut prev = Digest::ZERO;
        let mut expect_seq = 1i64;
        while let Some(r) = rows.next()? {
            let seq: i64 = r.get(0)?;
            let at: i64 = r.get(1)?;
            let actor: String = r.get(2)?;
            let action: String = r.get(3)?;
            let detail: String = r.get(4)?;
            let prev_hash = dig(r.get(5)?)?;
            let hash = dig(r.get(6)?)?;
            if seq != expect_seq
                || prev_hash != prev
                || hash != audit_hash(&prev, seq, at, &actor, &action, &detail)
            {
                return Ok(Err(expect_seq.min(seq) as u64));
            }
            prev = hash;
            expect_seq += 1;
        }
        Ok(Ok((expect_seq - 1) as u64))
    }

    // ------------------------------------------------------------ key-value

    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .c
            .query_row("SELECT value FROM meta_kv WHERE key = ?1", params![key], |r| r.get(0))
            .optional()?)
    }

    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.c.execute(
            "INSERT INTO meta_kv(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn kv_delete(&self, key: &str) -> Result<()> {
        self.c.execute("DELETE FROM meta_kv WHERE key = ?1", params![key])?;
        Ok(())
    }

    /// Test hook: lets a test tamper with the audit log.
    #[doc(hidden)]
    pub fn raw_execute(&self, sql: &str) -> Result<usize> {
        Ok(self.c.execute(sql, [])?)
    }
}
