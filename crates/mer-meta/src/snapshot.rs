//! Repos, blobs, interval-encoded snapshots, publish pointer, GC, retention.
use crate::{dig, snap_id, MetaError, Result, Tx};
use mer_core::{validate_path, Digest, EntryOp, EntryRec, EntryRole, SnapshotId};
use rusqlite::{params, OptionalExtension};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    pub id: i64,
    pub name: String,
    pub format: String,
    pub quota_bytes: Option<u64>,
    /// Private repos are served only to clients presenting a valid token.
    pub private: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobState {
    Present,
    Deleting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobRow {
    pub id: i64,
    pub digest: Digest,
    pub size: u64,
    pub state: BlobState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapState {
    Building,
    Sealed,
    Approved,
    Rejected,
    Retired,
    Aborted,
}

impl SnapState {
    pub fn as_str(self) -> &'static str {
        match self {
            SnapState::Building => "building",
            SnapState::Sealed => "sealed",
            SnapState::Approved => "approved",
            SnapState::Rejected => "rejected",
            SnapState::Retired => "retired",
            SnapState::Aborted => "aborted",
        }
    }
    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "building" => SnapState::Building,
            "sealed" => SnapState::Sealed,
            "approved" => SnapState::Approved,
            "rejected" => SnapState::Rejected,
            "retired" => SnapState::Retired,
            "aborted" => SnapState::Aborted,
            _ => return Err(MetaError::Corrupt("snapshot state")),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRow {
    pub repo_id: i64,
    pub seq: u64,
    pub state: SnapState,
    pub chain_root: Option<SnapshotId>,
    pub created_at: i64,
    pub sealed_at: Option<i64>,
    pub entry_count: u64,
    pub total_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishKind {
    Publish,
    Rollback,
}

impl PublishKind {
    fn as_str(self) -> &'static str {
        match self {
            PublishKind::Publish => "publish",
            PublishKind::Rollback => "rollback",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Published {
    pub repo_id: i64,
    pub seq: u64,
    pub prev_seq: Option<u64>,
    pub generation: u64,
    pub kind: String,
}

const SNAP_COLS: &str =
    "repo_id, seq, state, chain_root, created_at, sealed_at, entry_count, total_bytes";

type RawSnap = (i64, i64, String, Option<Vec<u8>>, i64, Option<i64>, i64, i64);

fn raw_snap(r: &rusqlite::Row) -> rusqlite::Result<RawSnap> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))
}

fn snap_row(raw: RawSnap) -> Result<SnapshotRow> {
    Ok(SnapshotRow {
        repo_id: raw.0,
        seq: raw.1 as u64,
        state: SnapState::parse(&raw.2)?,
        chain_root: raw.3.map(snap_id).transpose()?,
        created_at: raw.4,
        sealed_at: raw.5,
        entry_count: raw.6 as u64,
        total_bytes: raw.7 as u64,
    })
}

fn entry_rec(path: String, role: i64, digest: Vec<u8>, size: i64) -> Result<EntryRec> {
    Ok(EntryRec {
        path,
        role: EntryRole::from_u8(role as u8).ok_or(MetaError::Corrupt("entry role"))?,
        digest: dig(digest)?,
        size: size as u64,
    })
}

/// A live or closed interval row of one path.
struct Interval {
    added_seq: i64,
    role: i64,
    blob_id: i64,
}

impl Tx<'_> {
    // ---------------------------------------------------------------- repos

    pub fn repo_create(&self, name: &str, format: &str) -> Result<Repo> {
        if name.is_empty()
            || name.len() > 128
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
            || name.starts_with('.')
        {
            return Err(MetaError::Invalid(format!("repo name {name:?}")));
        }
        // These are paths of the HTTP server itself.
        if ["ui", "api", "healthz", "metrics"].contains(&name) {
            return Err(MetaError::Invalid(format!("repo name {name:?} is reserved")));
        }
        if self.repo_get(name)?.is_some() {
            return Err(MetaError::Conflict(format!("repo {name} already exists")));
        }
        self.c.execute("INSERT INTO repo(name, format) VALUES (?1, ?2)", params![name, format])?;
        Ok(Repo { id: self.c.last_insert_rowid(), name: name.into(), format: format.into(), quota_bytes: None, private: false })
    }

    pub fn repo_get(&self, name: &str) -> Result<Option<Repo>> {
        Ok(self
            .c
            .query_row(
                "SELECT id, name, format, quota_bytes, access FROM repo WHERE name = ?1",
                params![name],
                |r| {
                    Ok(Repo {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        format: r.get(2)?,
                        quota_bytes: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                        private: r.get::<_, String>(4)? == "private",
                    })
                },
            )
            .optional()?)
    }

    pub fn repo_require(&self, name: &str) -> Result<Repo> {
        self.repo_get(name)?.ok_or_else(|| MetaError::NotFound(format!("repo {name}")))
    }

    pub fn repo_list(&self) -> Result<Vec<Repo>> {
        let mut st = self.c.prepare("SELECT id, name, format, quota_bytes, access FROM repo ORDER BY name")?;
        let rows = st.query_map([], |r| {
            Ok(Repo {
                id: r.get(0)?,
                name: r.get(1)?,
                format: r.get(2)?,
                quota_bytes: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                private: r.get::<_, String>(4)? == "private",
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn repo_set_private(&self, repo_id: i64, private: bool) -> Result<()> {
        self.c.execute(
            "UPDATE repo SET access = ?2 WHERE id = ?1",
            params![repo_id, if private { "private" } else { "public" }],
        )?;
        Ok(())
    }

    pub fn repo_set_quota(&self, repo_id: i64, quota: Option<u64>) -> Result<()> {
        self.c.execute(
            "UPDATE repo SET quota_bytes = ?2 WHERE id = ?1",
            params![repo_id, quota.map(|q| q as i64)],
        )?;
        Ok(())
    }

    /// Bytes referenced by the newest state of a repo (each blob counted once).
    pub fn repo_usage(&self, repo_id: i64) -> Result<u64> {
        let v: i64 = self.c.query_row(
            "SELECT COALESCE(SUM(size), 0) FROM blob WHERE id IN
               (SELECT blob_id FROM snapshot_entry WHERE repo_id = ?1 AND removed_seq IS NULL)",
            params![repo_id],
            |r| r.get(0),
        )?;
        Ok(v as u64)
    }

    // ---------------------------------------------------------------- blobs

    /// Record that the bytes of `digest` are durable in the store.
    pub fn blob_present(&self, digest: &Digest, size: u64, now: i64) -> Result<BlobRow> {
        match self.blob_get(digest)? {
            Some(b) if b.state == BlobState::Deleting => Err(MetaError::Busy(*digest)),
            Some(b) if b.size != size => Err(MetaError::Corrupt("blob size differs for one digest")),
            Some(b) => {
                self.c.execute("UPDATE blob SET orphaned_at = NULL WHERE id = ?1", params![b.id])?;
                Ok(b)
            }
            None => {
                self.c.execute(
                    "INSERT INTO blob(digest, size, state, created_at) VALUES (?1, ?2, 1, ?3)",
                    params![digest.as_bytes(), size as i64, now],
                )?;
                Ok(BlobRow {
                    id: self.c.last_insert_rowid(),
                    digest: *digest,
                    size,
                    state: BlobState::Present,
                })
            }
        }
    }

    pub fn blob_get(&self, digest: &Digest) -> Result<Option<BlobRow>> {
        let raw: Option<(i64, i64, i64)> = self
            .c
            .query_row(
                "SELECT id, size, state FROM blob WHERE digest = ?1",
                params![digest.as_bytes()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(raw.map(|(id, size, state)| BlobRow {
            id,
            digest: *digest,
            size: size as u64,
            state: if state == 1 { BlobState::Present } else { BlobState::Deleting },
        }))
    }

    pub fn blob_is_present(&self, digest: &Digest) -> Result<bool> {
        Ok(matches!(self.blob_get(digest)?, Some(b) if b.state == BlobState::Present))
    }

    pub fn blob_count(&self) -> Result<(u64, u64)> {
        let (n, bytes): (i64, i64) = self.c.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM blob WHERE state = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((n as u64, bytes as u64))
    }

    pub fn blob_ref_add(&self, owner: &str, digest: &Digest) -> Result<()> {
        self.c.execute(
            "INSERT OR IGNORE INTO blob_ref(owner, digest) VALUES (?1, ?2)",
            params![owner, digest.as_bytes()],
        )?;
        self.c.execute(
            "UPDATE blob SET orphaned_at = NULL WHERE digest = ?1",
            params![digest.as_bytes()],
        )?;
        Ok(())
    }

    pub fn blob_ref_clear(&self, owner: &str) -> Result<()> {
        self.c.execute("DELETE FROM blob_ref WHERE owner = ?1", params![owner])?;
        Ok(())
    }

    // ------------------------------------------------------------ snapshots

    /// Open the single building snapshot of a repo and return its seq.
    pub fn snapshot_begin(&self, repo_id: i64, now: i64) -> Result<u64> {
        if let Some(seq) = self.snapshot_building(repo_id)? {
            return Err(MetaError::Conflict(format!("snapshot {seq} is already being built")));
        }
        let seq: i64 =
            self.c.query_row("SELECT next_seq FROM repo WHERE id = ?1", params![repo_id], |r| r.get(0))?;
        self.c.execute("UPDATE repo SET next_seq = next_seq + 1 WHERE id = ?1", params![repo_id])?;
        self.c.execute(
            "INSERT INTO snapshot(repo_id, seq, state, created_at) VALUES (?1, ?2, 'building', ?3)",
            params![repo_id, seq, now],
        )?;
        Ok(seq as u64)
    }

    pub fn snapshot_building(&self, repo_id: i64) -> Result<Option<u64>> {
        Ok(self
            .c
            .query_row(
                "SELECT seq FROM snapshot WHERE repo_id = ?1 AND state = 'building'",
                params![repo_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(|v| v as u64))
    }

    fn require_building(&self, repo_id: i64, seq: u64) -> Result<()> {
        match self.snapshot_get(repo_id, seq)? {
            Some(s) if s.state == SnapState::Building => Ok(()),
            Some(s) => Err(MetaError::Conflict(format!(
                "snapshot {seq} is {} and can no longer change",
                s.state.as_str()
            ))),
            None => Err(MetaError::NotFound(format!("snapshot {seq}"))),
        }
    }

    fn live_interval(&self, repo_id: i64, path: &str) -> Result<Option<Interval>> {
        Ok(self
            .c
            .query_row(
                "SELECT added_seq, role, blob_id FROM snapshot_entry
                  WHERE repo_id = ?1 AND path = ?2 AND removed_seq IS NULL",
                params![repo_id, path],
                |r| Ok(Interval { added_seq: r.get(0)?, role: r.get(1)?, blob_id: r.get(2)? }),
            )
            .optional()?)
    }

    /// The interval this building snapshot closed for `path`, if any.
    fn closed_here(&self, repo_id: i64, path: &str, seq: i64) -> Result<Option<Interval>> {
        Ok(self
            .c
            .query_row(
                "SELECT added_seq, role, blob_id FROM snapshot_entry
                  WHERE repo_id = ?1 AND path = ?2 AND removed_seq = ?3",
                params![repo_id, path, seq],
                |r| Ok(Interval { added_seq: r.get(0)?, role: r.get(1)?, blob_id: r.get(2)? }),
            )
            .optional()?)
    }

    /// Set `path` in the building snapshot `seq`. Returns false when nothing changed.
    pub fn put_entry(
        &self,
        repo_id: i64,
        seq: u64,
        path: &str,
        role: EntryRole,
        digest: &Digest,
    ) -> Result<bool> {
        validate_path(path)?;
        self.require_building(repo_id, seq)?;
        let blob = match self.blob_get(digest)? {
            Some(b) if b.state == BlobState::Present => b,
            Some(_) => return Err(MetaError::Busy(*digest)),
            None => return Err(MetaError::Invalid(format!("blob {digest} is not in the store"))),
        };
        self.c.execute("UPDATE blob SET orphaned_at = NULL WHERE id = ?1", params![blob.id])?;
        let n = seq as i64;
        let role_i = role.as_u8() as i64;
        let same = |iv: &Interval| iv.blob_id == blob.id && iv.role == role_i;
        let live = self.live_interval(repo_id, path)?;
        let closed = self.closed_here(repo_id, path, n)?;

        match live {
            Some(ref cur) if same(cur) => return Ok(false),
            Some(ref cur) if cur.added_seq < n => {
                self.c.execute(
                    "UPDATE snapshot_entry SET removed_seq = ?3
                      WHERE repo_id = ?1 AND path = ?2 AND added_seq = ?4",
                    params![repo_id, path, n, cur.added_seq],
                )?;
                self.insert_interval(repo_id, path, n, role_i, blob.id)?;
            }
            Some(_) => {
                // The live row was written by this same snapshot.
                if closed.as_ref().is_some_and(same) {
                    // Back to the value the previous snapshot had: undo both rows.
                    self.c.execute(
                        "DELETE FROM snapshot_entry WHERE repo_id = ?1 AND path = ?2 AND added_seq = ?3",
                        params![repo_id, path, n],
                    )?;
                    self.reopen(repo_id, path, n)?;
                } else {
                    self.c.execute(
                        "UPDATE snapshot_entry SET role = ?4, blob_id = ?5
                          WHERE repo_id = ?1 AND path = ?2 AND added_seq = ?3",
                        params![repo_id, path, n, role_i, blob.id],
                    )?;
                }
            }
            None => {
                if closed.as_ref().is_some_and(same) {
                    self.reopen(repo_id, path, n)?;
                } else {
                    self.insert_interval(repo_id, path, n, role_i, blob.id)?;
                }
            }
        }
        Ok(true)
    }

    fn insert_interval(&self, repo_id: i64, path: &str, n: i64, role: i64, blob_id: i64) -> Result<()> {
        self.c.execute(
            "INSERT INTO snapshot_entry(repo_id, path, added_seq, removed_seq, role, blob_id)
             VALUES (?1, ?2, ?3, NULL, ?4, ?5)",
            params![repo_id, path, n, role, blob_id],
        )?;
        Ok(())
    }

    fn reopen(&self, repo_id: i64, path: &str, n: i64) -> Result<()> {
        self.c.execute(
            "UPDATE snapshot_entry SET removed_seq = NULL
              WHERE repo_id = ?1 AND path = ?2 AND removed_seq = ?3",
            params![repo_id, path, n],
        )?;
        Ok(())
    }

    /// Remove `path` from the building snapshot `seq`. Returns false when it was absent.
    pub fn remove_entry(&self, repo_id: i64, seq: u64, path: &str) -> Result<bool> {
        self.require_building(repo_id, seq)?;
        let n = seq as i64;
        match self.live_interval(repo_id, path)? {
            None => Ok(false),
            Some(cur) if cur.added_seq == n => {
                self.c.execute(
                    "DELETE FROM snapshot_entry WHERE repo_id = ?1 AND path = ?2 AND added_seq = ?3",
                    params![repo_id, path, n],
                )?;
                Ok(true)
            }
            Some(cur) => {
                self.c.execute(
                    "UPDATE snapshot_entry SET removed_seq = ?3
                      WHERE repo_id = ?1 AND path = ?2 AND added_seq = ?4",
                    params![repo_id, path, n, cur.added_seq],
                )?;
                Ok(true)
            }
        }
    }

    /// Number of interval rows the building snapshot touched.
    pub fn snapshot_mutations(&self, repo_id: i64, seq: u64) -> Result<u64> {
        let v: i64 = self.c.query_row(
            "SELECT (SELECT COUNT(*) FROM snapshot_entry WHERE repo_id = ?1 AND added_seq = ?2)
                  + (SELECT COUNT(*) FROM snapshot_entry WHERE repo_id = ?1 AND removed_seq = ?2)",
            params![repo_id, seq as i64],
            |r| r.get(0),
        )?;
        Ok(v as u64)
    }

    /// Exact undo of everything a building snapshot did.
    pub fn snapshot_abort(&self, repo_id: i64, seq: u64) -> Result<()> {
        self.require_building(repo_id, seq)?;
        let n = seq as i64;
        self.c.execute(
            "DELETE FROM snapshot_entry WHERE repo_id = ?1 AND added_seq = ?2",
            params![repo_id, n],
        )?;
        self.c.execute(
            "UPDATE snapshot_entry SET removed_seq = NULL WHERE repo_id = ?1 AND removed_seq = ?2",
            params![repo_id, n],
        )?;
        self.c.execute(
            "UPDATE snapshot SET state = 'aborted' WHERE repo_id = ?1 AND seq = ?2",
            params![repo_id, n],
        )?;
        Ok(())
    }

    /// Make a building snapshot immutable.
    pub fn snapshot_seal(
        &self,
        repo_id: i64,
        seq: u64,
        chain_root: Option<SnapshotId>,
        now: i64,
    ) -> Result<SnapshotRow> {
        self.require_building(repo_id, seq)?;
        let n = seq as i64;
        let (count, bytes): (i64, i64) = self.c.query_row(
            "SELECT COUNT(*), COALESCE(SUM(b.size), 0)
               FROM snapshot_entry e JOIN blob b ON b.id = e.blob_id
              WHERE e.repo_id = ?1 AND e.added_seq <= ?2 AND (e.removed_seq IS NULL OR e.removed_seq > ?2)",
            params![repo_id, n],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let bad: i64 = self.c.query_row(
            "SELECT COUNT(*) FROM snapshot_entry e JOIN blob b ON b.id = e.blob_id
              WHERE e.repo_id = ?1 AND e.added_seq = ?2 AND b.state <> 1",
            params![repo_id, n],
            |r| r.get(0),
        )?;
        if bad > 0 {
            return Err(MetaError::Invalid(format!("{bad} entries reference blobs that are not present")));
        }
        self.c.execute(
            "UPDATE snapshot SET state = 'sealed', chain_root = ?3, sealed_at = ?4,
                    entry_count = ?5, total_bytes = ?6
              WHERE repo_id = ?1 AND seq = ?2",
            params![repo_id, n, chain_root.map(|r| r.0.to_vec()), now, count, bytes],
        )?;
        self.snapshot_get(repo_id, seq)?.ok_or(MetaError::Corrupt("sealed snapshot vanished"))
    }

    pub fn snapshot_get(&self, repo_id: i64, seq: u64) -> Result<Option<SnapshotRow>> {
        self.c
            .query_row(
                &format!("SELECT {SNAP_COLS} FROM snapshot WHERE repo_id = ?1 AND seq = ?2"),
                params![repo_id, seq as i64],
                raw_snap,
            )
            .optional()?
            .map(snap_row)
            .transpose()
    }

    /// Newest immutable, usable snapshot (sealed or approved).
    pub fn snapshot_head(&self, repo_id: i64) -> Result<Option<SnapshotRow>> {
        self.c
            .query_row(
                &format!(
                    "SELECT {SNAP_COLS} FROM snapshot
                      WHERE repo_id = ?1 AND state IN ('sealed', 'approved')
                      ORDER BY seq DESC LIMIT 1"
                ),
                params![repo_id],
                raw_snap,
            )
            .optional()?
            .map(snap_row)
            .transpose()
    }

    pub fn snapshot_list(&self, repo_id: i64) -> Result<Vec<SnapshotRow>> {
        let mut st = self
            .c
            .prepare(&format!("SELECT {SNAP_COLS} FROM snapshot WHERE repo_id = ?1 ORDER BY seq"))?;
        let raws: Vec<RawSnap> =
            st.query_map(params![repo_id], raw_snap)?.collect::<rusqlite::Result<_>>()?;
        raws.into_iter().map(snap_row).collect()
    }

    /// sealed -> approved | rejected.
    pub fn snapshot_decide(&self, repo_id: i64, seq: u64, approve: bool) -> Result<()> {
        let snap = self
            .snapshot_get(repo_id, seq)?
            .ok_or_else(|| MetaError::NotFound(format!("snapshot {seq}")))?;
        if snap.state != SnapState::Sealed {
            return Err(MetaError::Conflict(format!(
                "snapshot {seq} is {}, only a sealed snapshot can be decided",
                snap.state.as_str()
            )));
        }
        let to = if approve { SnapState::Approved } else { SnapState::Rejected };
        self.c.execute(
            "UPDATE snapshot SET state = ?3 WHERE repo_id = ?1 AND seq = ?2",
            params![repo_id, seq as i64, to.as_str()],
        )?;
        Ok(())
    }

    pub fn lookup(&self, repo_id: i64, seq: u64, path: &str) -> Result<Option<EntryRec>> {
        let raw: Option<(i64, Vec<u8>, i64)> = self
            .c
            .query_row(
                "SELECT e.role, b.digest, b.size
                   FROM snapshot_entry e JOIN blob b ON b.id = e.blob_id
                  WHERE e.repo_id = ?1 AND e.path = ?2 AND e.added_seq <= ?3
                    AND (e.removed_seq IS NULL OR e.removed_seq > ?3)
                  ORDER BY e.added_seq DESC LIMIT 1",
                params![repo_id, path, seq as i64],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        raw.map(|(role, digest, size)| entry_rec(path.to_string(), role, digest, size)).transpose()
    }

    /// Every entry of snapshot `seq`, sorted by path bytes.
    pub fn entries_at(&self, repo_id: i64, seq: u64) -> Result<Vec<EntryRec>> {
        let mut st = self.c.prepare(
            "SELECT e.path, e.role, b.digest, b.size
               FROM snapshot_entry e JOIN blob b ON b.id = e.blob_id
              WHERE e.repo_id = ?1 AND e.added_seq <= ?2
                AND (e.removed_seq IS NULL OR e.removed_seq > ?2)
              ORDER BY e.path",
        )?;
        let raws: Vec<(String, i64, Vec<u8>, i64)> = st
            .query_map(params![repo_id, seq as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        raws.into_iter().map(|(p, r, d, s)| entry_rec(p, r, d, s)).collect()
    }

    /// Changes that turn snapshot `from` into snapshot `to`, sorted by path.
    /// Cost is proportional to the number of changed paths, not to repo size.
    pub fn diff(&self, repo_id: i64, from: u64, to: u64) -> Result<Vec<EntryOp>> {
        // Works in both directions: forwards for "what would publishing
        // change", backwards for "what would a rollback change".
        let (lo, hi) = (from.min(to), from.max(to));
        let mut st = self.c.prepare(
            "SELECT DISTINCT path FROM snapshot_entry
              WHERE repo_id = ?1
                AND ((added_seq > ?2 AND added_seq <= ?3) OR (removed_seq > ?2 AND removed_seq <= ?3))
              ORDER BY path",
        )?;
        let paths: Vec<String> = st
            .query_map(params![repo_id, lo as i64, hi as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut ops = Vec::new();
        for path in paths {
            let before = self.lookup(repo_id, from, &path)?;
            let after = self.lookup(repo_id, to, &path)?;
            match (before, after) {
                (b, Some(a)) if b.as_ref() != Some(&a) => ops.push(EntryOp::Put(a)),
                (Some(_), None) => ops.push(EntryOp::Del { path }),
                _ => {}
            }
        }
        Ok(ops)
    }

    // -------------------------------------------------------------- publish

    pub fn live(&self, repo_id: i64) -> Result<Option<Published>> {
        Ok(self
            .c
            .query_row(
                "SELECT snapshot_seq, prev_seq, generation, kind FROM publish_pointer WHERE repo_id = ?1",
                params![repo_id],
                |r| {
                    Ok(Published {
                        repo_id,
                        seq: r.get::<_, i64>(0)? as u64,
                        prev_seq: r.get::<_, Option<i64>>(1)?.map(|v| v as u64),
                        generation: r.get::<_, i64>(2)? as u64,
                        kind: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Move the live pointer. No data moves: publish and rollback are one row update.
    pub fn publish(&self, repo_id: i64, seq: u64, kind: PublishKind, now: i64) -> Result<Published> {
        let snap = self
            .snapshot_get(repo_id, seq)?
            .ok_or_else(|| MetaError::NotFound(format!("snapshot {seq}")))?;
        if snap.state != SnapState::Approved {
            return Err(MetaError::Conflict(format!(
                "snapshot {seq} is {}, only an approved snapshot can go live",
                snap.state.as_str()
            )));
        }
        let cur = self.live(repo_id)?;
        match (&cur, kind) {
            (Some(c), PublishKind::Publish) if seq <= c.seq => {
                return Err(MetaError::Conflict(format!(
                    "snapshot {} is live; going back to {seq} needs a rollback",
                    c.seq
                )))
            }
            (Some(c), PublishKind::Rollback) if seq >= c.seq => {
                return Err(MetaError::Conflict(format!(
                    "rollback target {seq} is not older than live snapshot {}",
                    c.seq
                )))
            }
            (None, PublishKind::Rollback) => {
                return Err(MetaError::Conflict("nothing is live, nothing to roll back".into()))
            }
            _ => {}
        }
        let from = cur.as_ref().map(|c| c.seq as i64);
        self.c.execute(
            "INSERT INTO publish_pointer(repo_id, snapshot_seq, prev_seq, generation, kind, updated_at)
             VALUES (?1, ?2, NULL, 1, ?3, ?4)
             ON CONFLICT(repo_id) DO UPDATE SET
               prev_seq = snapshot_seq, snapshot_seq = excluded.snapshot_seq,
               generation = generation + 1, kind = excluded.kind, updated_at = excluded.updated_at",
            params![repo_id, seq as i64, kind.as_str(), now],
        )?;
        self.c.execute(
            "INSERT INTO publish_event(repo_id, from_seq, to_seq, kind, at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![repo_id, from, seq as i64, kind.as_str(), now],
        )?;
        self.live(repo_id)?.ok_or(MetaError::Corrupt("publish pointer vanished"))
    }

    // ------------------------------------------------------------------- GC

    /// Phase 1: stamp blobs nothing refers to, and clear the stamp where a
    /// reference has appeared since. Returns how many are currently stamped.
    pub fn gc_mark(&self, now: i64) -> Result<u64> {
        const UNREFERENCED: &str = "NOT EXISTS (SELECT 1 FROM snapshot_entry e WHERE e.blob_id = blob.id)
             AND NOT EXISTS (SELECT 1 FROM blob_ref r WHERE r.digest = blob.digest)";
        self.c.execute(
            &format!("UPDATE blob SET orphaned_at = NULL WHERE orphaned_at IS NOT NULL AND NOT ({UNREFERENCED})"),
            [],
        )?;
        self.c.execute(
            &format!("UPDATE blob SET orphaned_at = ?1 WHERE orphaned_at IS NULL AND state = 1 AND {UNREFERENCED}"),
            params![now],
        )?;
        let n: i64 =
            self.c.query_row("SELECT COUNT(*) FROM blob WHERE orphaned_at IS NOT NULL", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Phase 2: claim blobs orphaned since before `cutoff`. Claimed rows are
    /// `deleting`; the caller deletes the bytes, then calls `gc_finish`.
    pub fn gc_claim(&self, cutoff: i64, limit: u32) -> Result<Vec<Digest>> {
        let mut st = self.c.prepare(
            "SELECT id, digest FROM blob
              WHERE state = 1 AND orphaned_at IS NOT NULL AND orphaned_at <= ?1
                AND NOT EXISTS (SELECT 1 FROM snapshot_entry e WHERE e.blob_id = blob.id)
                AND NOT EXISTS (SELECT 1 FROM blob_ref r WHERE r.digest = blob.digest)
              LIMIT ?2",
        )?;
        let rows: Vec<(i64, Vec<u8>)> = st
            .query_map(params![cutoff, limit], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, digest) in rows {
            self.c.execute("UPDATE blob SET state = 2 WHERE id = ?1", params![id])?;
            out.push(dig(digest)?);
        }
        Ok(out)
    }

    /// Rows left `deleting` by a crash between claim and finish.
    pub fn gc_deleting(&self) -> Result<Vec<Digest>> {
        let mut st = self.c.prepare("SELECT digest FROM blob WHERE state = 2")?;
        let rows: Vec<Vec<u8>> = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        rows.into_iter().map(dig).collect()
    }

    pub fn gc_finish(&self, digest: &Digest) -> Result<()> {
        self.c.execute("DELETE FROM blob WHERE digest = ?1 AND state = 2", params![digest.as_bytes()])?;
        Ok(())
    }

    // ------------------------------------------------------------ retention

    /// Drop history older than `keep_from`: snapshots below it are retired and
    /// can no longer be looked up; their exclusive blobs become collectable.
    pub fn compact(&self, repo_id: i64, keep_from: u64) -> Result<u64> {
        if let Some(live) = self.live(repo_id)? {
            if keep_from > live.seq {
                return Err(MetaError::Conflict(format!(
                    "cannot retire the live snapshot {} (keep_from {keep_from})",
                    live.seq
                )));
            }
        }
        let head = self.snapshot_head(repo_id)?.map(|s| s.seq).unwrap_or(0);
        if keep_from > head {
            return Err(MetaError::Conflict(format!("keep_from {keep_from} is beyond head {head}")));
        }
        let bundled: Option<i64> = self.c.query_row(
            "SELECT MIN(m) FROM (SELECT MAX(snapshot_seq) AS m FROM bundle_step
                                  WHERE repo_id = ?1 GROUP BY channel)",
            params![repo_id],
            |r| r.get(0),
        )?;
        if let Some(b) = bundled {
            if keep_from > b as u64 {
                return Err(MetaError::Conflict(format!(
                    "snapshot {b} is the base of the next export delta and must be kept"
                )));
            }
        }
        let k = keep_from as i64;
        let removed = self.c.execute(
            "DELETE FROM snapshot_entry WHERE repo_id = ?1 AND removed_seq IS NOT NULL AND removed_seq <= ?2",
            params![repo_id, k],
        )?;
        self.c.execute(
            "UPDATE snapshot SET state = 'retired'
              WHERE repo_id = ?1 AND seq < ?2 AND state IN ('sealed', 'approved', 'rejected')",
            params![repo_id, k],
        )?;
        Ok(removed as u64)
    }
}
