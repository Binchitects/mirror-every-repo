//! Debian/Ubuntu (apt) repositories.
//!
//! The collector verifies the upstream archive signature, selects the wanted
//! packages plus their dependencies, downloads them against the signed hashes,
//! and writes a complete, self-consistent apt repository of that subset into
//! one snapshot, signed with the site key. The vault serves those files as
//! they are.
mod closure;
mod control;
mod gpg;
mod release;
mod sync;
#[cfg(test)]
mod tests;
mod version;

pub use closure::{parse_depends, Closure, Index};
pub use control::{parse_stanzas, Stanza};
pub use gpg::{verify_inrelease, SiteKey};
pub use release::{parse_release, Release, ReleaseFile};
pub use sync::{sync, AptConfig, SyncReport};
pub use version::compare_versions;

#[derive(Debug, thiserror::Error)]
pub enum AptError {
    #[error(transparent)]
    Fetch(#[from] mer_fetch::FetchError),
    #[error(transparent)]
    Storage(#[from] mer_storage::StorageError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("upstream signature: {0}")]
    Signature(String),
    #[error("upstream metadata: {0}")]
    Metadata(String),
    #[error("gpg: {0}")]
    Gpg(String),
    #[error("package {0} is not in the upstream index")]
    UnknownPackage(String),
}

impl From<mer_meta::MetaError> for AptError {
    fn from(e: mer_meta::MetaError) -> Self {
        AptError::Storage(e.into())
    }
}

impl From<mer_blob::StoreError> for AptError {
    fn from(e: mer_blob::StoreError) -> Self {
        AptError::Storage(e.into())
    }
}
