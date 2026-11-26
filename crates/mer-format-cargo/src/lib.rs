//! Cargo registries (Rust crates).
//!
//! The collector reads the sparse index of a registry (crates.io by
//! default), resolves the wanted crates and their dependencies the way Cargo
//! does (newest version that matches each requirement, optional dependencies
//! only when a feature turns them on), or takes the exact versions a
//! `Cargo.lock` names, downloads each `.crate` against the SHA-256 the index
//! states, and writes a sparse index of that subset into one snapshot.
//!
//! Version requirements are matched with the `semver` crate, the library
//! Cargo itself uses for them.
mod lock;
mod sync;
#[cfg(test)]
mod tests;

pub use lock::{parse_lockfile, Locked};
pub use sync::{index_path, sync, valid_name, CargoConfig, CargoError, CargoReport, BASE_PLACEHOLDER};
