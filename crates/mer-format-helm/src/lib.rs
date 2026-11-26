//! Helm chart repositories.
//!
//! The collector reads a chart repository's `index.yaml`, picks the wanted
//! chart versions, downloads each chart against the SHA-256 the index
//! states, and writes an index of that subset (with chart URLs relative to
//! the repository) into one snapshot. Charts carry their dependencies inside
//! the archive (`charts/`), so there is nothing further to resolve.
mod sync;
#[cfg(test)]
mod tests;

pub use sync::{sync, valid_name, HelmConfig, HelmError, HelmReport};
