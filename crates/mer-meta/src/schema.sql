-- Migration 1. Timestamps are unix milliseconds. Digests are 32-byte BLOBs.

CREATE TABLE repo (
  id          INTEGER PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  format      TEXT NOT NULL,
  next_seq    INTEGER NOT NULL DEFAULT 1,
  quota_bytes INTEGER
);

-- state: 1 present, 2 deleting
CREATE TABLE blob (
  id          INTEGER PRIMARY KEY,
  digest      BLOB NOT NULL UNIQUE,
  size        INTEGER NOT NULL,
  state       INTEGER NOT NULL,
  created_at  INTEGER NOT NULL,
  orphaned_at INTEGER
);
CREATE INDEX blob_orphaned ON blob(orphaned_at) WHERE orphaned_at IS NOT NULL;

-- Extra GC roots that are not snapshot entries (ops objects of retained bundles).
CREATE TABLE blob_ref (
  owner  TEXT NOT NULL,
  digest BLOB NOT NULL,
  PRIMARY KEY (owner, digest)
) WITHOUT ROWID;
CREATE INDEX blob_ref_digest ON blob_ref(digest);

CREATE TABLE snapshot (
  repo_id     INTEGER NOT NULL REFERENCES repo(id),
  seq         INTEGER NOT NULL,
  state       TEXT NOT NULL,
  chain_root  BLOB,
  created_at  INTEGER NOT NULL,
  sealed_at   INTEGER,
  entry_count INTEGER NOT NULL DEFAULT 0,
  total_bytes INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (repo_id, seq)
) WITHOUT ROWID;

-- Interval-encoded membership: one row per change, not one per file per snapshot.
-- A path is in snapshot s when added_seq <= s and (removed_seq is null or removed_seq > s).
CREATE TABLE snapshot_entry (
  repo_id     INTEGER NOT NULL,
  path        TEXT NOT NULL,
  added_seq   INTEGER NOT NULL,
  removed_seq INTEGER,
  role        INTEGER NOT NULL,
  blob_id     INTEGER NOT NULL REFERENCES blob(id),
  PRIMARY KEY (repo_id, path, added_seq)
) WITHOUT ROWID;
CREATE INDEX entry_added   ON snapshot_entry(repo_id, added_seq);
CREATE INDEX entry_removed ON snapshot_entry(repo_id, removed_seq) WHERE removed_seq IS NOT NULL;
CREATE INDEX entry_blob    ON snapshot_entry(blob_id);

CREATE TABLE publish_pointer (
  repo_id      INTEGER PRIMARY KEY REFERENCES repo(id),
  snapshot_seq INTEGER NOT NULL,
  prev_seq     INTEGER,
  generation   INTEGER NOT NULL,
  kind         TEXT NOT NULL,
  updated_at   INTEGER NOT NULL
);

CREATE TABLE publish_event (
  id       INTEGER PRIMARY KEY,
  repo_id  INTEGER NOT NULL,
  from_seq INTEGER,
  to_seq   INTEGER NOT NULL,
  kind     TEXT NOT NULL,
  at       INTEGER NOT NULL
);

-- A channel is one collector -> vault stream of bundles.
CREATE TABLE channel (
  id           BLOB PRIMARY KEY,
  epoch        INTEGER NOT NULL,
  head_seq     INTEGER NOT NULL DEFAULT 0,   -- collector: last sealed bundle
  imported_seq INTEGER NOT NULL DEFAULT 0    -- vault: highest imported bundle
) WITHOUT ROWID;

-- Collector side.
CREATE TABLE bundle (
  channel    BLOB NOT NULL,
  seq        INTEGER NOT NULL,
  bundle_id  BLOB NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (channel, seq)
) WITHOUT ROWID;

CREATE TABLE bundle_step (
  channel         BLOB NOT NULL,
  repo_id         INTEGER NOT NULL,
  bundle_seq      INTEGER NOT NULL,
  snapshot_seq    INTEGER NOT NULL,
  prev_bundle_seq INTEGER NOT NULL,
  prev_root       BLOB NOT NULL,
  root            BLOB NOT NULL,
  ops             BLOB NOT NULL,
  op_count        INTEGER NOT NULL,
  added_bytes     INTEGER NOT NULL,
  PRIMARY KEY (channel, repo_id, bundle_seq)
) WITHOUT ROWID;

-- What the collector assumes the vault already holds (no feedback needed).
CREATE TABLE channel_blob (
  channel   BLOB NOT NULL,
  digest    BLOB NOT NULL,
  added_seq INTEGER NOT NULL,
  PRIMARY KEY (channel, digest)
) WITHOUT ROWID;

-- Vault side.
CREATE TABLE bundle_import (
  channel    BLOB NOT NULL,
  seq        INTEGER NOT NULL,
  bundle_id  BLOB NOT NULL,
  state      TEXT NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (channel, seq)
) WITHOUT ROWID;

CREATE TABLE repo_import (
  channel     BLOB NOT NULL,
  repo_id     INTEGER NOT NULL,
  applied_seq INTEGER NOT NULL,
  chain_root  BLOB NOT NULL,
  state       TEXT NOT NULL,
  detail      TEXT NOT NULL DEFAULT '',
  PRIMARY KEY (channel, repo_id)
) WITHOUT ROWID;

-- Client requests the vault could not satisfy: the source of the wishlist.
CREATE TABLE miss (
  repo     TEXT NOT NULL,
  path     TEXT NOT NULL,
  count    INTEGER NOT NULL,
  first_at INTEGER NOT NULL,
  last_at  INTEGER NOT NULL,
  PRIMARY KEY (repo, path)
) WITHOUT ROWID;

-- Append-only, hash-chained.
CREATE TABLE audit_log (
  seq       INTEGER PRIMARY KEY,
  at        INTEGER NOT NULL,
  actor     TEXT NOT NULL,
  action    TEXT NOT NULL,
  detail    TEXT NOT NULL,
  prev_hash BLOB NOT NULL,
  hash      BLOB NOT NULL
);

CREATE TABLE meta_kv (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) WITHOUT ROWID;
