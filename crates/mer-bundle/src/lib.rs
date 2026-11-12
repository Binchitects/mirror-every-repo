//! Bundles: sealed on the collector, assembled and imported on the vault.
mod collect;
#[cfg(test)]
mod tests;
mod vault;

pub use collect::*;
pub use vault::*;

use mer_bundle_format::FormatError;
use mer_meta::MetaError;
use mer_storage::StorageError;

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Meta(#[from] MetaError),
    #[error(transparent)]
    Format(#[from] FormatError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("bundle rejected: {0}")]
    Rejected(String),
}

impl From<mer_blob::StoreError> for BundleError {
    fn from(e: mer_blob::StoreError) -> Self {
        BundleError::Storage(e.into())
    }
}

pub type Result<T> = std::result::Result<T, BundleError>;
