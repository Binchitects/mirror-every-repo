//! Metadata database (SQLite). All SQL lives in this crate.
mod access;
mod channel;
mod history;
mod snapshot;
#[cfg(test)]
mod tests;

pub use access::*;
pub use channel::*;
pub use history::*;
pub use snapshot::*;

use mer_core::{CoreError, Digest, SnapshotId};
use rusqlite::{Connection, TransactionBehavior};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA_VERSION: i64 = 3;
const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("schema.sql")), (2, include_str!("migration_2.sql")), (3, include_str!("migration_3.sql"))];

#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("blob is being deleted, retry: {0}")]
    Busy(Digest),
    #[error("database corrupt: {0}")]
    Corrupt(&'static str),
    #[error(transparent)]
    Core(#[from] CoreError),
}

pub type Result<T> = std::result::Result<T, MetaError>;

pub struct Meta {
    conn: Mutex<Connection>,
}

/// A transaction. Every typed operation is a method on this.
pub struct Tx<'a> {
    pub(crate) c: &'a Connection,
}

impl Meta {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(MetaError::Invalid(format!(
                "database schema version {version} is newer than this build understands ({SCHEMA_VERSION}); upgrade mer"
            )));
        }
        // Each migration runs in its own transaction together with the version
        // bump, so an interrupted upgrade is simply repeated.
        for (target, sql) in MIGRATIONS {
            if version < *target {
                conn.execute_batch(&format!("BEGIN;\n{sql}\nPRAGMA user_version = {target};\nCOMMIT;"))?;
            }
        }
        Ok(Meta { conn: Mutex::new(conn) })
    }

    /// Run `f` in one write transaction. Any error rolls everything back.
    pub fn write<T>(&self, f: impl FnOnce(&Tx) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let t = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = f(&Tx { c: &t })?;
        t.commit()?;
        Ok(out)
    }

    /// Run `f` against one consistent view. Nothing is committed.
    pub fn read<T>(&self, f: impl FnOnce(&Tx) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let t = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        f(&Tx { c: &t })
    }
}

pub(crate) fn dig(v: Vec<u8>) -> Result<Digest> {
    Digest::from_slice(&v).map_err(|_| MetaError::Corrupt("digest length"))
}

pub(crate) fn snap_id(v: Vec<u8>) -> Result<SnapshotId> {
    SnapshotId::from_slice(&v).map_err(|_| MetaError::Corrupt("snapshot id length"))
}
