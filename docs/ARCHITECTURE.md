# mirror-every-repo: Architecture

Status: authoritative design, reconciled from five subsystem drafts and three adversarial reviews. Nothing has been compiled (no Rust toolchain on the authoring machine). Crate versions are stated where known; items marked "(verify)" must be confirmed with `cargo add` at workspace init, after which `Cargo.lock` is the pin. Binary name `mer`, crate prefix `mer-`, metric prefix `mer_`, environment prefix `MER__`.

This document is the target design. What is built so far, and where the code is deliberately simpler than this document, is listed in `docs/STATUS.md`. Where the two disagree about the current code, `STATUS.md` wins.

## 1. Overview and data flow

One binary, two roles.

- **collector** (internet side): mirrors upstreams selectively (allowlist plus dependency closure), verifies upstream signatures, stores everything in a content-addressed store, seals signed delta bundles, sends them over removable media and/or a data diode. It never needs feedback from the vault.
- **vault** (air-gapped side): receives segments from any transport, verifies, ingests blobs, applies per-repo snapshot steps, runs the quarantine gate, publishes immutable snapshots atomically, serves package clients, records misses, exports a small signed report (state plus wishlist) that may be carried back by hand.

```
upstreams --fetch--> [collector: CAS + meta.db] --seal--> sealed bundle (segments + Merkle + signed descriptor)
                                                     |-- media volumes (.mab) --hand carry--> [vault assembler]
                                                     |-- UDP diode (RaptorQ, carousel) -----> [vault assembler]
[vault assembler] -> staging -> import worker -> CAS + per-repo snapshot steps -> verify -> reindex -> gate
                 -> approve -> publish (pointer CAS, site signature) -> serve apt/PyPI/npm/OCI/raw clients
[vault] misses + import state -> signed report (.mar) --hand carry (optional)--> [collector] -> next bundle
```

Core ideas that every subsystem relies on:

1. **One content-addressed blob store** (sha256) shared by all formats. Blobs of 8 KiB or less are stored inline in SQLite (`small.db`); the rest are files or S3 objects.
2. **One snapshot model**: per-repo linear history, interval-encoded membership (`snapshot_entry`), identified across the gap by a chained root (`SnapshotId`).
3. **One bundle encoding**: a canonical byte stream cut into fixed-size segments, each individually verifiable against a Merkle root in one signed descriptor. Every transport carries the same segments, so a bundle that arrived 97 percent over the diode can be completed from a USB stick.
4. **Chained per-step deltas**: each bundle carries, per repo, the ops of the last `step_window` bundle steps, each relative to the previous step. The vault applies only steps beyond its own head. Verification and seal are O(delta).
5. **Transport unit and apply unit are separate**: the channel position advances when a bundle's blobs and manifest are verified; each repo is then applied independently, so one bad repo never stalls the others.
6. **Reject never reverts content**. It writes denylist rows; denylisted entries are never served and taint any snapshot that contains them.

## 2. Processes and trust boundaries

Untrusted input is parsed in processes that hold no keys and listen on no sockets.

| Role | Process | Network | Keys | Duties |
|---|---|---|---|---|
| collector | `mer collector serve` | upstream egress, admin listener | none | scheduler, upstream fetch, resolve, sync snapshots, dashboard, report ingest |
| collector | `mer collector export` | diode TX interface only (or none) | Bundle signing key via `Signer` (file with separate uid in MVP, PKCS#11 later) | seal, sign, media export, diode carousel |
| vault | `mer vault serve` | package listener, admin listener, metrics listener | registry JWT key, session secrets | HTTP serving, auth, RBAC, publish, GC, retention, reports |
| vault | `mer vault worker` | diode RX socket only (UDP, receive) | none (trust anchors are public keys) | transports RX, assembler, import, plugin verify, reindex. Runs with seccomp/landlock, `RLIMIT_AS`, `RLIMIT_CPU`, read-write on staging and CAS, no key files |
| vault | `mer keyd` | unix socket `$DATA/run/keyd.sock` | site OpenPGP key, preview key, audit checkpoint key, Report key | signs only what it can re-derive from the DB: `sign_site(repo, snapshot_seq)`, `sign_checkpoint(seq)`, `sign_report(report_seq)` |

Rules:

- All processes of one data directory share `meta.db` through SQLite WAL. Each process has exactly one writer connection on one dedicated thread (section 4.3). Cross-process exclusion is the SQLite write lock with `BEGIN IMMEDIATE`.
- At most one `serve` and one `worker` (or `export`) per data directory, enforced by `std::fs::File::try_lock` on `$DATA/run/{serve,worker}.lock`.
- GC, retention, publish and anything that consults the in-process snapshot lease registry run only in `serve`. The CLI talks to `serve` over the admin API (unix socket `$DATA/run/admin.sock`), except offline commands (`mer meta backup|rebuild`, `mer fsck`) which take both locks.
- The worker never auto-resumes a bundle whose import crashed twice: `bundle_import.attempts` is incremented before each resume, and at 2 the bundle moves to `Failed{crashed}`. A crafted bundle therefore cannot produce a restart loop, and it never takes package serving down because serving is a different process.
- Release profile uses `panic = "unwind"` so the HTTP `CatchPanic` layer works; the worker additionally wraps each bundle and each plugin call in `catch_unwind`.
- The collector fetch process cannot sign. In the compose demo `collector-serve` is on the internet network and `collector-export` is not.

## 3. Crate map

```
mirror-every-repo/
  Cargo.toml  Cargo.lock  rust-toolchain.toml  deny.toml  clippy.toml  rustfmt.toml
  crates/
    mer-core/  mer-crypto/  mer-fec/  mer-blob/  mer-meta/  mer-bundle-format/
    mer-storage/  mer-transport-media/  mer-transport-diode/  mer-bundle/
    mer-format/  mer-format-raw/  mer-format-apt/  mer-format-pypi/  mer-format-npm/  mer-format-oci/
    mer-fetch/  mer-auth/  mer-http/  mer-collector/  mer-vault/
    mer-testkit/   (publish = false, dev-dependency only)
    mer/           (the single binary)
  xtask/  fuzz/  tests/e2e/  deploy/  Dockerfile  compose.yaml  config/{collector,vault}.toml
```

| Layer | Crate | Owns | Depends on (workspace) |
|---|---|---|---|
| L0 | `mer-core` | Every cross-crate plain data type: `Digest`, ids, `Position`, entry model, wire structs (`Descriptor`, `Manifest`, `TrustDoc`, `Report`, `Heartbeat`), config structs, `Clock`, `ErrorKind`. No I/O, no tokio. | none |
| L1 | `mer-crypto` | Ed25519 keys, fixed binary signed envelope, role-scoped `TrustStore`, `Signer`, link MAC | core |
| L1 | `mer-fec` | `BlockCodec` trait, RaptorQ implementation. Pure CPU, sync. | core |
| L1 | `mer-blob` | `BlobStore` trait, fs, mem and S3 backends, conformance suite | core |
| L2 | `mer-meta` | `Meta` (rusqlite, single writer thread), `ReadTx`/`WriteTx`, all DDL and migrations | core |
| L2 | `mer-bundle-format` | CBOR codecs with limits, slot codec, Merkle tree, blob index, stream layout, `SegmentSource`/`SegmentSink` traits, report codec. No filesystem or network I/O. | core, crypto |
| L3 | `mer-storage` | Facade over blob + meta: ingest windows, snapshot writer, seal, publish, lookup cache, denylist, GC, quota, retention, checkpoints, jobs, fsck | core, blob, meta, bundle-format (entry codec only) |
| L3 | `mer-transport-media` | `.mab` volume writer and safe reader | core, crypto, bundle-format |
| L3 | `mer-transport-diode` | UDP packet codec, paced carousel sender, receiver with block pool | core, crypto, fec, bundle-format |
| L4 | `mer-bundle` | Collector: delta computation, seal, `SealedBundle`. Vault: `Assembler` (implements `SegmentSink`), staging, import state machine, per-repo apply. Both: report build and ingest | core, crypto, blob, meta, bundle-format, storage |
| L4 | `mer-format` | `FormatPlugin`, `UpstreamSync`, `Resolve`, `Reindex`, `Serve`, shared request and response types | core, storage |
| L5 | `mer-format-{raw,apt,pypi,npm,oci}` | One plugin each; collector half behind feature `collector` | core, storage, format, fetch (optional) |
| L4 | `mer-fetch` | reqwest client with egress policy, retry, parallel fetch into ingest windows | core, storage |
| L4 | `mer-auth` | Users, tokens, RBAC evaluation, LDAP/OIDC (features), approval policy, audit checkpoints | core, crypto, meta, storage |
| L6 | `mer-http` | Listeners, TLS, middleware, `respond()`, API, dashboard, metrics | core, storage, format, auth |
| L6 | `mer-collector` | Scheduler, closure driver, solve check, wish ingest | everything below, both transports |
| L6 | `mer-vault` | Worker loop, gate, miss recorder, report export | everything below, both transports |
| L7 | `mer` | clap CLI, config loading, wiring, signals. Only crate using `anyhow`. | everything |

Layer rules, enforced by `cargo xtask check-layers` over `cargo metadata`:

- A crate depends only on strictly lower layers.
- Transports depend on `mer-bundle-format`, never on `mer-bundle`, blob or meta. They move slots and signed objects between a `SegmentSource` and a `SegmentSink`.
- Format plugins never depend on each other, on `mer-http`, or on bundle or transport crates. Plugins and bundle code reach storage only through `mer-storage`.
- `mer-testkit` is used from `tests/` directories only, never from in-crate `#[cfg(test)]` modules.
- A vault-only build (`--no-default-features --features vault,...`) contains no `reqwest`; CI asserts `cargo tree -i reqwest` is empty.

Single-owner table for concepts that the drafts defined more than once:

| Concept | The one definition |
|---|---|
| Snapshot membership | `snapshot_entry` interval table (section 4.5) |
| Snapshot identity | `SnapRowId(i64)` local; `SnapshotId([u8;32])` chained root crossing the gap; URLs use per-repo `seq` |
| Snapshot state | `SnapshotState` in `mer-core`: building, sealed, approved, rejected, retired, aborted. "Quarantined" and "pending" are views over `sealed` + `snapshot_check` + `snapshot_review`. "Tainted" is a flag. "Live" is `publish_pointer`. |
| Live pointer, history | `publish_pointer`, `publish_event` |
| Derived indices | `derived` interval overlay, outside snapshot membership |
| Records | Ordinary blobs with `role = record`; small ones inline |
| Import tracking | `bundle_import` (transport unit), `repo_import` (apply unit), `ingest_session` (lease owner) |
| Blob-omission ledger | `channel_blob` only |
| Misses | `miss` in `meta.db`, flushed in batches |
| Traffic stats | `traffic_*` in `stats.db` |
| Key-value | `meta_kv` |
| Audit | `audit_log`, `audit_head` (from migration 0001), `audit_checkpoint`, `audit_foreign_anchor` (M4) |
| DB access | rusqlite only, all writes through `Meta::write` |
| Migrations | `schema_migration` table with checksums, one numbered sequence |
| Report file | `.mar`, fixed envelope + CBOR `Report` |
| Wire format | segments + Merkle + signed `Descriptor`; big-endian; `crc32c` |
| TLS provider | `ring` by default, `aws-lc-rs` behind feature `fips` |

## 4. Storage

### 4.1 Invariants

- **I1** `blob.state = present` implies the bytes are durable (file, object or inline row) and hash to `digest`.
- **I2** Every entry of a sealed snapshot references a `present` blob. Enforced by `put_entry` and re-checked at seal.
- **I3** A store object with no blob row is garbage, deletable once older than `orphan_grace`.
- **I4** Write order: DB `pending` -> bytes durable -> DB `present`. Delete order: DB `deleting` (durable commit) -> store delete -> DB row delete.
- **I5** A blob is deletable only if, inside one write transaction, it has no `snapshot_entry`, `derived`, `blob_ref` or `blob_lease` row and `orphaned_at < now - gc_grace`.
- **I6** Sealed snapshots are immutable. The published view changes only by a pointer compare-and-swap.
- **I7** A transaction that writes `small.db` never sets a blob `present`. (WAL commits spanning attached files are atomic per file, not across files.)
- **I8** A denylisted `(path, digest)` is never returned by the core lookup function, for any snapshot, pinned or live.

### 4.2 Blob store

The trait (exact signatures in the contract, `mer-blob/src/store.rs`):

```rust
#[async_trait]
pub trait BlobStore: Send + Sync + 'static {
    fn caps(&self) -> StoreCaps;
    async fn writer(&self, opts: PutOpts) -> Result<Box<dyn BlobWriter>, StoreError>;
    async fn stat(&self, d: &Digest) -> Result<Option<BlobStat>, StoreError>;
    async fn open(&self, d: &Digest, range: Option<ByteRange>) -> Result<BlobBody, StoreError>;
    async fn delete(&self, d: &Digest) -> Result<(), StoreError>;          // idempotent
    fn list(&self, shard: u8) -> BoxStream<'_, Result<BlobStat, StoreError>>; // 256 shards by first digest byte
    async fn sync(&self, batch: SyncBatch) -> Result<(), StoreError>;      // durability barrier for Durability::Batch
    async fn sweep_tmp(&self, older_than: Duration) -> Result<u64, StoreError>;
    async fn space(&self) -> Result<Option<SpaceInfo>, StoreError>;
}
pub enum BlobBody {
    File   { file: std::fs::File, offset: u64, len: u64 },
    Bytes  (bytes::Bytes),                                   // inline blobs, mem backend
    Stream { stream: BoxStream<'static, std::io::Result<bytes::Bytes>>, len: u64 },
}
```

`adopt_file` from the storage draft is removed: once the reflink claim was dropped it has no caller, and removing it removes the hard-link aliasing bug class. If it is ever reintroduced it must take ownership (rename the source into `tmp/`, `fchmod 0444`, `fdatasync`, link, fail if `st_nlink > 1`).

**Filesystem layout**

```
<data>/store/
  FORMAT                         "mer-blobstore 1\nalgo=sha256\nfanout=2/2\n"; refuse to start on mismatch or downgrade
  blobs/sha256/ab/cd/<64hex>     mode 0444
  tmp/<instance-uuid>/<rand>.part   same filesystem as blobs/ (st_dev checked at start)
  quarantine/<64hex>.<ts>        corrupt blobs moved here by scrub
  checkpoints/<repo>/<seq>       plain file holding one hex digest (section 4.13)
<data>/meta.db  <data>/small.db  <data>/stats.db  <data>/backup/
```

Startup refuses a store root on overlayfs (writeback error propagation depends on the upper filesystem) and a `tmp/` on a different device.

**Write path** (one blocking task per writer, bounded `mpsc<Bytes>` of depth 8):

1. `open(tmp, O_CREAT|O_EXCL|O_WRONLY)`, `fallocate(size_hint)` when known.
2. Per chunk: update sha256 and any requested alt hashers (md5, sha1, sha512), write.
3. Commit: compare size and digest with `expect`. On mismatch unlink the temp and return `DigestMismatch`/`SizeMismatch`.
4. `fchmod 0444`.
5. Placement depends on `PutOpts.mode`, which the caller derives from the reserve result:
   - `CommitMode::Replace` (row was `New`, `Pending`, or a repair of `missing`/`corrupt`): `fdatasync(tmp)` (or defer to the batch), then `rename(tmp, final)`. The verified bytes atomically replace whatever unverified file was there.
   - `CommitMode::LinkIfAbsent` (another committer may legitimately have won): `link(tmp, final)`; on `EEXIST` verify the existing file's size, and re-hash it fully unless the caller states the row is `present`; only then report `existed = true`. Fallback to `rename` where hard links are unsupported.
6. Durability:
   - `Durability::PerBlob`: `fdatasync` before placement, `fsync` of the shard directory after.
   - `Durability::Batch`: the writer returns its open fd inside `Committed.sync_token`; `BlobStore::sync(batch)` runs `fdatasync` on every fd (parallel on the blocking pool) and `fsync` on each touched shard directory. `syncfs` is used as an optimisation only on Linux >= 5.8 and never as the sole barrier. Rows stay `pending` until `sync` returns.

**Read path**: `open` returns the `std::fs::File` plus range. An open fd survives unlink. No hash check on read; clients verify, scrub covers rot.

**S3 backend** (`object_store`, feature `s3`): keys `<prefix>/blobs/sha256/ab/cd/<hex>`; multipart upload gives atomic visibility; hash locally while streaming and complete only on match; single PUT with `x-amz-checksum-sha256` for objects of 8 MiB or less; `sync` is a no-op; `sweep_tmp` aborts stale multipart uploads; no decision depends on LIST except reconcile, which is age-gated.

**Small objects**: a blob with `size <= inline_max` (default 8192) is stored in `small.db.blob_inline` and has `blob.loc = 1`. This is decided by the `mer-storage` facade at ingest; backends never see inline blobs. Records (apt stanzas, npm version objects, PyPI file records, OCI tag records), PEP 658 metadata, OCI manifests and ops objects therefore cost no inode. GC, quota, scrub and bundle export treat inline blobs like any other blob.

### 4.3 Metadata layer

```rust
pub trait ReadTx:  MetaRead + ChannelRead {}
pub trait WriteTx: ReadTx + MetaWrite + ChannelWrite {}
impl Meta {
    pub async fn read<T, F>(&self, f: F) -> Result<T, MetaError>
        where F: FnOnce(&dyn ReadTx) -> Result<T, MetaError> + Send + 'static, T: Send + 'static;
    pub async fn write<T, F>(&self, class: WriteClass, f: F) -> Result<T, MetaError>
        where F: FnMut(&mut dyn WriteTx) -> Result<T, MetaError> + Send + 'static, T: Send + 'static;
}
pub enum WriteClass { Interactive, Bulk, Exclusive }
```

M4 adds `AuthRead`/`AuthWrite` as further supertraits implemented in the same backend module. No SQL leaks outside `mer-meta/src/sqlite/`. There is no sqlx anywhere.

**SQLite backend** (`rusqlite`, `bundled`):

- One OS thread owns the only write connection. Jobs arrive on a priority queue: `Interactive` (auth, publish, decisions, audit-only events) before `Bulk` (ingest windows, miss flush, compaction, mark).
- **Group commit**: up to 64 queued `Interactive`/`Bulk` jobs run in one `BEGIN IMMEDIATE` transaction, each in a `SAVEPOINT`. Every job's result is buffered and delivered only after `COMMIT` returns OK. After any job error the writer checks `sqlite3_get_autocommit()`; if SQLite rolled the whole transaction back (`SQLITE_FULL`, `IOERR`, `NOMEM`, `INTERRUPT`), or if `COMMIT` fails, every job of the group is re-run individually in its own transaction and reports its own outcome. Closures are `FnMut` and pure apart from transaction calls for this reason.
- `Exclusive` jobs (`gc_claim_batch`, `seal_group`, `publish`, the collector seal commit) are never grouped with others.
- **Post-commit hooks**: `publish` records the new pointers in the transaction context; after `COMMIT` the writer thread itself invokes the registered hook with them, in commit order, before delivering results (section 4.8).
- Readers: a pool of N `query_only` connections under `spawn_blocking` behind a semaphore. Read closures paginate so WAL checkpoints are not starved.
- Long maintenance statements (compaction, mark) are batch-limited by rows and by elapsed time (50 ms), so they never hold the write lock for long.
- Pragmas: `journal_mode=WAL`, `synchronous=FULL` (I4 breaks if a committed `deleting` flip is lost after the unlink), `foreign_keys=ON`, `busy_timeout=5000`, `temp_store=MEMORY`, `mmap_size=1GiB`, `wal_autocheckpoint=4000`. `small.db` is `ATTACH`ed as schema `small` on every connection with the same pragmas.
- `stats.db` has its own writer thread, `synchronous=NORMAL`, and is disposable. It holds traffic counters plus `token_use` and `session_seen` (high-churn last-used timestamps), so those never touch `meta.db`.
- Metrics: `mer_db_writer_queue_depth`, `mer_db_write_tx_seconds`, `mer_db_wal_bytes`.

**Postgres later**: same traits, `SERIALIZABLE` with retry on `40001` (another reason for `FnMut`). `audit_append` runs inside the transaction, so a re-run cannot double-append. Portability rules for DDL: integer ids, `BLOB(32)` maps to `BYTEA`, timestamps are unix milliseconds, JSON as `TEXT`, `path` is `COLLATE "C"` on Postgres, `STRICT`/`WITHOUT ROWID` only in SQLite files, migrations per dialect under `migrations/{sqlite,postgres}/NNNN_*.sql`.

### 4.4 DDL (SQLite dialect)

Migration `0001_storage.sql` (`meta.db`):

```sql
CREATE TABLE schema_migration (version INTEGER PRIMARY KEY, name TEXT NOT NULL,
  checksum BLOB NOT NULL, applied_at INTEGER NOT NULL) STRICT;
CREATE TABLE meta_kv (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT, WITHOUT ROWID;
-- keys: instance_id, role, publish_epoch, rbac_epoch, clock_floor, scrub_cursor, gc_lock,
--       blob_count, blob_bytes, snap_count.<state>, last_backup_at

CREATE TABLE repo (
  id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE,
  format TEXT NOT NULL,                              -- raw|apt|pypi|npm|oci
  config_json TEXT NOT NULL DEFAULT '{}',
  next_seq INTEGER NOT NULL DEFAULT 1,               -- bumped at begin_snapshot; gaps allowed
  derived_generation INTEGER NOT NULL DEFAULT 0,     -- 0 = no derived requirement (before M3, or format raw without listings)
  deny_version INTEGER NOT NULL DEFAULT 0,           -- bumped on every denylist change touching this repo
  created_at INTEGER NOT NULL, deleted_at INTEGER
) STRICT;

CREATE TABLE blob (
  id INTEGER PRIMARY KEY,
  digest BLOB NOT NULL UNIQUE CHECK (length(digest) = 32),
  size INTEGER NOT NULL CHECK (size >= 0),
  state INTEGER NOT NULL,          -- 0 pending 1 present 2 deleting 3 missing 4 corrupt
  loc INTEGER NOT NULL DEFAULT 0,  -- 0 store object 1 inline (small.blob_inline)
  created_at INTEGER NOT NULL,
  orphaned_at INTEGER,             -- GC mark; cleared whenever a reference or lease is added
  verified_at INTEGER              -- last scrub or ingest hash
) STRICT;
CREATE INDEX blob_orphaned ON blob(orphaned_at) WHERE orphaned_at IS NOT NULL;
CREATE INDEX blob_abnormal ON blob(state) WHERE state <> 1;

CREATE TABLE blob_alt_digest (     -- apt MD5Sum/SHA1, npm shasum/integrity; computed at ingest
  blob_id INTEGER NOT NULL REFERENCES blob(id) ON DELETE CASCADE,
  algo INTEGER NOT NULL,           -- 1 md5 2 sha1 3 sha512
  value BLOB NOT NULL, PRIMARY KEY (blob_id, algo)
) STRICT, WITHOUT ROWID;
CREATE INDEX alt_lookup ON blob_alt_digest(algo, value);

CREATE TABLE snapshot (
  id INTEGER PRIMARY KEY,
  repo_id INTEGER NOT NULL REFERENCES repo(id),
  seq INTEGER NOT NULL,            -- local, monotonic per repo, allocated at begin, gaps allowed
  state INTEGER NOT NULL,          -- 0 building 1 sealed 2 approved 3 rejected 4 retired 5 aborted
  origin TEXT NOT NULL CHECK (origin IN ('sync','import','manual')),
  channel_id INTEGER, upstream_epoch INTEGER, upstream_seq INTEGER, bundle_id BLOB,  -- import origin
  chain_root BLOB,                 -- SnapshotId after the last applied step (vault imports)
  content_root BLOB,               -- full root; set only by Full applies, checkpoints and fsck
  mutation_count INTEGER NOT NULL DEFAULT 0,   -- bumped by every put/remove while building
  entry_count INTEGER, logical_bytes INTEGER,  -- maintained incrementally from the previous sealed snapshot
  added_count INTEGER, removed_count INTEGER, added_bytes INTEGER,
  tainted INTEGER NOT NULL DEFAULT 0,          -- contains a denylisted entry (section 4.9)
  verified_at INTEGER,                         -- plugin verify passed (set in seal_group)
  derived_generation INTEGER NOT NULL DEFAULT 0,
  derived_state INTEGER NOT NULL DEFAULT 0,    -- 0 none 1 building 2 complete
  pinned INTEGER NOT NULL DEFAULT 0, label TEXT,
  session_id INTEGER,
  created_at INTEGER NOT NULL, sealed_at INTEGER,
  decided_at INTEGER, decided_by TEXT, retired_at INTEGER,
  UNIQUE (repo_id, seq)
) STRICT;
CREATE UNIQUE INDEX snapshot_one_building ON snapshot(repo_id) WHERE state = 0;

CREATE TABLE snapshot_entry (      -- interval-encoded membership; rowid table so indexes carry 8-byte rowids
  id INTEGER PRIMARY KEY,
  repo_id INTEGER NOT NULL REFERENCES repo(id),
  path TEXT NOT NULL,              -- format-defined key, bytewise order
  added_seq INTEGER NOT NULL,
  removed_seq INTEGER,             -- NULL = live at head; member of snapshot s iff added_seq <= s < removed_seq
  blob_id INTEGER NOT NULL REFERENCES blob(id),
  role INTEGER NOT NULL,           -- 0 artifact 1 record 2 evidence
  UNIQUE (repo_id, path, added_seq)
) STRICT;
CREATE INDEX entry_blob    ON snapshot_entry(blob_id, repo_id);
CREATE INDEX entry_removed ON snapshot_entry(repo_id, removed_seq) WHERE removed_seq IS NOT NULL;
CREATE INDEX entry_added   ON snapshot_entry(repo_id, added_seq);   -- needed by the O(delta) diff; confirm with EXPLAIN QUERY PLAN in M1

CREATE TABLE derived (             -- vault-generated indices; interval overlay per generation; never exported
  id INTEGER PRIMARY KEY,
  repo_id INTEGER NOT NULL REFERENCES repo(id),
  generation INTEGER NOT NULL,     -- (plugin schema_version, config hash, signer key id) epoch, see repo.derived_generation
  key TEXT NOT NULL,               -- served path or logical index name
  added_seq INTEGER NOT NULL, removed_seq INTEGER,
  blob_id INTEGER NOT NULL REFERENCES blob(id),
  content_type TEXT NOT NULL,
  input_hash BLOB NOT NULL,        -- sha256 over (schema_version, config hash, deny_version if masked, sorted input digests)
  needs_sig INTEGER NOT NULL DEFAULT 0,        -- 1 = a site_signature row is required before publish
  UNIQUE (repo_id, generation, key, added_seq)
) STRICT;
CREATE INDEX derived_blob    ON derived(blob_id, repo_id);
CREATE INDEX derived_removed ON derived(repo_id, generation, removed_seq) WHERE removed_seq IS NOT NULL;

CREATE TABLE site_signature (      -- produced by keyd at publish and by the re-sign timer; never before approval
  repo_id INTEGER NOT NULL REFERENCES repo(id), snapshot_seq INTEGER NOT NULL,
  key TEXT NOT NULL,               -- e.g. dists/bookworm/InRelease
  signer TEXT NOT NULL CHECK (signer IN ('site','preview')),
  blob_id INTEGER NOT NULL REFERENCES blob(id),
  key_fpr TEXT NOT NULL, signed_at INTEGER NOT NULL, valid_until INTEGER,
  PRIMARY KEY (repo_id, snapshot_seq, key, signer)
) STRICT, WITHOUT ROWID;

CREATE TABLE blob_ref (            -- generic GC roots: checkpoints, site signatures, extras, catalogs, collector ops blobs
  owner_kind TEXT NOT NULL, owner_id TEXT NOT NULL,
  blob_id INTEGER NOT NULL REFERENCES blob(id),
  PRIMARY KEY (owner_kind, owner_id, blob_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX blob_ref_blob ON blob_ref(blob_id);

CREATE TABLE publish_pointer (
  repo_id INTEGER NOT NULL REFERENCES repo(id),
  channel TEXT NOT NULL,           -- 'live' only in the MVP
  snapshot_id INTEGER REFERENCES snapshot(id),        -- NULL after unpublish
  prev_snapshot_id INTEGER REFERENCES snapshot(id),
  kind TEXT NOT NULL CHECK (kind IN ('publish','rollback','unpublish')),   -- kind of the last move
  generation INTEGER NOT NULL,
  updated_at INTEGER NOT NULL, updated_by TEXT NOT NULL,
  PRIMARY KEY (repo_id, channel)
) STRICT, WITHOUT ROWID;
CREATE TABLE publish_event (       -- append-only history
  id INTEGER PRIMARY KEY, group_id BLOB,
  repo_id INTEGER NOT NULL, channel TEXT NOT NULL,
  from_snapshot_id INTEGER, to_snapshot_id INTEGER,
  kind TEXT NOT NULL, generation INTEGER NOT NULL,
  actor TEXT NOT NULL, reason TEXT, at INTEGER NOT NULL
) STRICT;
CREATE INDEX publish_event_repo ON publish_event(repo_id, channel, id);

CREATE TABLE ingest_session (
  id INTEGER PRIMARY KEY, instance_id BLOB NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('import','sync','upload','reindex','seal')),
  bundle_ref TEXT,                 -- "<chan>/<epoch>/<seq>/<bundle_id hex>" for kind=import
  state INTEGER NOT NULL,          -- 0 active 1 suspended 2 done 3 dead
  started_at INTEGER NOT NULL, heartbeat_at INTEGER NOT NULL,
  expires_at INTEGER               -- NULL for kind=import: lifetime is bound to the bundle_import row
) STRICT;
CREATE TABLE blob_lease (          -- GC roots for not-yet-referenced blobs
  blob_id INTEGER NOT NULL REFERENCES blob(id),
  session_id INTEGER NOT NULL REFERENCES ingest_session(id) ON DELETE CASCADE,
  PRIMARY KEY (blob_id, session_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX blob_lease_session ON blob_lease(session_id);

CREATE TABLE repo_quota (
  repo_id INTEGER PRIMARY KEY REFERENCES repo(id),
  max_bytes INTEGER, max_entries INTEGER,
  soft_pct INTEGER NOT NULL DEFAULT 85,
  on_exceed TEXT NOT NULL DEFAULT 'reject' CHECK (on_exceed IN ('reject','expire_then_reject','warn'))
) STRICT;
CREATE TABLE repo_usage (          -- maintained by one helper on every entry mutation; recomputed by reconcile
  repo_id INTEGER PRIMARY KEY REFERENCES repo(id),
  logical_bytes INTEGER NOT NULL DEFAULT 0,   -- sum of sizes of distinct blobs referenced by any retained entry
  blob_count INTEGER NOT NULL DEFAULT 0,
  entry_rows INTEGER NOT NULL DEFAULT 0,
  reserved_bytes INTEGER NOT NULL DEFAULT 0,
  recomputed_at INTEGER
) STRICT;
CREATE TABLE quota_reservation (
  session_id INTEGER NOT NULL REFERENCES ingest_session(id) ON DELETE CASCADE,
  repo_id INTEGER NOT NULL, bytes INTEGER NOT NULL, PRIMARY KEY (session_id, repo_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE retention_policy (
  id INTEGER PRIMARY KEY,
  repo_id INTEGER UNIQUE REFERENCES repo(id),         -- NULL row = global default
  keep_last INTEGER NOT NULL DEFAULT 5,
  keep_within_ms INTEGER,
  rollback_depth INTEGER NOT NULL DEFAULT 2,          -- previously live snapshots always kept
  unpublished_grace_ms INTEGER NOT NULL DEFAULT 3600000,
  undecided_ttl_ms INTEGER, rejected_ttl_ms INTEGER NOT NULL DEFAULT 604800000,
  gc_grace_ms INTEGER NOT NULL DEFAULT 86400000
) STRICT;

CREATE TABLE denylist (
  id INTEGER PRIMARY KEY,
  repo_id INTEGER REFERENCES repo(id),                -- NULL = all repos (digest rules only)
  path TEXT, digest BLOB,
  reason TEXT NOT NULL,
  source TEXT NOT NULL CHECK (source IN ('reject','manual','tombstone')),
  snapshot_id INTEGER REFERENCES snapshot(id),        -- the rejected snapshot, if source=reject
  created_by TEXT NOT NULL, created_at INTEGER NOT NULL,
  removed_by TEXT, removed_at INTEGER,                -- soft delete; removal is audited
  CHECK (path IS NOT NULL OR digest IS NOT NULL)
) STRICT;
CREATE INDEX denylist_digest ON denylist(digest) WHERE removed_at IS NULL;
CREATE INDEX denylist_repo   ON denylist(repo_id, path) WHERE removed_at IS NULL;
CREATE TABLE taint_override (     -- explicit manual publish of a tainted snapshot
  snapshot_id INTEGER PRIMARY KEY REFERENCES snapshot(id),
  deny_version INTEGER NOT NULL, actor TEXT NOT NULL, reason TEXT NOT NULL, at INTEGER NOT NULL
) STRICT;

CREATE TABLE gc_run (id INTEGER PRIMARY KEY, started_at INTEGER NOT NULL, finished_at INTEGER,
  phase TEXT, retired INTEGER, compacted INTEGER, marked INTEGER, deleted INTEGER,
  bytes_freed INTEGER, error TEXT) STRICT;

CREATE TABLE job (
  id INTEGER PRIMARY KEY, kind TEXT NOT NULL,         -- import|export|seal|gc|scrub|reindex|sync|backup|fsck|root_audit
  ref TEXT, state TEXT NOT NULL CHECK (state IN ('queued','running','done','failed','interrupted','cancelled')),
  phase TEXT, done_units INTEGER NOT NULL DEFAULT 0, total_units INTEGER, unit TEXT,
  started_at INTEGER, heartbeat_at INTEGER, finished_at INTEGER, error TEXT
) STRICT;
CREATE INDEX job_state ON job(state, kind);

CREATE TABLE audit_log (
  seq INTEGER PRIMARY KEY,                 -- gapless, starts at 1
  ts INTEGER NOT NULL,                     -- advisory; seq is the order
  actor TEXT NOT NULL, action TEXT NOT NULL, target TEXT, outcome TEXT NOT NULL,
  record BLOB NOT NULL,                    -- exact UTF-8 JSON bytes that were hashed
  prev_hash BLOB NOT NULL CHECK (length(prev_hash) = 32),
  hash BLOB NOT NULL CHECK (length(hash) = 32)
) STRICT;
CREATE INDEX audit_ts ON audit_log(ts);
CREATE INDEX audit_actor ON audit_log(actor, seq);
CREATE INDEX audit_action ON audit_log(action, seq);
CREATE TABLE audit_head (id INTEGER PRIMARY KEY CHECK (id = 1), seq INTEGER NOT NULL, hash BLOB NOT NULL) STRICT;
CREATE TRIGGER audit_no_update BEFORE UPDATE ON audit_log BEGIN SELECT RAISE(ABORT, 'audit_log is append-only'); END;
CREATE TRIGGER audit_no_delete BEFORE DELETE ON audit_log BEGIN SELECT RAISE(ABORT, 'audit_log is append-only'); END;
```

`small.db` (attached as `small`):

```sql
CREATE TABLE small.blob_inline (blob_id INTEGER PRIMARY KEY, body BLOB NOT NULL) STRICT;
```

Migration `0002_channel.sql` (`meta.db`):

```sql
CREATE TABLE channel (
  id INTEGER PRIMARY KEY,
  channel_id BLOB NOT NULL UNIQUE CHECK (length(channel_id) = 8),
  name TEXT NOT NULL UNIQUE,
  side TEXT NOT NULL CHECK (side IN ('collector','vault')),
  epoch INTEGER NOT NULL DEFAULT 1,
  -- collector columns
  head_seq INTEGER NOT NULL DEFAULT 0,          -- last sealed
  blob_window INTEGER NOT NULL DEFAULT 1,       -- Wb: blobs omitted iff assumed present since head_seq+1-Wb
  step_window INTEGER NOT NULL DEFAULT 8,       -- Ws: chained steps carried per repo
  history_keep INTEGER NOT NULL DEFAULT 64,     -- sealed bundles retained for carousel and re-export
  segment_size INTEGER NOT NULL DEFAULT 4194304,
  emit_compat INTEGER NOT NULL DEFAULT 1,       -- newest format_version the vault is known to read
  session_ctr INTEGER NOT NULL DEFAULT 0,       -- diode sender session counter, +1 per sender start
  last_report_seq INTEGER NOT NULL DEFAULT 0,
  reported_seq INTEGER,                         -- imported_seq from the last ingested report
  -- vault columns
  imported_seq INTEGER NOT NULL DEFAULT 0,      -- highest imported seq in `epoch`
  trust_version INTEGER NOT NULL DEFAULT 0,
  trust_version_min INTEGER NOT NULL DEFAULT 0, -- operator-set floor
  hb_session INTEGER, hb_counter INTEGER, hb_head_seq INTEGER, hb_trust_version INTEGER,
  last_progress_at INTEGER,                     -- vault clock at last strictly newer head_seq or imported seq
  report_seq INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
) STRICT;
CREATE TABLE channel_repo (channel_id INTEGER NOT NULL REFERENCES channel(id),
  repo_id INTEGER NOT NULL REFERENCES repo(id), PRIMARY KEY (channel_id, repo_id)) STRICT, WITHOUT ROWID;
CREATE TABLE trust_doc (channel_id INTEGER NOT NULL REFERENCES channel(id), version INTEGER NOT NULL,
  envelope BLOB NOT NULL, applied_at INTEGER NOT NULL, PRIMARY KEY (channel_id, version)) STRICT, WITHOUT ROWID;

-- collector side
CREATE TABLE bundle (
  channel_id INTEGER NOT NULL REFERENCES channel(id), epoch INTEGER NOT NULL, seq INTEGER NOT NULL,
  bundle_id BLOB NOT NULL, base_kind TEXT NOT NULL CHECK (base_kind IN ('full','seq')), base_seq INTEGER NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('sealed','pruned')),
  descriptor BLOB NOT NULL,                     -- signed envelope bytes
  stream_len INTEGER NOT NULL, segment_count INTEGER NOT NULL, payload_bytes INTEGER NOT NULL,
  airtime_ms INTEGER NOT NULL,                  -- section 6.4
  route TEXT NOT NULL CHECK (route IN ('any','media_required')),
  created_at INTEGER NOT NULL,
  PRIMARY KEY (channel_id, epoch, seq)
) STRICT, WITHOUT ROWID;
CREATE TABLE bundle_step (                      -- one row per repo that changed in this bundle
  channel_id INTEGER NOT NULL, epoch INTEGER NOT NULL, seq INTEGER NOT NULL,
  repo_id INTEGER NOT NULL REFERENCES repo(id),
  kind TEXT NOT NULL CHECK (kind IN ('step','full')),
  snapshot_seq INTEGER NOT NULL,                -- collector-local head this step leads to (pinned by retention)
  prev_seq INTEGER, prev_root BLOB,             -- NULL for kind=full
  ops_blob_id INTEGER NOT NULL REFERENCES blob(id),   -- ops object (step) or entries object (full)
  chain_root BLOB NOT NULL, content_root BLOB,
  op_count INTEGER NOT NULL, added_bytes INTEGER NOT NULL,
  PRIMARY KEY (channel_id, epoch, seq, repo_id)
) STRICT, WITHOUT ROWID;
CREATE INDEX bundle_step_repo ON bundle_step(channel_id, repo_id, epoch, seq);
CREATE TABLE channel_blob (                     -- the only blob-omission ledger
  channel_id INTEGER NOT NULL, blob_id INTEGER NOT NULL REFERENCES blob(id),
  added_seq INTEGER NOT NULL, removed_seq INTEGER,
  PRIMARY KEY (channel_id, blob_id)
) STRICT, WITHOUT ROWID;
CREATE TABLE export (id INTEGER PRIMARY KEY, channel_id INTEGER NOT NULL, epoch INTEGER NOT NULL, seq INTEGER NOT NULL,
  transport TEXT NOT NULL, set_id BLOB, volumes_done INTEGER, volumes_total INTEGER, passes INTEGER,
  started_at INTEGER NOT NULL, finished_at INTEGER, detail TEXT) STRICT;
CREATE TABLE report_in (channel_id INTEGER NOT NULL, report_seq INTEGER NOT NULL, body_sha256 BLOB NOT NULL,
  ingested_at INTEGER NOT NULL, PRIMARY KEY (channel_id, report_seq)) STRICT, WITHOUT ROWID;
CREATE TABLE wish_in (
  id BLOB PRIMARY KEY CHECK (length(id) = 16), channel_id INTEGER NOT NULL, repo TEXT NOT NULL,
  root TEXT NOT NULL, env INTEGER NOT NULL, hits_class INTEGER NOT NULL, in_catalog INTEGER NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('pending','accepted','denied','fulfilled','not_found')),
  report_seq INTEGER NOT NULL, result_seq INTEGER
) STRICT, WITHOUT ROWID;

-- vault side
CREATE TABLE bundle_import (
  channel_id INTEGER NOT NULL REFERENCES channel(id), epoch INTEGER NOT NULL, seq INTEGER NOT NULL,
  bundle_id BLOB NOT NULL,
  mode TEXT NOT NULL CHECK (mode IN ('normal','blobs_only')),
  state TEXT NOT NULL CHECK (state IN ('announced','receiving','stalled','staged','verifying','imported',
                                       'applying','closed','rejected','failed','purged')),
  state_reason TEXT,
  descriptor BLOB NOT NULL,
  session_id INTEGER REFERENCES ingest_session(id),   -- lease owner; ended only on closed|rejected|purged
  staging_path TEXT, segments_total INTEGER NOT NULL, segments_have INTEGER NOT NULL DEFAULT 0,
  attempts INTEGER NOT NULL DEFAULT 0,                -- resume counter; 2 failed resumes => failed{crashed}
  sources TEXT NOT NULL DEFAULT '[]',
  first_seen_at INTEGER NOT NULL, staged_at INTEGER, imported_at INTEGER, closed_at INTEGER,
  PRIMARY KEY (channel_id, epoch, seq, bundle_id)
) STRICT, WITHOUT ROWID;
CREATE TABLE bundle_import_event (id INTEGER PRIMARY KEY, channel_id INTEGER NOT NULL, epoch INTEGER NOT NULL,
  seq INTEGER NOT NULL, bundle_id BLOB NOT NULL, from_state TEXT, to_state TEXT NOT NULL,
  actor TEXT NOT NULL, reason TEXT, at INTEGER NOT NULL) STRICT;
CREATE TABLE repo_import (                      -- apply unit: one row per (channel, repo)
  channel_id INTEGER NOT NULL REFERENCES channel(id), repo_id INTEGER NOT NULL REFERENCES repo(id),
  applied_epoch INTEGER NOT NULL DEFAULT 0, applied_seq INTEGER NOT NULL DEFAULT 0,
  chain_root BLOB,
  state TEXT NOT NULL CHECK (state IN ('ok','incomplete','waiting_base','diverged','quota_blocked','verify_failed')),
  reason TEXT, updated_at INTEGER NOT NULL,
  PRIMARY KEY (channel_id, repo_id)
) STRICT, WITHOUT ROWID;
CREATE TABLE repo_delta (                       -- per bundle, per repo apply tracking; drives lease release
  channel_id INTEGER NOT NULL, epoch INTEGER NOT NULL, seq INTEGER NOT NULL, repo_id INTEGER NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('pending','applied','superseded','skipped')),
  PRIMARY KEY (channel_id, epoch, seq, repo_id)
) STRICT, WITHOUT ROWID;
CREATE TABLE import_missing_blob (              -- only digests named by verified ops or entries objects
  channel_id INTEGER NOT NULL, repo_id INTEGER NOT NULL, digest BLOB NOT NULL,
  epoch INTEGER NOT NULL, seq INTEGER NOT NULL,
  PRIMARY KEY (channel_id, repo_id, digest)
) STRICT, WITHOUT ROWID;
CREATE TABLE report_out (channel_id INTEGER NOT NULL, report_seq INTEGER NOT NULL, body_sha256 BLOB NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('draft','approved','exported')),
  created_by TEXT NOT NULL, approved_by TEXT, second_approver TEXT,
  created_at INTEGER NOT NULL, exported_at INTEGER,
  PRIMARY KEY (channel_id, report_seq)) STRICT, WITHOUT ROWID;
CREATE TABLE miss (                             -- feeds the wishlist; flushed in batches every 10 s
  repo_id INTEGER NOT NULL REFERENCES repo(id), kind TEXT NOT NULL, subject TEXT NOT NULL,
  qualifier TEXT NOT NULL DEFAULT '',           -- version/range/tag, format-defined
  env INTEGER NOT NULL DEFAULT 0,               -- index into the repo's configured env list; 0 = unknown
  first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL,   -- local only, never exported
  hits INTEGER NOT NULL, clients INTEGER NOT NULL,
  state INTEGER NOT NULL DEFAULT 0,             -- 0 open 1 exported 2 satisfied 3 ignored
  in_catalog INTEGER NOT NULL DEFAULT 0, report_seq INTEGER,
  PRIMARY KEY (repo_id, kind, subject, qualifier, env)
) STRICT, WITHOUT ROWID;
CREATE TABLE self_update (build INTEGER PRIMARY KEY, product TEXT NOT NULL, role TEXT NOT NULL, target TEXT NOT NULL,
  version TEXT NOT NULL, blob_id INTEGER NOT NULL REFERENCES blob(id), manifest BLOB NOT NULL,
  staged_at INTEGER NOT NULL, applied_at INTEGER, applied_by TEXT) STRICT;
```

`stats.db` (M5): `traffic_client_hourly`, `traffic_client_daily`, `traffic_package_daily`, `traffic_repo_minutely` as in the access draft, plus `token_use(token_id PRIMARY KEY, last_used_at, last_used_ip)` and `session_seen(id_sha256 PRIMARY KEY, last_seen_at)`. Access tables (M4) are listed in section 10.5.

### 4.5 Snapshot model

**Entries.** A snapshot is a set of `(path, role, digest, size)` entries. Roles: `artifact` (payload served byte-exact), `record` (per-unit metadata needed to regenerate indices and re-run closure), `evidence` (upstream metadata and signatures exactly as fetched). Derived indices are not entries.

**Interval encoding.** One row per change, not one per file per snapshot (1M files x 365 snapshots would be 365M rows). History per repo is linear.

```sql
-- lookup at snapshot :s
SELECT e.role, b.id, b.digest, b.size, b.state, b.loc FROM snapshot_entry e JOIN blob b ON b.id = e.blob_id
 WHERE e.repo_id = :r AND e.path = :p AND e.added_seq <= :s AND (e.removed_seq IS NULL OR e.removed_seq > :s)
 ORDER BY e.added_seq DESC LIMIT 1;
-- diff(from, to]: candidate paths = rows with added_seq in (from, to] or removed_seq in (from, to]; O(delta)
```

Mutations happen only in the single `building` snapshot at seq N:

- `begin_snapshot` allocates N by bumping `repo.next_seq` in the same transaction. Seq numbers are monotonic, not dense; aborted rows keep their number. Compaction ignores aborted rows when it computes "kept" neighbours.
- `put_entry(path, blob)`: requires `blob.state = present`, or `pending` with a lease held by the handle's session; clears `orphaned_at`; otherwise returns `Busy`. If the live row has the same blob and role, no-op. Otherwise set `removed_seq = N` on the live row and insert `(path, added_seq = N)`. A row already at `added_seq = N` is updated in place. Every entry insertion goes through this function; there is no direct blob id reuse.
- `remove_entry(path)`: set `removed_seq = N`, or delete the row if `added_seq = N`.
- Each mutation bumps `snapshot.mutation_count` and calls the usage helper (section 4.11).
- `abort_snapshot`: `DELETE ... WHERE added_seq = N; UPDATE ... SET removed_seq = NULL WHERE removed_seq = N;` plus usage adjustment, in batches. Exact undo, repeatable.
- A new import pre-empts a suspended building snapshot of an older or superseded bundle by aborting it. A failed seal aborts in the same transaction. The single building slot can therefore never be held forever.

**Identity across the gap.** Two hashes, both defined in `mer-core`:

```
content_root = SHA256("mer/content-root/v1\0" || for each entry sorted by path bytes:
                      u8 role || u32be len(path) || path || digest[32] || u64be size)
SnapshotId for a Full  : SHA256("mer/snap-full/v1\0" || content_root)
SnapshotId for a step k: SHA256("mer/snap-step/v1\0" || SnapshotId(k-1) || ops_sha256(k))
```

`ops_sha256` is the digest of the ops object (CBOR sequence of `EntryOp`, sorted by path, one op per path). The chained `SnapshotId` is what the collector records in `bundle_step.chain_root`, what the vault records in `repo_import.chain_root` and `snapshot.chain_root`, and what a review binds to. Verification and seal are O(delta). The full `content_root` is computed only for Full applies, checkpoints and `fsck`; when a step carries `content_root` (the collector includes it every `content_root_every` steps, default 64), the vault runs a background `root_audit` job in a read transaction and marks the repo `diverged` on mismatch.

**State machine.**

```
building --seal_group--> sealed --approve--> approved --retire--> retired --compact--> (row kept as tombstone)
   |                        \--reject--> rejected --ttl--> retired
   \--abort / failed seal / pre-empted--> aborted
"live" is not a state: it is publish_pointer.snapshot_id and requires approved.
"tainted" is not a state: it is snapshot.tainted, re-evaluated at seal, at publish and on denylist change.
```

**Derived overlay.** Derived indices live in `derived`, interval-encoded per `generation`, so an unchanged packument costs zero rows per snapshot and a delta touching 50 npm packages regenerates 50 packuments.

- Reindex of snapshot s at generation g runs in seq order within g: for each output key, if the live row for (g, key) has the same `input_hash`, no-op; otherwise close the interval at s and insert. It sets `snapshot.derived_generation = g` and flips `derived_state` to `complete` in the final transaction.
- A new generation (plugin `schema_version` bump, repo config change, site key rotation) is built for every retained snapshot in seq order, then `repo.derived_generation` is flipped in one transaction. Rollback targets therefore never keep indices built with a retired key or old schema.
- A derived lookup is performed only if `snapshot.derived_state = complete` and `snapshot.derived_generation = repo.derived_generation`. `publish` refuses any other snapshot. An import snapshot can therefore never inherit stale indices.
- Serve lookups consult `site_signature`, then `derived`, then `snapshot_entry`.
- Derived rows and site signatures of retained snapshots are GC roots (`derived.blob_id`, `blob_ref`).

### 4.6 Ingest protocol

Batched so that importing 1M small blobs costs about 3 transactions per window, not one fsync per blob. Target: at least 5,000 small blobs per second on NVMe (criterion bench in M1).

Per window (1024 records or 256 MiB, whichever first; always flushed before seal):

1. **T1** `reserve_blobs(session, [(digest, size)])` in one transaction. Per item:
   - no row: insert `pending` + lease -> `New`;
   - `present`: add lease, clear `orphaned_at` -> `Exists` (the importer skips the bytes without reading them);
   - `pending`: add lease -> `Pending` (the caller writes anyway; the owner may be dead);
   - `deleting`: -> `Busy` (retry after backoff; GC finishes within milliseconds);
   - `missing`/`corrupt`: reset to `pending` + lease -> `New` (repair path).
2. Write `New`/`Pending` blobs: store objects with 4 to 8 parallel writers using `CommitMode::Replace` and `Durability::Batch`; inline blobs are hashed in memory.
3. **T2** `put_inline(session, [(digest, body)])` for the window's inline blobs (touches `small.db` only; rows stay `pending`, invariant I7).
4. `store.sync(batch)`.
5. **T3** `mark_present(session, [(digest, alt)])` plus the window's `put_entry` calls. `mark_present` verifies the affected row count equals the input and that every row is `pending` (leased by this session) or already `present`; otherwise the whole job fails.

On `DigestMismatch` the caller invokes `release_failed(session, digest)`: it deletes only the caller's lease, and deletes the blob row only if it is `pending` and has no other lease. A corrupt stream on one transport therefore cannot break a healthy concurrent ingest of the same digest.

Crash windows:

| Crash point | State on restart | Recovery |
|---|---|---|
| During temp write | temp file; row `pending` | Temp swept; pending row handled below |
| After placement, before `sync` | Final file possibly torn; row `pending` | Re-hash. Good: mark `present`. Bad or absent: unlink; keep the row for a resumable session, else drop it |
| After `sync`, before T3 | Durable file; row `pending` | Same re-hash path |
| Between T2 and T3 (inline) | Body present or absent in `small.db`; row `pending` | Re-hash the inline body; absent body means the blob is simply re-requested |
| After T3 | Consistent | None |
| Mid-snapshot | Snapshot `building`, session `suspended`, leases intact | Resume replays; `reserve` returns `Exists`, `put_entry` is idempotent. Or abort |
| GC, after `deleting` commit | Row `deleting` | Reissue idempotent `store.delete`, then finish |
| DB restored from backup | Extra store objects; rows possibly missing | Orphan sweep after grace; section 4.13 runbook |
| Blob lost or rotted | Row `present`, bytes bad | Scrub or serve error sets `missing`/`corrupt`, quarantines, alerts, adds the digest to `import_missing_blob` so the next report asks for it |

Startup order: sessions of another `instance_id` in state `active` become `suspended` (import with a `bundle_ref`) or `dead`; pending-row recovery; `deleting` replay; tmp sweep; `job.state = running` becomes `interrupted`.

Session lifetime: `sync`, `upload`, `reindex` and `seal` sessions expire after `partial_ttl` (7 days) when suspended. An `import` session never expires by time. It ends only in the transaction that moves its `bundle_import` row to `closed`, `rejected` or `purged` (section 7.3).

### 4.7 Seal

`MetaWrite::seal_group(items, commit)` is one `Exclusive` write transaction that, for every item (one building snapshot each, possibly several repos):

1. Re-validates that `snapshot.mutation_count` equals the value the caller observed when it computed its checks (root, closure) in a read transaction; mismatch fails the job.
2. Fails if any entry with `added_seq = N` references a blob that is not `present`.
3. Fills counters incrementally from the previous sealed snapshot, sets `chain_root`, optional `content_root`, `verified_at`, `tainted` (join of the step's added entries against the denylist), `state = sealed`.
4. If `commit` is given (vault import): upserts `repo_import` (applied position, `chain_root`, `state = ok`), marks `repo_delta` rows `applied`, inserts `bundle_import_event` rows.
5. Appends audit rows.

Leases are not dropped at seal; they belong to the session and end with it. A root or chain mismatch runs `abort_snapshot` inside the same transaction and records `repo_import.state = diverged`.

Per-repo apply is idempotent: if `repo_import.applied = (epoch, seq)` and `chain_root` equals the step's root, the repo is done. A failpoint test kills the worker between repos and asserts the bundle still closes.

### 4.8 Publish, rollback and the serve lookup

```sql
UPDATE publish_pointer SET prev_snapshot_id = snapshot_id, snapshot_id = :to, kind = :kind,
       generation = generation + 1, updated_at = :now, updated_by = :actor
 WHERE repo_id = :r AND channel = :c AND generation = :expect;      -- 0 rows => Conflict
INSERT INTO publish_event ...;  UPDATE meta_kv SET value = value + 1 WHERE key = 'publish_epoch';
```

`MetaWrite::publish` enforces, inside the transaction and not in the HTTP layer:

- target state is `approved` and not retired;
- `derived_state = complete` at `repo.derived_generation` (when the generation is non-zero), and a `site_signature(signer = 'site')` row exists for every derived key with `needs_sig = 1`;
- the target is not tainted, or its derived generation was built masked at the current `repo.deny_version`, or a `taint_override` row for the current `deny_version` exists;
- for `kind = publish`, target seq is greater than the current live seq; a lower seq is only reachable with `kind = rollback`;
- the audit row is appended in the same transaction.

Multi-repo publish updates several pointers in one transaction with a shared `group_id`. Rollback is the same operation with `kind = rollback`; no data moves.

**In-process published view.** `ArcSwap<HashMap<(RepoId, Channel), Arc<Published>>>` where `Published { snapshot, seq, generation, kind, prev, lease: Arc<SnapshotLease>, deny: Arc<DenySet>, by_hash: Arc<ByHashIndex> }`.

- The swap is performed by the writer thread as the post-commit hook of the publish job, in commit order. A new entry is installed only if its `generation` is greater than the installed one for that `(repo, channel)`.
- A reconciliation task polls `publish_epoch` every second on SQLite as well (the publish may have been committed by another process).
- A handler loads the map once per request and so sees exactly one snapshot.

**Snapshot leases.** `LeaseRegistry` hands out `Arc<SnapshotLease>` under one mutex. Retention calls `registry.begin_retire(snapshot)`, which marks the snapshot `retiring` (new acquisitions fail) and returns the outstanding count; `retire` proceeds only at zero. Pinned-snapshot lookups acquire through the same registry and return 404 (or 410) for `retiring`. There is no window between "load pointer" and "register guard".

**Resolving `repo_spec`.** Only core resolves `name` or `name@seq`; plugins never do.

- `name`: the live pointer.
- `name@seq` with only `repo.read`: allowed iff the snapshot is `approved`, not retired or retiring, and has at least one `publish_event` as `to_snapshot_id` (it was live at some point).
- Anything else (sealed, rejected, never published) requires `snapshot.preview`. Preview responses use the preview signing key, which production clients do not trust.

**Core lookup.** `Storage::lookup(lease, path)` is the only way a plugin obtains a blob reference:

1. Cache hit or one indexed query: `moka` keyed `(repo_id, path)` with value `Arc<[Interval]>`, the full interval history of that path joined to `blob` (digest, size, state, loc, role). Any snapshot seq resolves from the same cache entry. Negative results are cached as empty slices. A seal or publish invalidates only the paths in that step's ops.
2. Denylist check against the in-memory `DenySet` (digest set plus per-repo path set, loaded at publish and on denylist change): a hit returns `Lookup::Denied` (served as 410 or 403 and counted `reason = blocked`). This applies to every snapshot, including pinned and grace lookups, before any `FormatResponse::Blob` is honoured.
3. Miss.

**Cross-request skew.** `Storage::lookup_grace` retries a miss in the previous live snapshot only when all hold: the plugin declares the path immutable-content (pool files, by-hash, wheels, tarballs, OCI blobs by digest); the pointer's last move has `kind = publish`; the previous snapshot is `approved`; it was superseded less than `unpublished_grace` ago. After a `rollback` or `unpublish`, or when the previous snapshot is rejected, every grace fallback (previous snapshot, by-hash, artifact) is disabled. Content that exists only in a rolled-back snapshot is not served.

### 4.9 Reject, denylist and taint

- `decide(snapshot, Reject { items })` is a storage operation that writes denylist rows in the same transaction. Default `items`: every `(path, digest)` added in the snapshot relative to the last approved snapshot. The reviewer may narrow the selection but not to nothing. It bumps `repo.deny_version`, recomputes `tainted` for every non-retired snapshot of the repo that contains a matching live interval, and appends the audit row.
- Review diffs are always computed against the last approved (or live) snapshot, never against seq - 1.
- A tainted snapshot is never approved by `auto` mode. It appears in the approval queue with the offending items. It can be published only with an explicit, audited `taint_override`, or after a masked reindex in which the plugin drops the denylisted entries (the mask is part of `input_hash`).
- Rejecting never unpins and never frees blobs. GC roots are entries of retained snapshots (the head is always retained), leases and `blob_ref`. The collector's omission rule (section 5.4) is sound only under this contract.
- Tombstones from the collector (`Tombstone::Blob`, `Tombstone::Repo`) are proposals shown at the gate. A blob tombstone becomes a denylist row (`source = tombstone`) only when an approver accepts it; a repo tombstone always needs manual approval. Path removals are ordinary `Del` ops in a step and take effect at approval and publish like any other change.
- The report carries the denylist additions (`denied`), which is the only feedback about rejection.
- Denylist removal requires `repo.admin`, is audited, and may never arrive through a bundle.
- Property test (M1): for random histories of put, seal, reject, publish, rollback, a rejected digest is never reachable through `lookup` from a live pointer, a pinned snapshot or a grace fallback.

### 4.10 Garbage collection

Mark-and-sweep as indexed anti-joins. No refcounts. Batched, resumable, every step a short transaction (rows and 50 ms caps). Runs in `serve` only, one run at a time (`gc_lock` in `meta_kv` plus an in-process mutex).

1. **Retention** (section 4.12) retires snapshots.
2. **Compact.** For each retired snapshot s with kept neighbours p (largest kept seq below s) and n (smallest kept seq above s), where kept means not retired and not aborted:
   ```sql
   DELETE FROM snapshot_entry WHERE repo_id = :r AND removed_seq IS NOT NULL AND removed_seq <= :n AND added_seq > :p;
   ```
   with the usage helper applied to each deleted row. The same rule compacts `derived` per generation; obsolete generations are deleted entirely.
3. **Mark.** Cursor over `blob.id`:
   ```sql
   UPDATE blob SET orphaned_at = :now WHERE id IN (
     SELECT b.id FROM blob b WHERE b.id > :cur AND b.state = 1 AND b.orphaned_at IS NULL
       AND NOT EXISTS (SELECT 1 FROM snapshot_entry e WHERE e.blob_id = b.id)
       AND NOT EXISTS (SELECT 1 FROM derived d WHERE d.blob_id = b.id)
       AND NOT EXISTS (SELECT 1 FROM blob_ref r WHERE r.blob_id = b.id)
       AND NOT EXISTS (SELECT 1 FROM blob_lease l WHERE l.blob_id = b.id)
     ORDER BY b.id LIMIT 5000);
   ```
4. **Claim** (`Exclusive`, never grouped). For rows with `orphaned_at < now - gc_grace`, re-evaluate all four `NOT EXISTS` clauses and set `state = deleting`. The caller receives the claimed list only after `COMMIT` returned OK.
5. **Delete.** `store.delete` with 16 concurrent (inline blobs: delete the `small.blob_inline` row).
6. **Finish.** `gc_finish` re-checks `NOT EXISTS(entry)` and `NOT EXISTS(lease)` per row. If a reference appeared, the row is set to `missing` (which triggers repair) and not deleted; otherwise the row is deleted and bytes freed are recorded.
7. **Reconcile** (weekly, or `fsck`): per shard, merge-join the store listing with blob rows. Object without row and older than `orphan_grace` (24 h): delete. `present` row without object: `missing`. Size mismatch: `corrupt`. Recompute `repo_usage`.
8. **Scrub.** Rate-limited re-hash iterating by `blob.id` with a cursor persisted in `meta_kv` (`scrub_cursor`); a full pass is the unit of work. Metrics `mer_scrub_pass_progress_ratio`, `mer_scrub_last_full_pass_timestamp_seconds`.

Safety:

- **Against ingest.** `reserve_blobs` and the claim are both serialized write transactions. Either the lease lands first and the claim's `NOT EXISTS` fails, or the claim lands first and the importer sees `deleting`, waits and re-writes. `put_entry` refuses `deleting` blobs.
- **Against serves.** A serve resolves through an entry of a non-retired snapshot. A request pinned to a snapshot being retired is protected by the lease registry, by `unpublished_grace + gc_grace` (at least 25 h) and by the open fd.
- **Against clock jumps.** `now = max(now, clock_floor)` with `clock_floor` persisted in `meta_kv`; a backward step can only delay GC.

### 4.11 Quotas

- **Metric.** Per-repo `logical_bytes` is the sum of sizes of distinct blobs referenced by any retained entry of that repo. Dedup counts once inside a repo, not across repos.
- **One helper.** `usage_adjust(repo, blob, before_exists, after_exists)` is called on every `snapshot_entry` insert, delete and blob change: `put_entry` (including the in-place update, where blob A is replaced by B), `remove_entry`, `abort_snapshot` (batched) and compaction. It probes `entry_blob` for `(blob_id, repo_id)` after the mutation and adjusts `logical_bytes` and `blob_count` by the existence transition. `reserved_bytes` is decremented per blob as `put_entry` accounts it, never wholesale at seal. The proptest model checks `repo_usage` against a recomputation after every operation.
- **Global limits.** `store.max_bytes`, `store.min_free_bytes` via `space()`.

Enforcement:

1. Collector, at plan time: sizes are known from upstream indexes; projected usage over quota triggers `on_exceed`.
2. Vault, per-repo preflight: each step declares `added_bytes`. `quota_reserve(repo, session, bytes)` succeeds only if `logical_bytes + reserved_bytes + bytes <= max_bytes` and free space covers the bytes plus a reserve. Before parking a repo, reconcile recomputes `repo_usage` for it. On failure only that repo goes to `quota_blocked`; other repos in the bundle apply normally (section 7.4).
3. `NoSpace` from the store suspends the session; `fallocate` makes it fail early.

Because the vault cannot signal quota pressure, quota and retention settings can be authored on the collector and shipped as a `Policy` extra (post-MVP, section 5.1) so the collector can simulate vault usage.

Gauges: `mer_repo_referenced_bytes`, `mer_repo_quota_bytes`, `mer_blobstore_bytes`, `mer_blobstore_objects`, `mer_blobstore_free_bytes` (maintained as counters in `meta_kv` inside the transactions that change them; `/metrics` reads atomics refreshed at most every 15 s).

### 4.12 Retention

A pure function, unit-testable and usable for a dashboard dry-run:

```rust
pub fn evaluate(p: &RetentionPolicy, snaps: &[SnapshotInfo], pointers: &[Pointer], history: &[PublishEvent],
                pinned_by_steps: &BTreeSet<Seq>, now: Ts) -> Vec<(SnapRowId, Verdict)>;
```

Always kept: the head; any `building` snapshot; any snapshot a pointer targets; pinned snapshots; the last `rollback_depth` distinct snapshots that were live through a `publish` event (this set is also the by-hash window, section 9.3); anything superseded less than `unpublished_grace` ago; `sealed` snapshots younger than `undecided_ttl` (forever when unset); on the collector, every snapshot referenced by `bundle_step.snapshot_seq` of an unpruned bundle (`pinned_by_steps`). Among `approved` snapshots: the newest `keep_last` and those within `keep_within`. Everything else expires; `rejected` after `rejected_ttl`, `aborted` immediately.

Applying a verdict is `registry.begin_retire` then `retire(snap)`, which rechecks pointers in the transaction. Grandfather-father-son buckets and collector blob eviction are post-MVP.

### 4.13 Backup, checkpoints and rebuild

`meta.db` is the only place that knows which blobs form which snapshot, so it has a backup path that does not depend on re-importing bundles.

- **Backup.** `mer meta backup <dir>` uses the SQLite online backup API (`rusqlite::backup`) for `meta.db` and `small.db`. Nightly by default into `<data>/backup/`, N kept. Metric `mer_meta_last_backup_timestamp_seconds`. Copying a live WAL database by hand is documented as unsupported.
- **Checkpoints.** At every publish (and every K seals on the collector) storage writes a checkpoint blob: zstd CBOR sequence of the entries at that seq preceded by a header `{repo, format, seq, chain_root, content_root, upstream position}`. Its digest goes into `blob_ref('checkpoint', '<repo>/<seq>')` and into the plain file `<data>/store/checkpoints/<repo>/<seq>`, fsynced, outside the DB.
- **Rebuild.** `mer meta rebuild` restores repos, heads, the blob table (from a store listing plus `small.db`) and one snapshot per repo from the latest checkpoint. Derived items come from Reindex. On the collector the checkpoint plus the last sealed descriptor in `sealed/` lets it resume the same epoch and seq, so an epoch bump is needed only when both are lost.
- **Runbook.** Restore the DB backup; run `mer fsck` (orphan sweep is age-gated); re-import bundles newer than the backup if still on media or in the carousel; otherwise the next report requests per-repo Full.

### 4.14 Jobs

Every long operation (import, export, seal, GC, scrub, reindex, sync, backup, fsck, root audit) owns a `job` row and reports through `JobHandle::progress(done, total)`. On restart `running` becomes `interrupted` with a resume action. The dashboard reads job state from the table and the last 500 log lines from an in-memory ring per job. Gauge `mer_job_progress_ratio{kind}`.

## 5. Bundle model

### 5.1 Logical model

All structs live in `mer-core` (contract section 2). Encoding is CBOR via `ciborium` and serde derive: structs are maps keyed by the field names shown in the contract (the names are wire format), enums use serde's default external tagging, digests and keys are CBOR byte strings. Signatures and ids are computed over bytes as stored; nothing is re-serialised, so canonical CBOR is not required. Golden vectors pin every encoding.

```rust
pub struct Position { pub epoch: u32, pub seq: u64 }          // ordered lexicographically; used everywhere on the vault
pub struct Manifest {
    pub v: u16, pub channel: ChannelId, pub epoch: u32, pub seq: u64,
    pub base: DeltaBase,                 // describes the blob set
    pub created_at: i64,                 // informational; the vault never trusts collector clocks
    pub collector: String,
    pub repos: Vec<RepoDelta>,           // only repos with a step inside the step window
    pub tombstones: Vec<Tombstone>,      // proposals for the gate (section 4.9)
    pub extras: Vec<Extra>,
    pub wish_results: Vec<WishResult>,
    pub audit_anchor: Option<AuditAnchor>,   // collector checkpoint; None until M4, mandatory after
    pub blob_count: u64, pub payload_bytes: u64,
}
pub enum DeltaBase { Full, Seq(u64), Report { report_seq: u64 } }   // Report is reserved, never emitted in the MVP
pub struct RepoDelta { pub repo: String, pub format: String, pub change: RepoChange,
                       pub catalog: Option<Digest>, pub upstream_state: Vec<u8> }
pub enum RepoChange { Steps(Vec<Step>), Full(FullState) }
pub struct Step {                        // one per bundle seq in which this repo changed, ascending
    pub seq: u64, pub prev_seq: u64, pub prev_root: SnapshotId,
    pub ops: Digest, pub op_count: u32, pub added_bytes: u64,
    pub root: SnapshotId,                // = SnapshotId::step(prev_root, ops)
    pub content_root: Option<Digest>,
}
pub struct FullState { pub seq: u64, pub entries: Digest, pub entry_count: u64, pub total_bytes: u64,
                       pub content_root: Digest, pub root: SnapshotId }   // root = SnapshotId::full(content_root)
pub enum EntryOp { Put(EntryRec), Del { path: String } }
pub struct EntryRec { pub path: String, pub role: EntryRole, pub digest: Digest, pub size: u64 }
pub enum Tombstone { Repo { repo: String, reason: String }, Blob { digest: Digest, reason: String } }
pub struct Extra { pub kind: ExtraKind, pub name: String, pub version: String, pub blob: Digest, pub size: u64,
                   pub signed: Option<Vec<u8>> }       // signed envelope: ReleaseManifest or PolicyDoc
pub enum ExtraKind { VulnDb, SelfUpdate, Policy, Other(String) }      // there is no TrustDoc extra
pub struct WishResult { pub wish_id: WishId, pub status: WishStatus }  // Fulfilled | NotFound | Denied | Pending
```

- Ops and entries objects are ordinary blobs (class 0): a CBOR sequence of `EntryOp` or `EntryRec` sorted by path, one item per path, shipped zstd-compressed. The vault decodes them incrementally from a streaming reader; they are never materialised as one `Vec`.
- **Extras**. `SelfUpdate` carries a signed release manifest, domain `mer/release/v1\0`: `{product, role, target, version, build (monotonic u64), min_upgradable_from, artifact sha256, size}`, signed by a Release-role key. The vault stages an update only if `build` exceeds both the running and the highest ever staged build (persisted in `self_update`), product, role and target match, and the Release key is valid in the current trust version. Applying it is a manual, audited step that shows the manifest. The same manifest signs the `docker save` image tarball. `Policy` extras (post-MVP) carry a `PolicyDoc` signed by an offline Policy-role key and may only set quotas and retention; they may never touch approval mode, RBAC, trust or denylist removal. `VulnDb` versions must be monotonic.
- Extras are imported before and independently of repo deltas, so a vault can extract its own update from a bundle whose repo deltas it cannot read (`mer vault selfupdate export <dir>`).

**Blob index**: fixed 56-byte records in payload order; offsets are a running sum of `stored_len`.

```
0   32  sha256 of raw content (the CAS key)
32   8  raw_len    u64 BE
40   8  stored_len u64 BE
48   1  enc    0 raw, 1 zstd (classes 0 and 1 only; artifacts stay raw)
49   1  class  0 meta object (ops, entries, catalog)  1 record or evidence  2 artifact  3 extra
50   6  zero
```

Payload order: class 0, class 1, wish-fulfilment artifacts, remaining artifacts, extras. Early segments are the most useful ones and get higher FEC overhead on the diode.

### 5.2 Canonical stream, segments, descriptor

```
stream = manifest_z (zstd of CBOR Manifest) || blob_index (raw records) || payload (stored blobs, concatenated, no padding)
```

- The stream is virtual on the collector: a `StreamPlan` maps stream offsets to `(blob, offset)`. Sealing reads each included blob once; that single pass computes the segment leaf hashes and re-verifies the blob's sha256 (catching CAS rot before export). Only `manifest.z`, `index.bin`, `merkle.bin` and zstd'd class 0/1 objects are spooled under `sealed/<chan>/<epoch>/<seq>/`.
- Segment size `S` is fixed per bundle (default 4 MiB, power of two). Segment `i` is `stream[i*S .. min((i+1)*S, len)]`.
- Merkle tree: SHA-256 with RFC 6962 style domain separation, padded to a power of two so every proof has depth `d = max(1, ceil(log2 n))`. `leaf_i = SHA256(0x00 || u32be(i) || segment_i)`, `leaf_i = 0^32` for `i >= n`, `node = SHA256(0x01 || L || R)`.
- `BundleId = SHA256("mer/bundle-id/v1\0" || manifest_sha256 || index_sha256)`.

```rust
pub struct Descriptor {
    pub format_version: u16,             // 1
    pub min_reader_version: u16,         // a reader with a lower max version refuses before staging, with an exact message
    pub features: u64,                   // bitset; unknown bits above the reader's mask are refused
    pub channel: ChannelId, pub epoch: u32, pub seq: u64,
    pub base: DeltaBase, pub bundle_id: BundleId, pub created_at: i64,
    pub stream_len: u64, pub segment_size: u32, pub segment_count: u32,
    pub hash_alg: u8,                    // 1 = sha256
    pub merkle_root: Digest,
    pub manifest_len: u64, pub manifest_raw_len: u64, pub manifest_sha256: Digest,   // sha256 of the stored (compressed) bytes
    pub index_len: u64, pub index_sha256: Digest,
    pub enc: Option<EncInfo>,            // reserved; always None in the MVP
    pub trust_version: u32,              // version the signer used; a hint only
}
```

The descriptor is under 1 KiB and is the only thing signed per bundle; everything else chains to it by hash.

**Slot** (the unit in volume files, in vault staging and as one RaptorQ block). Big-endian.

```
0     4    magic "MSG1"
4     1    kind (0 data; 1 parity, reserved)
5     1    depth d
6     2    zero
8     4    index
12    4    data_len (== S except the last segment)
16    16   bundle_id[0..16]
32    32d  audit path, leaf sibling first
H     data_len bytes                          (H = 32 + 32d)
...   zero pad
slot_size-4  4  crc32c of [0, slot_size-4)
slot_size = round_up(H + S + 4, 4096)
```

Any single slot is verifiable against the signed root regardless of transport or order. The crc32c is a cheap pre-filter; the Merkle proof is the authority. The "block" used by FEC and by `SegmentSink::offer_slot` is the meaningful prefix `[0, H + data_len)`.

**Limits on untrusted input** (constants in `mer_core::limits`):

- All signed objects use one fixed binary envelope, and signatures are verified over `body` before any CBOR decoding:
  ```
  0  4  magic: "MSD1" descriptor, "MST1" trust doc, "MSH1" heartbeat, "MSR1" report, "MSL1" release manifest, "MSP1" policy
  4  4  body_len u32 BE (cap per kind: 16 KiB descriptor, 64 KiB trust doc, 4 KiB heartbeat, 256 KiB report, 4 KiB release, 64 KiB policy)
  8  body
  .. 1  sig_count (1..=8)
  .. sig_count x { key_hash[32], sig[64] }      sig = Ed25519(domain || body), verify_strict
  ```
- zstd is decoded through a streaming reader limited to the signed `raw_len` (or `manifest_raw_len`, capped at 16 MiB), `window_log_max <= 27`; overrun is `Integrity`.
- CBOR decoding uses a recursion limit of 16 and per-field maxima (repos 4096, steps per repo 64, tombstones 4096, extras 64, wishes 4096, strings 1024 bytes).
- Fuzz targets cover every one of these decoders (section 16).

### 5.3 Signing and trust anchors

```rust
pub enum Role { Root, Bundle, Release, Report, Policy }
pub struct TrustDoc {
    pub version: u32,                    // +1 per change, strictly sequential
    pub channel: ChannelId,
    pub epoch: u32,                      // the only epoch in which descriptors are accepted
    pub roots: Vec<PubKey>, pub root_threshold: u8,
    pub bundle_threshold: u8,            // k valid Bundle-role signatures required on a descriptor (default 1)
    pub keys: Vec<TrustedKey>,
    pub created_at: i64,
}
pub struct TrustedKey { pub pubkey: PubKey, pub role: Role,
                        pub valid_from: Position, pub valid_until: Option<Position>, pub note: String }
pub struct PubKey(pub [u8; 32]);         // Ed25519 public key; CBOR byte string
pub struct KeyHash(pub [u8; 32]);        // sha256(pubkey); the full hash selects keys, never a truncation
```

Signature domains (each signed object type has its own): `mer/trust/v1\0`, `mer/descriptor/v1\0`, `mer/heartbeat/v1\0`, `mer/report/v1\0`, `mer/release/v1\0`, `mer/policy/v1\0`, `mer/audit-ckpt/v1\0`.

Rules, all evaluated against the vault's persisted state:

1. **Anchor.** TrustDoc v1 is installed at pairing from a `channel.pair` file (channel id plus the v1 envelope). The pairing command displays the channel id and the root key fingerprints for two-person confirmation and audits it. The diode link key travels in a separate secret file. The vault's Report public key goes back to the collector in the same ceremony.
2. **Rotation.** Version v+1 is accepted iff `version == current + 1`, the channel matches, `epoch >= current.epoch`, and it is signed by at least `root_threshold` of roots(v) and of roots(v+1). Never by the outgoing Bundle key. The vault persists the highest applied version (`channel.trust_version`, `trust_doc` rows) and never goes back. The chain is embedded in every volume header and repeated in diode ANNOUNCE packets, and is processed before descriptor verification.
3. **Key validity.** A signature counts only if the key is present with the right role in the vault's current trust version, `valid_from <= P`, and `valid_until` is absent or `P <= valid_until`, where P is the descriptor position. A `valid_until` that is greater than the vault's imported position at the moment that trust version is applied marks the key as removed. A stolen key revoked "at seq 100" therefore cannot sign anything a lagging vault has not yet imported. Because positions are `(epoch, seq)` tuples and the epoch is fixed by the trust doc, a removed key stays removed across epochs.
4. **Signer set.** A descriptor is rejected unless at least `bundle_threshold` Bundle-role keys valid under the current trust version signed it, regardless of `Descriptor.trust_version`. If `Descriptor.trust_version` is higher than the vault's, the bundle waits (`announced`, reason `waiting_trust`) for the chain. During key overlap the collector dual-signs; retained bundles are re-signed when a key is removed (the envelope's signature list changes, the body does not).
5. **Epoch.** `descriptor.epoch` must equal `trust.epoch`. An epoch bump therefore requires a root-signed trust version. Applying a trust doc that raises the epoch is staged and needs explicit operator confirmation; it resets `imported_seq` to 0 and puts every repo in `waiting_base` (the first bundle of the new epoch is Full).
6. **Withheld trust.** `mer vault trust set-min <version>` records a floor from a root-signed, hand-carried notice; descriptors signed under a lower version are refused. The dashboard alerts when a heartbeat advertises a trust version above the vault's.
7. **Replay and equivocation.** A descriptor is accepted iff `epoch == trust.epoch` and either `imported_seq < seq <= imported_seq + accept_window` (default 64) or `seq` is a recorded gap (no terminal `bundle_import` row, `seq > imported_seq - gap_keep`). A known `bundle_id` at an imported position is an idempotent no-op. The same `(epoch, seq)` with a different `bundle_id` is rejected with a critical audit event.
8. **Key custody.** Root, Release and Policy keys stay offline. The Bundle key is held by `mer collector export` behind the `Signer` trait (a key file owned by a separate uid in a container with no upstream network in the MVP; PKCS#11 later). Vault private keys are held by `mer keyd`.
9. **Defence in depth.** The vault re-verifies upstream signatures in the plugin `verify` step before any snapshot is sealed, and the quarantine gate still applies.

Per-bundle AEAD encryption (X25519 recipients, ChaCha20-Poly1305 per segment, Merkle over ciphertext) is post-MVP; `Descriptor.enc` reserves the field.

### 5.4 Delta computation without feedback (collector)

Sealing never waits for anything. State is `channel`, `bundle`, `bundle_step`, `channel_blob` (section 4.4).

Sealing bundle N on channel c:

1. **Steps.** For each repo of the channel, compare the latest sealed local snapshot with `last_step(c, repo).snapshot_seq`. If it changed, `ops = MetaRead::diff(repo, last.snapshot_seq, head.seq)` (index range scans, O(delta)); the ops object is written to the CAS; `root = SnapshotId::step(last.chain_root, ops_digest)`. A repo flagged for Full (first bundle, new epoch, or `full_request` from a report) gets a `FullState` built by a full walk.
2. **Step window.** For each repo, the manifest carries every step with `seq > N - step_window` (default 8), newest last. Their ops objects are always included (they are small), so a vault that missed bundles can still catch up on metadata.
3. **Blob set.** For every `Put` in step N (and for Full repos when `--with-blobs` or `DeltaBase::Full`): include the blob unless `channel_blob` has it with `added_seq <= b AND removed_seq IS NULL`, where `b = N - blob_window` (default window 1, so a bundle ships only what is new in it). Plus ops and entries objects, catalogs, extras, and any `missing_blobs` from the last report.
4. **Ledger updates** (computed now, committed in step 7): new blobs get `added_seq = N`; a blob with no remaining live entry in any repo of the channel (probe `entry_blob` with `removed_seq IS NULL`) gets `removed_seq = N`; a re-added blob gets `added_seq = N, removed_seq = NULL`, so it ships again. The rule errs towards shipping.
5. **Spool.** Write `manifest.z`, `index.bin`, `merkle.bin` in a temp directory, fsync, rename into `sealed/<chan>/<epoch>/<seq>/`.
6. **Sign** the descriptor through the `Signer`.
7. **Commit**, one `Exclusive` transaction: `INSERT bundle`, `INSERT bundle_step` rows (plus `blob_ref` for ops objects), apply all `channel_blob` changes, `UPDATE channel.head_seq`, audit row. On startup, spool directories with no `bundle` row are deleted. `fsck` recomputes `channel_blob` from `bundle_step` and reachability for the retained history and compares.

**Vault-side contract** that makes the omission rule sound: GC never deletes blobs referenced by the head snapshot of any repo (the head is never retired) or by a lease, independent of approval state (section 4.9).

**When the vault misses bundle N**

- Bundle N+1 still imports (it is inside `accept_window`). Its step window contains step N, so each repo applies steps N and N+1 from it. Blobs first shipped in N are absent, so affected repos become `incomplete` with exact digests in `import_missing_blob`.
- Resolution, in order: the carousel re-delivers N; N is re-exported to media (`history_keep` sealed bundles are retained); a report lists `missing_blobs`, which the next bundle force-includes; a report with `full_request` for a repo yields `RepoChange::Full`.
- A vault further behind than `step_window` puts the repo in `waiting_base`; `collector export --catch-up <seq>` builds one bundle whose windows reach back to the reported position if history is retained, otherwise Full for that repo.
- `blob_window > 1` re-includes recent blobs for sites that prefer bandwidth over round trips; the diode default stays 1 because the carousel already repeats.

**Report ingest** sets `reported_seq`, updates `emit_compat` from the reported software version, force-includes `missing_blobs`, marks `full_request` repos, records wishes (section 8) and adds `denied` items to the collector exclusion list. A report is always optional.

**Collector DB lost**: restore from backup or checkpoint (section 4.13) and continue the same epoch. Only if that is impossible: a root-signed trust doc bumps the epoch, seq restarts at 1, the first bundle is Full.

### 5.5 Version negotiation

- The collector upgrades first. `channel.emit_compat` is set at pairing and raised only by a state report (`Report.software`, `Report.format_max`) or by `mer collector channel set-vault-version`. The collector writes manifest, record and slot formats no newer than that (writer compatibility N-1).
- Any bundle containing a `SelfUpdate` is sealed at the oldest supported format.
- `staging/*/state.json` and the store `FORMAT` file carry a version, with explicit migrate-on-start steps and a refuse-to-downgrade check.
- Plugin records carry a leading version; the vault refuses a repo delta whose record version it does not understand with repo state `verify_failed` and reason `upgrade the vault`, without affecting other repos or extras.
- Golden-file tests read version N-1 bundles.

## 6. Transports

### 6.1 Shared interfaces

Defined in `mer-bundle-format` so transports never depend on `mer-bundle`:

```rust
#[async_trait]
pub trait SegmentSource: Send + Sync {             // a sealed bundle; random access to blocks
    fn descriptor_envelope(&self) -> &[u8];
    fn descriptor(&self) -> &Descriptor;
    fn trust_chain(&self) -> &[Vec<u8>];           // signed envelopes, oldest first
    fn params(&self) -> BundleParams;
    async fn read_block(&self, index: u32, buf: &mut Vec<u8>) -> Result<(), FormatError>;   // header + proof + data
}
pub trait SegmentSink: Send + Sync {               // the vault assembler; all input is untrusted
    fn offer_trust(&self, envelope: &[u8], via: Source) -> TrustOutcome;
    fn offer_descriptor(&self, envelope: &[u8], via: Source) -> Result<BundleKey, Reject>;
    fn params(&self, key: BundleKey) -> Option<BundleParams>;
    fn slot_state(&self, key: BundleKey, index: u32) -> SlotState;      // Empty | Collecting | Verified
    fn offer_block(&self, key: BundleKey, index: u32, block: &[u8], via: Source) -> SlotOutcome;
    fn missing(&self, key: BundleKey) -> Vec<std::ops::Range<u32>>;
    fn offer_heartbeat(&self, envelope: &[u8]) -> HeartbeatOutcome;
    fn note_link(&self, ev: LinkEvent);
}
```

No transport signature contains a return channel. `offer_block` verifies header fields against the descriptor, the Merkle proof against the signed root, then writes the padded slot with one aligned `pwrite`. Each slot has a state (`Empty`, `Collecting`, `Verified`) under a striped mutex; a `Verified` slot is never written again and `offer_block` returns `Duplicate` before any I/O.

MVP transports: removable media and UDP diode. A file-drop diode transport (small `.mab` volumes dropped into a one-way file proxy) is post-MVP; when added, its receiver must open candidates with `openat` + `O_NOFOLLOW`, require regular files, and delete a dropped file with `unlinkat` on the verified directory fd only after its slots are fdatasynced and the bitmap update is durable.

### 6.2 Removable media container (`.mab`)

One export is a set of data volumes sized to the medium. `volume_size` presets: `fat32` (4 GiB - 4 KiB), `dvd` (4.37 GiB), `bd25`, or custom. Parity volumes (Reed-Solomon across volumes) are post-MVP; the operator re-copies a bad volume or lets the diode fill the gaps. Fields for parity are reserved.

File names are derived from indices only: `<chan16hex>-e<epoch>-s<seq:010>-d<idx:04>.mab`. The importer ignores names entirely; no name or path from media metadata is ever used. A `CONTENTS.txt` is written for operators and scanning stations; its first line is `UNAUTHENTICATED, INFORMATIONAL ONLY`.

```
VolumeHeader, 4096 bytes, big-endian:
0     8   magic "MERVOL01"
8     2   format version (1)
10    1   vol_kind (0 data; 1 parity, reserved)
11    1   zero
12    16  set_id (random per export)
28    32  bundle_id
60    4   vol_index
64    4   vol_count
68    4   first_segment
72    4   slots_in_volume
76    4   slot_size          (must be in [4 KiB, 64 MiB + 4 KiB] and equal the value derived from the verified descriptor)
80    4   segment_size
84    4   descriptor_len     (<= 16 KiB + envelope overhead)
88    4   trust_chain_len    (<= 256 KiB)
92    20  reserved for parity groups (zero)
112   ..  zero
4092  4   crc32c of [0, 4092)
then: signed descriptor envelope || trust chain (u16 count, then per item u32 len + envelope), zero padded to a 4096 multiple
then: slots_in_volume slots of slot_size bytes each
VolumeTrailer, 4096 bytes: magic "MERVEND1", slots_written u32, sha256 of all preceding bytes [32], zero, crc32c in the last 4 bytes
```

Reader rules (the header is attacker-controlled until the descriptor verifies):

- Every length is hard-capped before any allocation. The descriptor signature is verified before any header-derived value is used for offsets; `slot_size`, `segment_size` and `bundle_id` must then equal the values derived from the descriptor.
- Candidates are opened with `openat` + `O_NOFOLLOW | O_NONBLOCK` relative to the scanned directory fd; `S_ISREG` is required; the scan never follows symlinks and never leaves the given roots.
- Fixed slots: slot j is at a computable offset, so a bad sector damages exactly the slots it touches. A missing or bad trailer means the volume is partial; valid slots are still used.
- Media is never modified or deleted. Recommended mount options: `ro,nodev,nosuid,noexec`. Import always copies; `--in-place` is post-MVP and, if added, must re-verify the Merkle proof of each slot on every read.
- After each medium the importer prints the missing segment ranges and which volume indexes would supply them.

Export: each volume is written as `*.part`, fsynced, renamed, then the file and its directory are fsynced (`syncfs` on the mount where available), the trailer hash is re-read from the medium, and only then is the volume recorded as finished in `export`. Every volume carries the descriptor and trust chain, so import can start from any volume.

### 6.3 UDP diode

One RaptorQ source block is the block of one slot, padded with zeros to `K * T`.

- Crate `raptorq` (RFC 6330), per-block encoder and decoder behind `mer_fec::BlockCodec`. The segment index travels in our header (the RFC's SBN is 8 bits).
- Symbol size `T = 1368` (a multiple of 8): `40 (IPv6) + 8 (UDP) + 68 (header) + 1368 + 16 (MAC) = 1500`. `T = 8192` for jumbo frames. K is about 3067 for a 4 MiB segment.
- RaptorQ is rateless: every repair pass emits fresh symbols and any K + 2 or so symbols from any mix of passes decode the block.

**Packet layout** (big-endian). Every packet ends with a 16-byte MAC: the first 16 bytes of `HMAC-SHA256(link_key, packet[0 .. len-16])`. The MAC is mandatory; sender and receiver refuse to start without `link_key`.

```
Common header, 32 bytes
0   4  magic "MDD1"
4   1  version (1)
5   1  type: 1 ANNOUNCE, 2 DATA, 3 HEARTBEAT, 4 END_OF_PASS
6   1  flags: bit0 = 1 (MAC tail present; a packet with bit0 = 0 is dropped)
7   1  header_len
8   8  channel_id
16  4  session_id  (channel.session_ctr: +1 at every sender start, persisted)
20  8  link_seq    (+1 for every packet of the session, all types)
28  2  payload_len
30  2  zero

DATA, header_len 68
32  4  epoch
36  8  bundle_seq
44  4  sbn  (segment index)
48  4  esi  (< K source symbol, >= K repair)
52  4  block_len (bytes before padding)
56  2  symbol_size T
58  2  pass
60  8  bundle_id[0..8]
68  T  symbol

ANNOUNCE, header_len 40
32  1  obj_kind (1 signed descriptor, 2 signed trust doc)
33  1  frag_idx
34  1  frag_cnt (<= 48)
35  1  zero
36  4  obj_tag (first 4 bytes of sha256(object))
40  .. fragment (<= T bytes)

HEARTBEAT, header_len 32: payload = signed envelope "MSH1" over CBOR Heartbeat
END_OF_PASS, header_len 48:  32 4 epoch | 36 8 bundle_seq | 44 2 pass | 46 2 zero ; no payload
```

```rust
pub struct Heartbeat { pub channel: ChannelId, pub epoch: u32, pub session_id: u32, pub hb_counter: u64,
                       pub head_seq: u64, pub oldest_retained_seq: u64, pub trust_version: u32,
                       pub rate_bps: u64, pub interleave: u16, pub carousel: Vec<CarouselItem> }
pub struct CarouselItem { pub seq: u64, pub bundle_id: BundleId, pub segment_count: u32, pub passes_done: u16 }
```

**Receiver admission pipeline** (nothing is allocated or written from unauthenticated input):

1. Length, magic, version, known channel.
2. MAC, constant-time compare.
3. Session: `session_id` lower than the highest authenticated one (persisted) is dropped; a higher one is adopted and resets the replay window.
4. Replay window on `link_seq` (sliding bitmap of 4096); duplicates and stale packets are dropped.
5. ANNOUNCE reassembly is bounded to 4 objects per channel, each at most 64 KiB; a completed object is checked against `obj_tag`, then handed to `offer_trust` or `offer_descriptor`, which verify signatures before decoding. Descriptors outside the acceptance rule of section 5.3 item 7 are dropped, so replayed old descriptors create no staging directories.
6. DATA for a bundle with no verified descriptor is dropped and counted (`unknown_bundle`). There is no orphan buffer; the descriptor is one packet repeated at least every second, at every pass start and every 2000 DATA packets, so at most about a second of data is lost and the next pass refills it.
7. For a known bundle: `sbn < segment_count`, `block_len` equals the value derived from the descriptor for that segment, `T` equals the session's symbol size, `bundle_id[0..8]` matches; otherwise drop. A slot in state `Verified` is dropped before any work.

**Receiver data path** (designed for 1 Gbit/s):

- A dedicated thread runs the receive loop with `SO_RCVBUF` of at least 64 MiB and never blocks on disk; overflow of the bounded hand-off queues is counted as `rx_backpressure_drops`.
- Each block being collected owns a 4 MiB buffer from a pool of `D + spare` buffers. Source symbols are copied to `esi * T`; repair symbols are kept in the block's repair list. No symbol is ever written to `slots.part`.
- A block with all K source symbols is verified in memory (slot header, Merkle proof) and handed to `offer_block`: one aligned `pwrite` per completed slot. No decoding on lossless reception.
- A block with at least K + 2 symbols but missing source symbols goes to a decode pool (N cores, bounded queue; metrics `mer_diode_decode_queue_depth`, `mer_diode_block_decode_seconds`).
- The assembler batches durability: `fdatasync(slots.part)` and an atomic bitmap flush every 64 slots or 1 s. A crash loses only data the carousel will resend.
- Verify failure after decode (possible only with a sender bug or a leaked link key): drop the repair symbols, keep the source symbols, count `poisoned`, wait for the next pass.
- When the pool is exhausted the least recently touched incomplete block is spilled to `partial/<index>.blk` within a byte budget (symbol bitmap and symbols in one file, written to a temp name and renamed, crc32c trailer) or dropped. On restart, spill files with a bad crc are discarded.

Per-block state: `Empty -> Collecting -> (all source | decode ok) -> Verified`. Per-bundle receive state is part of `bundle_import.state` (section 7.2).

**Sender**

1. **Passes.** Pass 0 and every even pass: the K source symbols plus `ceil(K * ovh)` fresh repair symbols. Odd passes: `ceil(K * (1 + ovh))` fresh repair symbols, ESI continuing. A receiver that missed whole passes fills systematically on the next even pass without decoding. Default `ovh = 0.10`; segments holding the manifest and blob index get `ovh = 1.0`.
2. **Interleaving.** Blocks are processed in windows of D and emitted round-robin (`for symbol round: for block in window: send`). A burst of duration t costs each block about `t * R / (D * S)` of its symbols. D is derived: `D = clamp(ceil(burst_target * R / (ovh * S)), 8, interleave_max)` with `R` in bytes per second, `burst_target` 1 s, `interleave_max` 128 (512 MiB of receiver buffers). The heartbeat advertises D so the receiver can alert if its pool is smaller than `D + 8`.
3. **Repair cache.** Repair symbols are encoded once per (segment, pass) and kept in a bounded LRU, so repeat passes do not re-read and re-encode from the CAS.
4. **Pacing.** Byte token bucket ticking at 1 ms with a hard cap at `rate_bps` (anything above the diode's capacity is silent loss), large `SO_SNDBUF`.
5. ANNOUNCE as in item 6 above; HEARTBEAT every 5 s, even when idle; END_OF_PASS after each pass.
6. A true diode answers no ARP or ND: document a static neighbour entry or a multicast or broadcast destination, and bind to a dedicated interface.

**Scheduler** (strict priority): (1) pass 0 of the newest bundle's class 0 and wish segments; (2) pass 0 of remaining segments, oldest bundle first; (3) the second initial pass; (4) repair passes for bundles younger than `carousel_ttl` (72 h) or below `min_passes` (4), oldest first, only with idle capacity; (5) optional `idle_fill`.

**Gap and freshness detection with no ack**

- Packet level: gaps in `link_seq` give loss-rate and burst-length histograms.
- Block level: `segment_count` against the bitmap; missing ranges logged at each END_OF_PASS.
- Bundle level: heartbeats are signed (Bundle role, domain `mer/heartbeat/v1\0`) over `{channel, epoch, session_id, hb_counter, head_seq, trust_version, ...}`. The vault persists the highest `head_seq` and the per-session `hb_counter`, rejects non-increasing counters and any `head_seq` regression (metric `mer_diode_heartbeat_replays_total`, audited). "Sender is at 44, I imported 41, 42 incomplete, 43 waiting" raises `bundle_gap`.
- Staleness: `channel.last_progress_at` is the vault's monotonic-clamped clock at the last strictly newer `head_seq` or imported seq. A per-channel `max_staleness` alert (`mer_channel_staleness_seconds`) applies to diode and media channels alike, so a withheld bundle or a vault that stopped receiving media is visible.
- No valid heartbeat for 3 intervals raises `diode_link_silent`.

**Acceptance bench (M2)**: at least 800 Mbit/s lossless and at least 300 Mbit/s at 3 percent loss on 4 cores, loopback, T = 1368.

### 6.4 Diode capacity planning and admission

- At seal: `airtime = stream_len * 8 * (1 + ovh) * initial_passes / rate_bps`, stored in `bundle.airtime_ms`. Effective goodput on first delivery is about 45 percent of link rate with two initial passes.
- `mer_diode_queue_airtime_seconds` and an ETA per bundle in `/ui/bundles`.
- Per-channel policy `diode.max_bundle_airtime` (default 12 h): larger bundles are sealed with `route = media_required`, are not queued on the carousel, and raise a dashboard alert. Initial Full and catch-up bundles are always `media_required`.
- The carousel never re-sends more than the bundle itself: `blob_window = 1` on diode channels; catch-up comes from the step window (metadata) plus repeat passes.

## 7. Vault import

### 7.1 Staging

```
<data>/staging/<chan16hex>/e<epoch>-s<seq>-<bundle_id16hex>/
  descriptor.bin      signed envelope
  slots.part          sparse; slot i at i * slot_size (the same slot format as media)
  have.bitmap         roaring bitmap + crc32c trailer; written as have.bitmap.tmp, fsync, rename, fsync dir
  punched.bitmap      same format; slots whose bytes were hole-punched after CAS ingest
  partial/<index>.blk diode spill files
  state.json          {version, sources, attempts}
```

- **Budget.** `staging.max_bytes` and a `statvfs` preflight from `stream_len` before a staging directory is created; at most `staging.max_bundles` concurrent bundles (default 8), evicting the oldest `stalled` one. Gauge `mer_staging_bytes{state}`. `mer vault staging ls|purge <seq>`. Staging of superseded and closed bundles is purged immediately.
- **Bitmap recovery.** A missing or invalid bitmap is rebuilt by scanning `slots.part` and verifying each slot's crc and Merkle proof.
- **Streaming ingest** starts only after the descriptor signature has verified and the manifest and blob index segments are complete and match `manifest_sha256` and `index_sha256`. From then on, as soon as every segment covering a blob is verified, the blob is hashed and ingested into the CAS in windows (section 4.6) under the bundle's session lease.
- **Hole punching.** A slot is punched (`fallocate(PUNCH_HOLE)`) only when every blob overlapping it is `present` in the CAS and leased by this bundle; segment boundaries do not align with blob boundaries, so a slot shared with a not yet ingested neighbour is kept. `punched.bitmap` is separate from `have.bitmap`.
- **Lease lifetime.** The lease owner is the `bundle_import` row through its `ingest_session`, not a timer. Leases end only in the transaction that moves the bundle to `closed`, `rejected` or `purged`. That transaction, and `fsck` when it finds a leased blob missing, first clears the `have` bits of punched slots whose blobs are gone, so `missing()` reports exactly what must be re-carried. Before the import transaction the worker re-checks that every index blob is `present` or still readable from an unpunched slot; otherwise it clears the bits and returns to `receiving`.
- **Read errors.** If the staged reader gets a digest mismatch, the Merkle proofs of the covering slots are re-verified, bad bits are cleared and the bundle returns to `receiving`.
- Cost statement: import is one sequential read of staging plus one sequential write into the CAS; there is no reflink path because blob bytes in slots are interleaved with headers and proofs.

### 7.2 Bundle state machine (transport unit)

```
announced --first segment--> receiving <--> stalled (END_OF_PASS or idle with missing > 0; next pass or media resumes)
receiving --bitmap full or every index blob present--> staged --> verifying --> imported --> applying --> closed
announced|receiving|staged|verifying --> rejected(reason)   bad signature, bad hash, manifest/descriptor mismatch, fork
any non-terminal --> failed{crashed}   after 2 failed resumes (attempts); never auto-resumed; operator retry or purge
any non-terminal --> purged            operator; leases released, have bits of punched slots cleared
```

`verifying` (idempotent, restartable):

1. Trust chain, descriptor signature and position rules (already done at `announced`; repeated).
2. `manifest_sha256`, `index_sha256`; manifest fields equal descriptor fields; limits.
3. Every index blob is in the CAS with a verified digest (or was already `present`).
4. Extras: recorded and staged independently of repos (SelfUpdate rules of section 5.1).
5. One transaction (`import_commit`): `channel.imported_seq = max(imported_seq, seq)`, `last_progress_at`, `repo_delta` rows `pending` for every repo in the manifest, `bundle_import_event`, audit. State `imported`.

A bundle that fills a recorded gap (`seq < imported_seq`) runs in `mode = blobs_only`: steps 1 to 4, then its blobs satisfy `incomplete` repos, then `closed`. It never applies steps, because every repo is already beyond it.

`applying`: each `RepoDelta` is applied independently (section 7.3). The bundle is `closed` when every `repo_delta` row is `applied`, `superseded` (a later Full covered it) or `skipped` (the repo was already past it); the closing transaction ends the session and releases the leases. A frozen repo therefore keeps the blobs it will need pinned.

### 7.3 Per-repo apply (apply unit)

For `RepoChange::Steps(steps)` and `r = repo_import(channel, repo)`:

1. Select steps with `seq > r.applied_seq`. None: mark `skipped`.
2. Base check: the first selected step must satisfy `prev_root == r.chain_root`. If `r.applied_seq < prev_seq` the window does not reach the vault's head: state `waiting_base`. Any other mismatch: `diverged`. Each step's `root` must equal `SnapshotId::step(prev_root, ops)`.
3. Quota preflight on the sum of `added_bytes` (section 4.11): failure gives `quota_blocked`.
4. Closure: every `Put` digest of the selected steps is `present`. Otherwise `incomplete`, with the digests in `import_missing_blob`.
5. `begin_snapshot(repo, Import)`, apply the ops of all selected steps in order through `put_entry`/`remove_entry`, in windows. A `Del` of an absent path or a `Put` identical to the live entry is a divergence: abort, `diverged`.
6. Plugin `verify` runs on the building snapshot (upstream trust chain from evidence and records, offline). Failure: abort, `verify_failed`. Unverified content never becomes a sealed snapshot.
7. `seal_group` (section 4.7): `sealed`, `repo_import` advanced, `repo_delta` `applied`.
8. Queue reindex (derived overlay), then checks and the approval gate (section 10.4).

`RepoChange::Full`: begin a snapshot, compute the diff between the entries object and the current head in a streaming merge, apply it, verify `content_root` in a read transaction before seal, set `chain_root = SnapshotId::full(content_root)`.

A frozen repo (`diverged`, `quota_blocked`, `verify_failed`, `incomplete`, `waiting_base`) stays at its old head. Later bundles still apply to every other repo. Because each bundle carries a window of chained steps, the frozen repo catches up automatically from the next bundle once unblocked (`mer vault repo retry <repo>` re-runs the newest bundle's delta), as long as the window still reaches it; otherwise the report requests Full for that repo. Metric `mer_repo_import_state{repo,state}` and a per-repo row in `/ui/bundles` name the repo at fault.

## 8. Report and wishlist (vault to collector, by hand)

One format. File `<chan16hex>-r<report_seq:06>.mar`: the signed envelope `MSR1` (section 5.2), Report role, domain `mer/report/v1\0`, body cap 256 KiB.

```rust
pub struct Report {
    pub v: u16, pub channel: ChannelId, pub report_seq: u64,
    pub epoch: u32, pub imported_seq: u64, pub trust_version: u32,
    pub software: SoftwareVersion, pub format_max: u16,
    pub gaps: Vec<u64>,                      // <= 256 seqs never imported
    pub repos: Vec<RepoState>,               // {repo, applied_seq, state, full_request}
    pub missing_blobs: Vec<Digest>,          // <= 4096; only digests from import_missing_blob
    pub wishes: Vec<Wish>,                   // <= 4096
    pub denied: Vec<Denied>,                 // {repo, path, digest, reason code}; pairs present in an imported snapshot
    pub link: Option<LinkClass>,             // log2 classes only: loss, burst, passes needed
    pub audit_anchor: Option<AuditAnchor>,   // vault checkpoint; None until M4, mandatory after
}
pub struct Wish { pub id: WishId, pub repo: String, pub root: String, pub env: u8, pub hits_class: u8, pub in_catalog: bool }
```

The report is a high-to-low channel, so it is treated as an exfiltration path:

- **Narrow fields.** No client identities, hostnames, free text, timestamps or exact counts. `hits_class` is `floor(log2(hits))`. `env` is an index into the repo's configured environment list, never a string from a `User-Agent`. There is no blob wish, no digest-form OCI wish and no raw digest except `missing_blobs`, which can only name digests that a verified ops or entries object already named. `root` is a line that the plugin's `parse_root` accepts (strict grammar, at most 256 bytes). Lockfiles uploaded on the vault are reduced there to `name==version` roots.
- **Name catalog.** Bundles carry, per repo, an upstream name catalog (`RepoDelta.catalog`: a sorted name list up to 1M names, otherwise a Bloom filter with 10 bits per name), refreshed at most weekly. A wish whose name is in the catalog can be exported with the normal single approval. Any other wish needs two-person `wishlist.manage` approval, and is highlighted in the rendering.
- **Mandatory review.** `mer vault report show` renders the exact content deterministically (including the audit anchor). Export requires an approved `report_out` row; there is no unattended export. Hard budgets: 4096 wishes, 4096 missing blobs, 256 KiB.
- **Collector side.** `mer collector report ingest` verifies the signature against the paired Report key and requires `report_seq > last_report_seq`. Wishes are untrusted: a wish is fetched unattended only if it matches the repo's `auto_allow` patterns and its name exists in the collector's cached upstream index; names absent from the upstream index are never queried automatically, and upstreams without an enumerable index always need operator approval. Names matching `private_patterns` are dropped on the vault before export and never queried on the collector. Outcomes return in the next manifest's `wish_results`.
- **Miss recording** (vault): only syntactically valid misses from authenticated principals by default, aggregated in memory and flushed every 10 s, capped at 50,000 open rows per repo, rate-limited per client.

What a server can see: a 404 by name (unknown PyPI project, npm packument or tarball, OCI tag or digest, apt index or pool path, raw path). Client-side failures (apt "Unable to locate package", an unsatisfied version inside a present index) are invisible, which is why the collector runs a real solve check (section 9.2) and why `POST /api/v1/wish` and `mer wish` accept explicit roots and lockfiles. The soft-miss heuristic is post-MVP.

## 9. Format plugins

### 9.1 Traits (`mer-format`)

```rust
pub trait FormatPlugin: Send + Sync + 'static {
    fn id(&self) -> &'static str;                       // "apt" | "pypi" | "npm" | "oci" | "raw"
    fn schema_version(&self) -> u32;                    // a bump starts a new derived generation
    fn parse_config(&self, raw: &toml::Table) -> Result<Arc<dyn RepoConfig>, ConfigError>;
    fn upstream(&self) -> Option<&dyn UpstreamSync>;    // collector only (the "Sync" role)
    fn resolver(&self) -> &dyn Resolve;                 // both sides
    fn reindexer(&self) -> &dyn Reindex;                // vault (collector uses it for the solve check)
    fn server(&self) -> &dyn Serve;                     // vault
}
#[async_trait] pub trait UpstreamSync: Send + Sync {
    async fn refresh(&self, cx: &SyncCx<'_>) -> Result<RefreshReport, SyncError>;       // all-or-nothing, rejects rollback
    async fn post_fetch(&self, cx: &SyncCx<'_>, unit: &Unit) -> Result<Vec<Requirement>, SyncError>;
    async fn finalize(&self, cx: &SyncCx<'_>, units: &[Unit], draft: &mut dyn SnapshotDraft) -> Result<(), SyncError>;
}
#[async_trait] pub trait Resolve: Send + Sync {
    fn parse_root(&self, cfg: &dyn RepoConfig, s: &str) -> Result<Vec<Requirement>, ParseError>;
    async fn candidates(&self, cx: &ResolveCx<'_>, req: &Requirement) -> Result<Vec<Unit>, ResolveError>;
    async fn requirements(&self, cx: &ResolveCx<'_>, unit: &Unit, via: &Requirement) -> Result<Vec<Requirement>, ResolveError>;
    /// Real solve restricted to the given contents, per configured root and env. Runs on the collector and again at the vault gate.
    async fn solve_check(&self, cx: &ResolveCx<'_>, roots: &[Requirement]) -> Result<SolveReport, ResolveError>;
}
#[async_trait] pub trait Reindex: Send + Sync {
    async fn verify(&self, cx: &ReindexCx<'_>, snap: &SnapshotView) -> Result<TrustReport, TrustError>;
    async fn reindex(&self, cx: &ReindexCx<'_>, snap: &SnapshotView, out: &mut dyn DerivedSink) -> Result<ReindexReport, ReindexError>;
    fn signing_plan(&self, cx: &ReindexCx<'_>, snap: &SnapshotView) -> Vec<SignRequest>;   // unsigned inputs keyd signs at publish
}
#[async_trait] pub trait Serve: Send + Sync {
    fn mount(&self) -> Mount;                           // UnderRepo | OciV2
    /// The only place a raw path is interpreted. Decodes once, normalises names, resolves the repo (OCI ns mapping).
    fn parse(&self, cx: &ParseCx<'_>, raw_path: &str, method: &Method, query: Option<&str>) -> Result<ParsedRequest, ServeError>;
    async fn handle(&self, cx: &ServeCx<'_>, req: &ParsedRequest, headers: &HeaderMap) -> FormatResponse;
    fn challenge(&self, cx: &ChallengeCx<'_>) -> HeaderValue;
    fn wish_from_miss(&self, m: &MissRow) -> Option<String>;   // a line parse_root accepts
}
pub struct ParsedRequest { pub repo: Option<RepoName>, pub resource: ResourcePath, pub action: Action,
                           pub immutable: bool, pub op: Box<dyn std::any::Any + Send + Sync> }
pub enum FormatResponse {
    Blob     { hit: EntryHit, content_type: ContentType, cache: Cache, extra: HeaderMap, acct: Option<ArtifactRef> },
    Bytes    { status: StatusCode, content_type: ContentType, body: Bytes, cache: Cache, extra: HeaderMap },
    Redirect { status: StatusCode, location: String },
    NotFound { miss: Option<Miss>, body: Option<(ContentType, Bytes)> },
    MethodNotAllowed,
}
```

- Core calls `parse`, authorises on the returned `ResourcePath`, and passes the same `ParsedRequest` to `handle`; the plugin never sees the raw path again. A parse failure is 400; there is no "no resource" case. Cross-format tests assert that every encoded variant of a denied resource (`%2d`, doubled slashes, case variants, `@scope%2Fname`, PEP 503 name variants) yields the same decision.
- `EntryHit` can only be constructed by `Storage::lookup` and `lookup_grace` (section 4.8), so the denylist and snapshot-state checks cannot be bypassed by a plugin.
- `ContentType` is an enum drawn from a per-format allowlist, never a stored upstream header. Core `respond()` adds `X-Content-Type-Options: nosniff`, `Content-Security-Policy: sandbox; default-src 'none'` and `Cross-Origin-Resource-Policy: same-origin` to every format response.
- Plugins never touch the filesystem or sockets. Nothing maps a URL path to a filesystem path.
- The resolve driver in core owns the fixed-point loop (dedupe, cycle safety, budgets in `ClosurePolicy`, the "why included" graph). Exceeding a budget fails the sync with the explain graph.

Sync pipeline in core: `refresh` -> resolve loop (roots = allowlist + accepted wishes) -> parallel fetch into ingest windows (skip present digests; verify every expected hash in one pass) -> `post_fetch` (may add requirements) -> `finalize` -> `solve_check` -> seal.

### 9.2 Closure and the solve check

The closure is an over-approximation (each requirement pulls its best k candidates, conflicts ignored). Because a client's real solver may need to back off to versions the over-approximation did not pull, and such failures are invisible to the vault, every sync is gated by a real solve restricted to the snapshot's contents, per configured root and env:

- apt: an internal EDSP-style solve over the generated indices; the e2e suite additionally runs `apt-get -s install` in a container.
- PyPI and npm: a PubGrub solve (`pubgrub` crate) over the mirrored metadata.
- If a root is unsatisfiable, the versions named by the solver's conflict explanation are added and the closure reruns (bounded rounds, default 5); otherwise the sync fails with the explain graph. The same solve reruns at the vault gate as a check named `closure`.

### 9.3 apt

- **Trust** (collector `refresh`, repeated offline by vault `verify`): fetch `InRelease` (fallback `Release` + `Release.gpg`); verify against the per-repo keyring only; reject SHA-1 and MD5 signature digests; parse only the bytes the OpenPGP verifier returns as signed; reject multi-stanza or NUL payloads; check `Suite`/`Codename`; reject a `Date` older than the last accepted one (anti-rollback state on both sides); fetch indices via `by-hash/SHA256` and verify size and hash before decompressing, capped at the declared size. Chain: pinned upstream key -> Release -> Packages -> `.deb`.
- **Entries.** Artifact `pool/...` (upstream `Filename`); Record `pkg/<suite>/<comp>/<arch>/<Package>/<Version>` (stanza, byte-verbatim); Record `suite/<suite>`; Evidence `up/<suite>/InRelease`, `up/<suite>/<comp>/binary-<arch>/Packages.xz`, `aux/...`.
- **Filename grammar**, enforced at collector ingest and again at vault import (violations fail `refresh`/`verify`): relative, `/`-separated, no empty, `.` or `..` segments, no control characters, ends in `.deb`, `.udeb` or `.ddeb`, does not start with `dists/` or any reserved route prefix.
- **Closure.** Relation grammar with alternatives and versioned Provides; `Multi-Arch` handling; `alternatives = "all"` by default; roots `name`, `name=version`, `@essential`, `@priority:<p>`, `@task:<t>`.
- **Reindex** (unsigned, before the gate): `Packages` (+ `.gz`, `.xz`, deterministic), an unsigned `Release` body template listing exactly the files served with `Acquire-By-Hash: yes` and SHA256 only.
- **Site signature** (keyd, at publish and on a timer): `Release` gets `Date` = vault clock minus a 1 h backdate, clamped to be monotonic per suite, and `Valid-Until` = `Date` + 14 days by default. A vault-side timer re-signs the live snapshot's `Release` (fresh `Date` and `Valid-Until`) every quarter of the validity period. `keyd` refuses to sign unless the snapshot row is `approved`, has `verified_at` and complete derived state. `Origin`, `Label`, `Suite`, `Codename` are copied from upstream. Preview requests get a preview-key signature.
- **Routes** under `/repo/{name}/`: `dists/{suite}/{InRelease,Release,Release.gpg}` (`Revalidate`); indices and aux paths; `dists/{suite}/**/by-hash/SHA256/{hex}`; `pool/**` served `Revalidate` with ETag = digest (third-party repos do replace a `.deb` under the same `Filename`); `site-key.asc`.
- **by-hash.** Resolved against a per-repo in-memory `ByHashIndex` built at publish from every file listed in the generated `Release` (derived and aux evidence) of the window snapshots: the last `rollback_depth` distinct snapshots that were live through a publish, plus anything superseded within `unpublished_grace`. Retention keeps exactly that set. The lookup key is `(suite, directory, digest)` and the request is authorised against the resolved index path, so a caller denied `/dists/jammy-security/` cannot fetch its indices through another suite. After a rollback the rolled-back-from snapshot is not in the window; clients need `apt-get update` after a rollback, which is documented.
- Hand-rolled: strict byte-preserving deb822 reader, Debian version comparison, relation parser. OpenPGP behind an internal trait; rPGP (`pgp`) preferred for static musl, decision after a spike, differential tests against `gpgv` and real `apt`.

### 9.4 PyPI

- **Upstream.** PEP 691 JSON; files without sha256 refused. No upstream index signature exists; trust is TLS plus artifact hashes, optional SPKI pins, lockfile hash roots, and a TOFU rule that a `(filename, sha256)` pair never changes. This is called out at the gate.
- **Entries.** Artifact `files/<project>/<filename>` and `.metadata` (PEP 658; extracted by the collector when upstream lacks it); Record `file/<project>/<filename>`; Record `project/<project>`; Evidence `up/<project>.json`.
- **Closure.** PEP 503 normalisation; tag compatibility ported from `packaging.tags`; versions are filtered per env to those with at least one compatible wheel (or an allowed sdist) before taking the newest k; markers evaluated per env, unknown variables treated as true; sdists are never executed; an opaque sdist unit is a gate failure unless explicitly waived.
- **Reindex.** `simple/index.{html,json}`, `simple/<project>/index.{json,html}` (PEP 691 v1.1 and PEP 503, relative URLs, `core-metadata`). No signing.
- **Routes.** `simple/`, `simple/{project}/` with content negotiation and 301 to the canonical name; `files/...` immutable with range support.
- Crates: `pep440_rs`, `pep508_rs` (verify the maintained line); wheel tags and core metadata reader hand-rolled.

### 9.5 npm

- **Upstream.** Full packument, streamed. Version objects are parsed with a strict parser that rejects duplicate keys at any depth and non-UTF-8 input, then re-emitted from the parsed tree in original key order (registry signatures cover `name@version:integrity`, not the bytes). The closure, the manifest-confusion check and the client therefore see the same object.
- **Trust.** `dist.integrity` verified while streaming; registry signatures (ECDSA P-256) against pinned keys, re-checked offline on the vault; `package/package.json` name and version must match; differing dependencies or scripts produce a gate warning.
- **Entries.** Artifact `tgz/<name>/<version>`; Record `ver/<name>/<version>` (version object plus `time` plus the upstream tarball filename verbatim); Record `pkg/<name>`; Evidence `keys/registry`.
- **Reindex.** `pkg/<name>/{full,abbrev}.json` with the mirrored subset. `dist.tarball = {public_base}/{name}/-/{upstream_filename}` where `public_base` already includes `/repo/{repo}`. A per-package `filename -> version` map is built at reindex and tarballs are served by exact lookup, never by parsing a filename. Invariant: every packument version's tarball filename resolves to a present `tgz` entry. `latest` is the highest mirrored non-prerelease version that is `<=` upstream `latest`; only if none exists, the lowest mirrored version above it, flagged at the gate.
- **Routes.** `/{name}`, `/@{scope}/{name}`, `/@{scope}%2f{name}`; `/{name}/{version|tag}`; `/{name}/-/{file}`; `/-/ping`; `/-/npm/v1/keys`; audit endpoints return `{}`; the legacy login route is opt-in per repo (section 10.2).
- Crates: `nodejs-semver` (verify), `p256`, `ecdsa`.

### 9.6 OCI

- **Upstream.** Bearer token flow, HEAD-first, schema 1 refused, referrers with tag fallback. Bytes are never re-serialised.
- **Entries.** Artifact `m/<name>/sha256:<hex>`, `b/<name>/sha256:<hex>`; Record `t/<name>/<tag>`, `mt/<name>/sha256:<hex>`. Keys are per image name on purpose: a blob is pullable from `<name>` only if linked there.
- **Content types.** Manifests are served with a media type from the registered manifest and index allowlist, cross-checked against the body at ingest; anything else is `application/octet-stream`. Blobs are always `application/octet-stream`.
- **Routes** under `/v2/`: repo selected by first path segment or by `?ns=` through `oci.ns_map`, inside `parse`, so `_catalog`, `tags/list` and every other route are authorised after mapping. Push verbs return 405 with the spec error body.
- Platform filtering keeps indexes byte-exact and fetches only matching children. Cosign keyed verification and PEP 740 style evidence fetching are post-MVP.

### 9.7 raw

- Sources: pinned sha256, signed sums file (OpenPGP or minisign), TOFU, TLS-only; the trust level is recorded per file and shown at the gate. Paths are normalised (NFC, no `.`/`..`, no control characters, at most 1024 bytes, case-insensitive collisions rejected).
- **Entries.** Artifact `f/<path>`; Record `meta/<path>`; Evidence `sums/...`.
- **Reindex.** Directory listings and a `SHA256SUMS` per directory that includes a signed header with the snapshot seq and signing time; signed by keyd at publish.
- Used as the test format for M2 because it has no closure and no upstream.

### 9.8 Collector egress policy (`mer-fetch`)

URLs taken from upstream metadata (PyPI `files[].url`, npm `dist.tarball`, OCI descriptor `urls`, Bearer `realm`) are untrusted.

- Per-repo allowlist of hosts for artifacts, redirects and token realms (default: the upstream host plus explicitly configured CDN and auth hosts). https only.
- A custom DNS resolver rejects loopback, RFC 1918, link-local, ULA and cloud metadata addresses after resolution and pins the resolved address for the connection.
- Manual redirect policy, at most 5 hops, each hop re-checked. Credentials are keyed to the configured host and realm; a realm not on the allowlist fails the sync.
- Raw wish paths are joined with `Url::join` after normalisation and must remain under `base`.

## 10. Access control

### 10.1 Authentication

`Principal { subject: Anonymous | User | Service, groups, grants, method, token, session }`. Credential extraction is first-match; a present but invalid credential is 401 and never falls through to anonymous.

1. `Authorization: Bearer`: API token (`mer_<id:12>_<secret:40>_<crc:6>`, base62; DB stores `sha256(secret)`; constant-time compare) or registry JWT.
2. `Authorization: Basic`: password `mer_...` is a token; otherwise local user, then LDAP.
3. Session cookie, accepted on the admin listener only.
4. TLS client certificate (optional mTLS).

Providers: local (argon2id in `spawn_blocking` behind a semaphore, hash upgrade on login), LDAP/AD search-then-bind (empty password rejected before bind, `ldap_escape`, exactly one entry, circuit breaker returning 503), OIDC code flow with PKCE for the browser UI. A verified-credential cache (HMAC-keyed, 60 s) keeps apt and pip Basic auth cheap. A directory resync task suspends tokens of disabled users, with a bounded `directory_grace`.

### 10.2 Tokens

- Every token carries an explicit scope set. A token is created as `repo.read` on selected repos; there is no "inherits the owner" default.
- Tokens of kind `api` or `npm` can never hold global or snapshot permissions. `/api/v1` mutation routes reject them.
- Admin automation uses a `service` token created under `token.manage_any` with explicit bindings, optional `allowed_cidrs` and a mandatory expiry.
- A token can never exceed its owner at evaluation time (effective = owner permissions intersected with token bindings).
- The npm legacy login route (`PUT /-/user/org.couchdb.user:{name}`) is opt-in per repo, goes through the same `governor` buckets, lockout counters and audit event as `/auth/login`, and issues a token bound to that one repo with `repo.read`.
- Docker token service: `/auth/docker/token` returns a 300 s EdDSA JWT whose access list is the requested scope intersected with RBAC; it carries the `rbac_epoch` at issue and is re-checked when stale.

### 10.3 RBAC

- Permissions (fixed enum, u64 bitset). Repo-scoped: `repo.read`, `repo.write` (reserved), `repo.admin`, `snapshot.preview`, `snapshot.approve`, `snapshot.publish`, `wishlist.read`, `wishlist.manage`, `sync.run`, `stats.read`. Global: `bundle.import`, `bundle.export`, `user.manage`, `role.manage`, `token.manage_any`, `audit.read`, `audit.export`, `system.read`, `system.admin`, `metrics.read`.
- Built-in roles: `admin`, `repo-admin`, `approver`, `publisher`, `operator`, `reader`, `auditor`, `monitor`.
- Binding `(subject, role, repo_pattern, path_prefix, effect)`. Evaluation: any matching `deny` wins, else any `allow`, else deny. `path_prefix` matches at segment boundaries against the canonical `ResourcePath` from `Serve::parse` (apt: URL path under the repo; PyPI: `/<normalized-project>/`; npm: `/<name>` or `/@scope/<name>`; OCI: `/<image path>`; raw: URL path).
- `GrantSet` per principal cached by `(subject, token, rbac_epoch)`; `rbac_epoch` is bumped in the same transaction as any change.
- Limitation, documented: signed aggregate indexes are not filtered per caller; path rules restrict fetching, not discoverability.

### 10.4 Approval and quarantine gate

The gate is a view over storage state plus three access tables: a `sealed` snapshot is "quarantined" while `snapshot_check` rows are not all `pass`, and "pending" until enough `snapshot_review` approvals exist.

- Policy per repo: `mode` (`auto` | `manual`; soak is post-MVP), `required_approvals`, `distinct_from_importer`, `auto_publish`, `required_checks`.
- Checks: built-in `sig` (bundle and upstream verification results), `integrity`, `closure` (section 9.2); one external command hook (`[[approval.check]]`, for example ClamAV), run in the worker with a read-only view.
- A review binds `content_digest = SnapshotId`; the transition is a state-guarded `decide` in storage (409 on a race).
- `auto` never approves a tainted snapshot, and still requires all checks.
- Reject semantics are those of section 4.9; there is no "GC-eligible on reject".
- Publishing N+1 while N is unapproved leaves N `sealed` until `undecided_ttl`; the review diff of N+1 is against the last approved snapshot, so it includes everything N added.

### 10.5 Access DDL (migration `0004_access.sql`, M4)

Singular names, timestamps in unix ms, same columns as the access draft with these changes: `token` has no `restrict` column and gains `scope_json`; `session.last_seen_at` and `token.last_used_at` live in `stats.db`.

```sql
CREATE TABLE user (id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE COLLATE NOCASE,
  source TEXT NOT NULL CHECK (source IN ('local','ldap','oidc','mtls')), provider TEXT, external_id TEXT,
  display_name TEXT, email TEXT, password_phc TEXT, must_change_pw INTEGER NOT NULL DEFAULT 0,
  disabled INTEGER NOT NULL DEFAULT 0, failed_logins INTEGER NOT NULL DEFAULT 0, locked_until INTEGER,
  directory_synced_at INTEGER, created_at INTEGER NOT NULL, last_login_at INTEGER, UNIQUE (provider, external_id)) STRICT;
CREATE TABLE user_group (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, description TEXT) STRICT;
CREATE TABLE group_member (group_id INTEGER NOT NULL REFERENCES user_group(id) ON DELETE CASCADE,
  user_id INTEGER NOT NULL REFERENCES user(id) ON DELETE CASCADE,
  origin TEXT NOT NULL CHECK (origin IN ('local','directory')), PRIMARY KEY (group_id, user_id, origin)) STRICT, WITHOUT ROWID;
CREATE TABLE group_mapping (provider TEXT NOT NULL, external_group TEXT NOT NULL COLLATE NOCASE,
  group_id INTEGER NOT NULL REFERENCES user_group(id) ON DELETE CASCADE,
  PRIMARY KEY (provider, external_group, group_id)) STRICT, WITHOUT ROWID;
CREATE TABLE role (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, builtin INTEGER NOT NULL DEFAULT 0, description TEXT) STRICT;
CREATE TABLE role_permission (role_id INTEGER NOT NULL REFERENCES role(id) ON DELETE CASCADE,
  permission TEXT NOT NULL, PRIMARY KEY (role_id, permission)) STRICT, WITHOUT ROWID;
CREATE TABLE token (id TEXT PRIMARY KEY, name TEXT NOT NULL, kind TEXT NOT NULL CHECK (kind IN ('api','npm','service')),
  owner_id INTEGER REFERENCES user(id) ON DELETE CASCADE, secret_sha256 BLOB NOT NULL CHECK (length(secret_sha256) = 32),
  scope_json TEXT NOT NULL, allowed_cidrs TEXT, created_by INTEGER REFERENCES user(id), created_at INTEGER NOT NULL,
  expires_at INTEGER, revoked_at INTEGER, suspended INTEGER NOT NULL DEFAULT 0,
  CHECK (kind <> 'service' OR expires_at IS NOT NULL)) STRICT;
CREATE TABLE binding (id INTEGER PRIMARY KEY,
  subject_kind TEXT NOT NULL CHECK (subject_kind IN ('user','group','token','anonymous','authenticated')),
  subject_id TEXT, role_id INTEGER NOT NULL REFERENCES role(id) ON DELETE CASCADE,
  repo_pattern TEXT NOT NULL DEFAULT '*', path_prefix TEXT NOT NULL DEFAULT '/' CHECK (substr(path_prefix, 1, 1) = '/'),
  effect TEXT NOT NULL DEFAULT 'allow' CHECK (effect IN ('allow','deny')), managed INTEGER NOT NULL DEFAULT 0,
  created_by INTEGER, created_at INTEGER NOT NULL, expires_at INTEGER,
  CHECK ((subject_kind IN ('anonymous','authenticated')) = (subject_id IS NULL))) STRICT;
CREATE INDEX binding_subject ON binding(subject_kind, subject_id);
CREATE TABLE session (id_sha256 BLOB PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES user(id) ON DELETE CASCADE,
  auth_method TEXT NOT NULL, csrf_secret BLOB NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
  ip TEXT, user_agent TEXT) STRICT, WITHOUT ROWID;
CREATE TABLE repo_approval_policy (repo_id INTEGER PRIMARY KEY REFERENCES repo(id) ON DELETE CASCADE,
  mode TEXT NOT NULL CHECK (mode IN ('auto','manual')), required_approvals INTEGER NOT NULL DEFAULT 1,
  distinct_from_importer INTEGER NOT NULL DEFAULT 1, auto_publish INTEGER NOT NULL DEFAULT 1,
  required_checks TEXT NOT NULL DEFAULT '[]') STRICT;
CREATE TABLE snapshot_review (id INTEGER PRIMARY KEY, snapshot_id INTEGER NOT NULL REFERENCES snapshot(id),
  content_digest BLOB NOT NULL, reviewer_id INTEGER NOT NULL REFERENCES user(id),
  decision TEXT NOT NULL CHECK (decision IN ('approve','reject')), comment TEXT, created_at INTEGER NOT NULL,
  UNIQUE (snapshot_id, reviewer_id)) STRICT;
CREATE TABLE snapshot_check (snapshot_id INTEGER NOT NULL REFERENCES snapshot(id), name TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('pending','running','pass','fail','error')), detail TEXT,
  started_at INTEGER, finished_at INTEGER, PRIMARY KEY (snapshot_id, name)) STRICT, WITHOUT ROWID;
CREATE TABLE audit_checkpoint (seq INTEGER PRIMARY KEY, hash BLOB NOT NULL, ts INTEGER NOT NULL,
  key_id TEXT NOT NULL, signature BLOB NOT NULL, anchored TEXT NOT NULL DEFAULT '[]') STRICT;
CREATE TABLE audit_foreign_anchor (instance_id TEXT NOT NULL, seq INTEGER NOT NULL, hash BLOB NOT NULL, key_id TEXT,
  signature BLOB, carried_by TEXT NOT NULL, received_at INTEGER NOT NULL, PRIMARY KEY (instance_id, seq)) STRICT, WITHOUT ROWID;
```

### 10.6 Audit log

- Append-only, hash-chained, in `meta.db` from migration 0001. `MetaWrite::audit_append(ev)` reads `audit_head`, inserts and updates inside the caller's write job, so the state change and its record commit atomically and a Postgres re-run cannot double-append. If the append fails, the action fails.
- Events without a state mutation (login failure, authz denial on `/api`) are written synchronously through an `Interactive` write job before the response is sent.
- Byte formats:
  ```
  record   = JSON {"v":1,"seq":N,"ts":ms,"instance":"<uuid>","actor":{...},"method":"..","ip":"..","request_id":"..",
                   "action":"snapshot.publish","target":{...},"outcome":"ok|denied|error","detail":{...}}
  hash[N]  = SHA256("mer/audit/v1\0" || u64be(N) || prev_hash || u32be(len(record)) || record)
  hash[0]  = SHA256("mer/audit-genesis/v1\0" || instance_id)
  checkpoint = Ed25519("mer/audit-ckpt/v1\0" || instance_id[16] || u64be(seq) || hash || u64be(ts))
  ```
- Audited set: login success, coalesced failures and logout; token, user, group, role and binding changes; repo and policy changes; bundle import and export; snapshot seal, review, reject (with the denylist rows), publish, rollback, taint override; denylist edits; trust doc application, epoch change, pairing, `trust set-min`; report approval, export and ingest; heartbeat regressions; GC runs; config reload; key rotation; self-update staging and application; audit export and verify.
- Flood control: failed logins are coalesced per IP (not per attacker-chosen username) with a capped distinct-username count per window, a global per-minute ceiling, and overflow into a single counter record.
- **External anchoring is mandatory on the vault from M4**: the vault role refuses to start unless at least one external sink is configured (syslog forwarder or an append-only file on a separate WORM or `chattr +a` mount). A signed checkpoint is emitted every 1000 records or 10 minutes. Cross-anchoring: the vault's latest checkpoint goes into every exported report and is printed in the deterministic rendering; the collector's goes into every manifest; each side stores the other's in `audit_foreign_anchor`.
- The checkpoint key is held by `keyd`, outside the data directory. Tail truncation is detectable only up to the last external anchor; this limit is documented.
- `mer audit verify` and `POST /api/v1/audit/verify` recompute the chain, verify checkpoints and compare anchors; an hourly job checks the newest 10k records (`mer_audit_chain_ok`).

## 11. HTTP surface

### 11.1 Listeners and origins

| Listener | Serves | Notes |
|---|---|---|
| `package` | `/repo/...`, `/v2/...`, `/auth/docker/token`, `/healthz` | Token, Basic, JWT and mTLS only. Session cookies are refused here. |
| `admin` (mandatory, separate origin: own address or hostname) | `/ui`, `/api/v1`, `/auth/login`, `/auth/logout`, `/auth/oidc/*` | Cookie sessions with CSRF header, strict CSP. Mirrored content is never served from this origin. |
| `metrics` | `/metrics`, `/healthz`, `/readyz` | Requires `metrics.read` or the bearer token unless bound to loopback. |

- Hand-written accept loop over `hyper-util` `auto::Builder` with `tokio-rustls` (ALPN `h2`, `http/1.1`), hot certificate reload, per-IP and global connection caps, 10 s header and handshake timeouts, body stall watchdog. h2c is disabled unless `server.behind_proxy = true`. Credentials on plaintext are refused unless explicitly allowed.
- Middleware (outermost first): request id, `CatchPanic`, trace and metrics, client-IP resolution, admission limits (API/UI only), `authn`, router -> `Serve::parse` -> `authz.require` -> `Serve::handle` -> `respond()`, accounting body wrapper.
- `respond()` implements RFC 9110 preconditions and single-range requests once for all formats, always sends the security headers of section 9.1, and derives embedded absolute URLs only from `server.public_url`.
- **Transfer.** The MVP has one mode, `stream`: `FileBody` does positional reads of 512 KiB chunks on the blocking pool with `posix_fadvise(SEQUENTIAL)`; one copy from page cache. Post-MVP fast paths must sit behind the normal middleware: a plaintext sendfile listener accepts only single-use HMAC tickets (30 s, bound to digest and client IP) minted by the authorised request; `X-Accel-Redirect` ships with an `internal;` proxy config and strips client-supplied `X-Accel-*`; S3 presigned redirects have a TTL of at most 60 s, re-check the denylist at mint time and are disabled for repos with path-scoped RBAC.

### 11.2 Routes

Package listener:

| Method | Path | Handler |
|---|---|---|
| GET, HEAD | `/repo/{name}[@{seq}]/{*rest}` | apt, PyPI, npm, raw via `Serve` (`repo_spec` resolved in core, section 4.8) |
| GET, HEAD | `/v2/`, `/v2/{*rest}` | OCI distribution |
| PUT | `/repo/{name}/-/user/org.couchdb.user:{user}` | npm legacy login, opt-in per repo |
| GET | `/auth/docker/token` | registry token service |
| other verbs | format routes | 405 |

Admin API (`/api/v1`, JSON, RFC 9457 errors, cursor pagination, `If-Match` on single-resource mutations, long operations return `202 {job_id}`). R: C collector, V vault, B both.

| Method | Path | Permission | R | Purpose |
|---|---|---|---|---|
| GET | `/me`; GET, POST `/me/tokens`; DELETE `/me/tokens/{id}`; PUT `/me/password`; GET, DELETE `/me/sessions[/{id}]` | authenticated | B | Self-service |
| GET, POST, PATCH, DELETE | `/users[/{id}]`, `/users/{id}/sessions`, `/groups[/{id}]`, `/groups/{id}/members/{user}`, `/groups/{id}/mappings` | user.manage | B | Users and groups |
| GET, POST, PATCH, DELETE | `/roles[/{id}]`, `/permissions`, `/bindings[/{id}]`; POST `/authz/explain` | role.manage | B | RBAC |
| GET, POST, PATCH, DELETE | `/tokens[/{id}]` | token.manage_any | B | All tokens, service tokens |
| GET, POST | `/repos`; GET, PATCH, DELETE `/repos/{r}` | repo.read / repo.admin / system.admin | B | Repos, quota, retention |
| GET, PUT | `/repos/{r}/selectors` | repo.admin | C | Allowlist and closure settings |
| GET, PUT | `/repos/{r}/approval-policy` | repo.admin | V | Gate policy |
| GET, POST, DELETE | `/repos/{r}/denylist[/{id}]` | repo.admin | V | Denylist (removal audited) |
| GET | `/repos/{r}/snapshots`, `/repos/{r}/snapshots/{seq}`, `.../diff?against=`, `.../contents?q=` | repo.read / snapshot.preview | B | Snapshots, diff against last approved |
| GET | `/snapshots?state=` | snapshot.preview | V | Cross-repo approval queue incl. tainted |
| POST | `/repos/{r}/snapshots/{seq}/reviews` | snapshot.approve | V | `{decision, comment, content_digest, items?}` |
| POST | `/repos/{r}/snapshots/{seq}/checks/{name}/retry` | snapshot.approve | V | Re-run a check |
| POST | `/repos/{r}/snapshots/{seq}/publish` | snapshot.publish | V | Publish (`override_taint` needs a reason) |
| POST | `/repos/{r}/rollback` | snapshot.publish | V | Re-publish an older approved snapshot |
| POST | `/repos/{r}/reindex`, `/repos/{r}/import/retry` | repo.admin | V | Derived rebuild; retry a frozen repo |
| GET, PATCH | `/repos/{r}/misses` | wishlist.read / wishlist.manage | V | Misses, bulk state change |
| POST | `/wish` | wishlist.manage | V | Explicit roots or lockfile (reduced to `name==version`) |
| GET, POST | `/reports`, `/reports/{seq}`, `/reports/{seq}/approve`, `/reports/{seq}/export` | wishlist.manage | V | Draft, render, approve (second approver when required), export |
| POST | `/reports/ingest` | wishlist.manage | C | Verify and ingest a report |
| GET, PATCH | `/wishes`, `/wishes/{id}` | wishlist.read / wishlist.manage | C | Accept or deny ingested wishes |
| GET | `/sync`; POST `/repos/{r}/sync` | repo.read / sync.run | C | Sync status, trigger |
| GET | `/channels`, `/channels/{c}`, `/bundles`, `/bundles/{seq}` | system.read | B | Channel state, history, per-repo import state, airtime |
| POST | `/bundles/seal`, `/bundles/{seq}/export`, `/bundles/{seq}/requeue` | bundle.export | C | Seal, media export, carousel requeue |
| PUT | `/channels/{c}/vault-version` | bundle.export | C | Set `emit_compat` |
| POST | `/bundles/import` | bundle.import | V | Import from a mounted path |
| GET, DELETE | `/staging`, `/staging/{seq}` | bundle.import | V | Staging list and purge |
| GET, POST | `/trust`, `/trust/min`, `/trust/confirm-epoch` | system.admin | V | Trust chain, floor, epoch confirmation |
| GET, POST | `/selfupdate`, `/selfupdate/{build}/apply` | system.admin | V | Staged updates |
| GET | `/diode/status` | system.read | B | Carousel or receiver state |
| GET, POST | `/jobs`, `/jobs/{id}`, `/jobs/{id}/log`, `/jobs/{id}/cancel` | system.read / system.admin | B | Jobs |
| GET | `/stats/traffic`, `/stats/top-packages`, `/stats/clients[/{kind}/{id}]`, `/stats/storage` | stats.read | B | Dashboard data |
| GET, POST | `/audit`, `/audit/{seq}`, `/audit/checkpoints`, `/audit/verify`, `/audit/export` | audit.read / audit.export | B | Audit |
| GET, POST, PUT | `/system/status`, `/system/config`, `/system/reload`, `/system/log-level`, `/system/tls`, `/gc`, `/gc/last`, `/meta/backup` | system.read / system.admin | B | Operations |

### 11.3 Dashboard

Server-rendered HTML (`askama`), vendored `htmx` and `uPlot`, one CSS file, embedded with `rust-embed`; no Node toolchain, no CDN, strict CSP without inline script. UI handlers call the same service functions as `/api/v1` with identical RBAC. Pages: overview, repos, snapshot detail with diff and gate, approvals (including tainted), bundles (history, per-repo import state, gaps, airtime ETA, staging), wishlist and reports, sync, clients, access, audit, system, account.

## 12. Metrics and observability

One registry. Label rules: never label by client IP, username, package or raw path; `route` is the matched template; `repo` is bounded by configuration. Expensive gauges are maintained as counters inside the transactions that change them and read from atomics refreshed at most every 15 s.

| Area | Metrics |
|---|---|
| Build | `mer_build_info{version,commit,role}` |
| HTTP | `mer_http_requests_total{listener,format,route,method,status}`, `mer_http_request_duration_seconds`, `mer_http_ttfb_seconds`, `mer_http_response_body_bytes_total{repo,format}`, `mer_http_requests_in_flight`, `mer_http_connections_open{listener,proto}`, `mer_http_connections_rejected_total{reason}`, `mer_http_aborted_responses_total{repo,reason}`, `mer_http_range_requests_total`, `mer_http_not_modified_total`, `mer_http_panics_total`, `mer_tls_handshake_errors_total`, `mer_tls_cert_not_after_timestamp_seconds` |
| Auth | `mer_auth_attempts_total{method,provider,outcome}`, `mer_auth_verify_duration_seconds`, `mer_auth_cache_total{result}`, `mer_authz_denied_total{repo,permission}`, `mer_auth_provider_up{provider}`, `mer_sessions_active`, `mer_tokens_active` |
| Repos | `mer_repo_downloads_total{repo,format,kind}`, `mer_repo_misses_total{repo,format,reason}`, `mer_wishlist_pending_entries{repo}`, `mer_repo_live_snapshot_seq`, `mer_repo_live_snapshot_published_timestamp_seconds`, `mer_repo_live_upstream_timestamp_seconds`, `mer_snapshots{repo,state}`, `mer_snapshots_tainted{repo}`, `mer_snapshot_oldest_pending_timestamp_seconds`, `mer_snapshot_transitions_total{repo,to_state}`, `mer_repo_referenced_bytes`, `mer_repo_quota_bytes`, `mer_repo_import_state{repo,state}` |
| Storage | `mer_blobstore_bytes`, `mer_blobstore_objects`, `mer_blobstore_free_bytes`, `mer_blobstore_op_duration_seconds{backend,op}`, `mer_blobstore_errors_total`, `mer_blobstore_integrity_failures_total`, `mer_blob_leases`, `mer_blobs_pending`, `mer_gc_runs_total{outcome}`, `mer_gc_phase`, `mer_gc_reclaimed_bytes_total`, `mer_gc_last_success_timestamp_seconds`, `mer_scrub_pass_progress_ratio`, `mer_scrub_last_full_pass_timestamp_seconds`, `mer_meta_cache_hit_ratio`, `mer_meta_last_backup_timestamp_seconds` |
| DB | `mer_db_writer_queue_depth`, `mer_db_write_tx_seconds`, `mer_db_wal_bytes`, `mer_db_size_bytes{db}`, `mer_db_query_duration_seconds{op}` |
| Bundles | `mer_bundle_exports_total{transport,outcome}`, `mer_bundle_imports_total{transport,outcome}`, `mer_bundle_bytes_total{direction,transport}`, `mer_bundle_last_seq{direction}`, `mer_bundle_missing_seqs`, `mer_bundle_import_stage_seconds{stage}`, `mer_staging_bytes{state}`, `mer_channel_staleness_seconds{channel}`, `mer_trust_version{channel}`, `mer_trust_version_advertised{channel}` |
| Diode | `mer_diode_symbols_total{direction,kind}`, `mer_diode_packets_dropped_total{reason}` (bad_mac, replay, unknown_bundle, param_mismatch, backpressure), `mer_diode_blocks_total{outcome}` (systematic, decoded, poisoned), `mer_diode_segments_missing{seq}`, `mer_diode_decode_queue_depth`, `mer_diode_block_decode_seconds`, `mer_diode_carousel_pass`, `mer_diode_queue_airtime_seconds`, `mer_diode_heartbeat_replays_total`, `mer_diode_link_loss_ratio` |
| Collector | `mer_sync_runs_total{repo,outcome}`, `mer_sync_duration_seconds`, `mer_sync_last_success_timestamp_seconds`, `mer_sync_fetched_bytes_total`, `mer_sync_fetch_errors_total`, `mer_upstream_signature_failures_total`, `mer_wishlist_ingested_items_total{repo,outcome}`, `mer_egress_blocked_total{reason}` |
| Audit, jobs | `mer_audit_head_seq`, `mer_audit_last_checkpoint_timestamp_seconds`, `mer_audit_chain_ok`, `mer_audit_records_total{class}`, `mer_job_progress_ratio{kind}`, `mer_integrity_failures_total{stage}`, `mer_traffic_events_dropped_total`, `mer_config_reloads_total`, `process_*` |

Logging: `tracing` JSON to stdout, one access-log event per response at body completion, runtime level changes, secrets as `secrecy::SecretString`, `Authorization` and cookies never logged. Per-client traffic accounting through a bounded channel into `stats.db` (drops counted, never blocks serving).

## 13. Configuration

Precedence: defaults < `/etc/mer/config.toml` < `conf.d/*.toml` < `MER__SECTION__KEY` < a few CLI flags. `deny_unknown_fields` everywhere. The file holds infrastructure; the DB holds what operators change at runtime (repos, selectors, users, bindings, policies), with optional idempotent `provision.d/*.toml`. Secrets are `{ file = ... }` or `{ env = ... }`. Role and instance id are pinned in `meta_kv`; a mismatch refuses to start.

```toml
role = "vault"
data_dir = "/var/lib/mer"

[storage]
backend = "fs"                      # fs | s3
inline_max = 8192
durability = "batch"                # batch | per_blob
max_bytes = "0"                     # 0 = unlimited
min_free_bytes = "10GiB"

[meta]
readers = 8
backup_keep = 7
backup_schedule = "daily"

[staging]
max_bytes = "600GiB"
max_bundles = 8

[[channel]]
name = "main"
pair_file = "/etc/mer/channel.pair"
accept_window = 64
max_staleness = "36h"
[channel.media]
in_dirs = ["/media/in"]
[channel.diode_rx]
bind = "10.9.0.2:9100"
link_key = { file = "/run/secrets/diode_link_key" }
pool_blocks = 136
rcvbuf = "64MiB"
decode_threads = 3
partial_budget = "2GiB"

[server]
public_url = "https://vault.lan"
behind_proxy = false
[server.listen.package] addr = "0.0.0.0:8443"
[server.listen.admin]   addr = "10.0.9.5:9443"
[server.listen.metrics] addr = "127.0.0.1:9090"
[server.tls]
cert = "/etc/mer/tls/fullchain.pem"
key  = { file = "/run/secrets/tls_key" }

[auth]
anonymous = "off"                   # off | per_repo
[audit]
syslog = "udp://10.0.9.9:514"       # at least one external sink is mandatory on the vault
```

Collector differences: `role = "collector"`; `[channel]` has `blob_window`, `step_window`, `history_keep`, `segment_size`, `signer = { file = ... }`; `[channel.media]` has `out_dir`, `volume_size = "fat32"`; `[channel.diode_tx]` has `dest`, `bind`, `rate_bps`, `symbol_size = 1368`, `overhead = 0.10`, `burst_target = "1s"`, `interleave_max = 128`, `initial_passes = 2`, `min_passes = 4`, `carousel_ttl = "72h"`, `max_bundle_airtime = "12h"`, `link_key`; `[[repo]]` sections carry upstream, roots and `[repo.egress]` allowlists.

## 14. Threat model

| Adversary | Capability | Controls | Residual |
|---|---|---|---|
| Hostile or compromised upstream | Serves malicious metadata, URLs, archives | Per-repo upstream keys, hash chains, egress allowlist and SSRF-safe resolver, strict parsers, bombs capped by signed sizes, solve check, quarantine gate | Formats without upstream signatures (PyPI) rest on TLS plus lockfile hashes |
| Compromised collector (fetch process) | Writes arbitrary snapshots | Cannot sign bundles (key in the export process or HSM); vault re-verifies upstream trust before sealing | A compromised export process can sign bundles |
| Stolen Bundle key | Signs bundles | Cannot rotate trust, change epoch, ship policy or binaries (Root, Policy, Release roles); tombstones are proposals; vault-side upstream verification; gate; optional `bundle_threshold > 1`; removal by root-signed trust doc evaluated against vault state | Can inject content that passes upstream verification for formats without upstream signatures |
| On-link attacker at the diode | Injects, replays, withholds packets | Mandatory MAC, session counter, replay window, nothing allocated before a verified descriptor, signed monotonic heartbeats, staleness alert | Withholding is a denial of service, detected by staleness |
| Malicious media or scanning station | Crafted volumes, symlinks, swapped files | Header caps, descriptor verified before use, `O_NOFOLLOW`, regular files only, names ignored, copy-always import, Merkle proof per slot | Destroying media is a denial of service |
| Enclave insider or malware | Exfiltration through the report; poisoning the wishlist | Narrow report fields, catalog gate, two-person approval, budgets, deterministic rendering; collector treats wishes as untrusted | Low-bandwidth channel through approved wish selection |
| Enclave network attacker | Replays old signed indices; steals tokens | `Valid-Until` with re-signing; signatures only at publish; preview key distinct; least-privilege tokens; TLS | Replay within the validity window |
| Hostile artifact content | Stored XSS against admins | Separate admin origin, cookies refused on the package listener, sandbox CSP and nosniff on every format response, content-type allowlist | None known |
| Local attacker with file access on the vault | Rewrites audit history | Mandatory external anchors, cross-anchoring, checkpoint key in `keyd` | Unanchored tail |
| Malformed or hostile bundle content reaching the vault | Parser crashes, resource exhaustion | Worker process isolation, limits before allocation, signatures before decoding, attempt counter, fuzzing of every vault-side parser | Worker denial of service until an operator purges |

## 15. Failure modes (summary)

| Failure | Handling |
|---|---|
| Truncated or bit-flipped blob stream | Digest and size checked before placement; nothing becomes visible |
| Unverified file already at the final path | `CommitMode::Replace` renames verified bytes over it |
| Power loss under `Durability::Batch` | fds fdatasynced and shard dirs fsynced before `mark_present`; pending rows re-hashed on recovery |
| `COMMIT` fails after jobs ran | Results are withheld until commit; the group is re-run job by job |
| Two publishes race | Generation CAS; swap in commit order; reconciliation poll |
| Snapshot retired under a reader | `LeaseRegistry` retiring flag; grace periods; open fd |
| Aborted or diverged build | Seq allocated at begin; failed seal aborts in the same transaction; pre-emption |
| Lost or corrupt volume, bad sectors | Fixed slots isolate damage; exact missing list; re-copy or complete via diode |
| Random or burst diode loss, receiver down | RaptorQ overhead, interleaving sized from rate, alternating source and repair passes, carousel, heartbeats reveal `head_seq` |
| Receiver disk slower than link | Bounded queues, drops counted, next pass refills |
| Vault missed a bundle | Step window carries metadata; blobs surface as `incomplete`; carousel, media re-export, report |
| One repo cannot apply | Only that repo freezes; `repo_import.state` names why; automatic catch-up inside the step window |
| Bundle waits longer than any TTL | Leases bound to `bundle_import`; `punched.bitmap` keeps staging honest |
| Collector crash during seal | Spool then one commit transaction; orphan spool directories removed at start |
| Collector and vault diverge | Chain root mismatch per repo; periodic `content_root` audit; per-repo Full |
| Rejected content | Denylist rows in the reject transaction; taint; never served |
| Vault cannot parse a newer format | `min_reader_version` and `features` refuse before staging; extras extractable; `emit_compat` |
| `meta.db` lost | Nightly backup, publish checkpoints, `mer meta rebuild` |
| Clock drift or dead RTC | No wall-clock validity on bundles; audit order by seq; monotonic clamp for GC and site signatures; skew gauge |
| Disk full | `statvfs` preflight, `fallocate`, staging budget, session suspends and resumes |

## 16. Testing strategy

| Level | What |
|---|---|
| Unit and golden | Codec round-trips for every byte layout in this document, with golden vectors under `testdata/`; version N-1 bundle fixtures |
| Blob conformance | One suite over fs, mem and S3 (MinIO in CI), including `Replace` over a torn file and `LinkIfAbsent` verification |
| Model check (`proptest`) | Interval table against a naive `BTreeMap<Seq, BTreeMap<Path, Entry>>` over put, remove, seal, abort, retire, compact; `repo_usage` equals a recomputation after every operation; a rejected digest is never reachable from a live pointer, pinned snapshot or grace fallback |
| Delta soundness (`proptest`) | Random collector histories; for every vault position within the step window, applying the bundle yields snapshot contents equal to the collector's (not only blob closure); dropped bundles converge through step windows, gap fills and reports |
| Crash recovery (`fail`) | Failpoints at every arrow of I4, between T1/T2/T3 of an ingest window, between repos of one bundle, at each collector seal step, at each import transition; kill -9 loops under `Durability::Batch`; then `fsck` asserts I1 to I8 |
| Concurrency | Ingest versus GC with `gc_grace = 0`; concurrent digests across sessions with one corrupt stream; publish ordering under contention; two processes on one `meta.db` |
| Simulated diode | `mer_testkit::LossyLink` (Bernoulli loss, Gilbert-Elliott bursts, reorder, duplication, truncation, corruption, rate cap) under `tokio::time::pause()`; a corrupted or injected symbol never yields a wrong block; MAC, replay and session tests; heartbeat replay is rejected |
| Fuzz (`cargo-fuzz`, nightly) | Envelope, descriptor, manifest, ops sequence, blob index, slot, volume header, diode packet, report, and every vault-side format parser (deb822, OpenPGP framing, PEP 691, packuments, OCI manifests, archive readers) |
| Access | Every encoded variant of a denied resource yields the same decision across formats; token scope matrix; origin separation (no cookie on the package listener) |
| E2E with real clients | `tests/e2e` with `testcontainers`: `apt-get`, `pip`, `uv`, `npm`, `pnpm`, `skopeo`, `docker pull` against a vault fed through media and through the simulated diode from a fixture upstream; the vault and clients have no route out, asserted |
| Performance | criterion: ingest at least 5,000 small blobs per second on NVMe; seal and import throughput; diode at least 800 Mbit/s lossless and 300 Mbit/s at 3 percent loss on 4 cores; `EXPLAIN QUERY PLAN` assertions for lookup and diff |

## 17. Milestones

| Step | Scope | Done when |
|---|---|---|
| M0 | Workspace, `mer-core` frozen by review, CI skeleton, layer check | `cargo check` green; contract types compile |
| M1 | `mer-blob`, `mer-meta` (migration 0001), `mer-storage`: ingest windows, interval snapshots, seal, publish and rollback, lookup cache, denylist and taint, GC, quota helper, retention, checkpoints, backup, jobs, fsck | Model check, crash tests and ingest bench pass |
| M2 | `mer-crypto`, `mer-fec`, `mer-bundle-format`, `mer-bundle`, both transports, migration 0002, report file, `mer` CLI with the raw test format | Media and simulated-diode round trips, delta-soundness property, diode bench, golden vectors |
| M3 | `mer-format`, `mer-fetch` with egress policy, apt and PyPI end to end, derived overlay, solve check, `keyd` site signing | Real `apt-get` and `pip` install from the vault in e2e |
| M4 | `mer-auth`, `mer-http`: listeners and origin split, tokens, RBAC, gate, audit checkpoints and mandatory anchors, cross-anchoring, metrics, worker sandboxing | Access test matrix; audit verify; vault refuses to start without an external sink |
| M5 | Dashboard, traffic stats, miss recording, name catalog, report approval workflow, wish ingest | Wishlist loop closes end to end |
| M6 | npm, OCI, quotas and retention policies in the UI, Policy extras, GC tuning, S3 backend hardening | `npm install` and `docker pull` in e2e |

Post-MVP, with wire fields already reserved: per-bundle AEAD (`Descriptor.enc`), parity volumes (`vol_kind = 1`), file-drop transport, `DeltaBase::Report` inventories, soak approval mode, extra publish channels, grandfather-father-son retention, collector blob eviction, Postgres, PKCS#11 signer, PEP 740 and cosign evidence, soft-miss heuristic, `accel`/`fastpath`/presign transfer modes, `--in-place` media import.

## 18. Review dispositions and open items

Every finding of the three reviews is applied in the sections above, with these exceptions and adaptations:

| Finding (sub-item) | Disposition |
|---|---|
| Apply deltas by materialising the target at `base_seq` and pinning base snapshots in retention | Problem accepted; fix replaced by chained per-step deltas (sections 4.5, 5.4, 7.3), which are O(delta) and need no base snapshot on the vault |
| Derived items as `local = 1` rows in `snapshot_entry` | Rejected: a later import snapshot would inherit stale indices and old snapshots could not be reindexed. The `derived` overlay is interval-encoded per generation instead, which keeps the zero-rows-for-unchanged property |
| Records as a `record` table with a nullable `record_digest` column | Superseded by inline blobs (`blob.loc = 1`), which give seal, diff, GC, quota and root coverage with no special case |
| Serve pool and by-hash from the rolled-back-from snapshot during the grace window | Rejected: it would serve content the rollback was meant to withdraw (critical security finding). Grace fallbacks are disabled after rollback |
| Move misses to `stats.db` | Rejected: the wishlist needs them durable and `stats.db` is disposable. They stay in `meta.db` but are flushed in batches, which removes the write churn |
| One transaction writes all snapshots and `imported_seq` | Adapted: `seal_group` is kept, but the channel position is decoupled from repo application so one repo cannot stall the others; per-repo apply is idempotent |
| Fix `adopt_file` aliasing | The function is removed (no caller); the ownership rule is recorded for any future reintroduction |
| Defer link HMAC, root-threshold verification and audit cross-anchoring to post-MVP | Rejected for HMAC and threshold verification (both are small and required by security findings). Cross-anchoring is deferred only until the audit subsystem lands in M4, then mandatory |
| Skip the seal-time re-hash and hash during the first export read | Adapted: the Merkle root must exist before signing, so the seal pass is the single read; it computes leaves and verifies digests together. The sender repair cache removes repeat reads |
| `small.db` keeps reserve and mark-present transactional | Applied with invariant I7, because WAL commits across attached files are not atomic |

Open items to settle at workspace init: exact versions of `rusqlite`, `object_store`, `roaring`, `rustix`, `raptorq` (per-block API names); `pgp` versus `sequoia-openpgp` after a spike; `pep440_rs`/`pep508_rs` versus the uv forks; `nodejs-semver` name; whether `recvmmsg`/`sendmmsg` batching is needed to meet the diode bench (the MVP uses plain socket calls on a dedicated thread); the inline threshold (8 KiB) and catalog encoding sizes against real repos.
