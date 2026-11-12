//! RPM repositories (yum and dnf).
//!
//! The collector reads the package lists of one or more upstream
//! repositories, works out what the wanted packages need (the way an
//! installation would: by what packages provide, with versions, file
//! dependencies and boolean dependencies), downloads those packages against
//! the checksums in the package list, and writes repository metadata that
//! lists exactly what was mirrored.
//!
//! Packages are stored unchanged, so the signature each carries from its
//! distribution still verifies on the clients.
mod repodata;
mod resolve;
mod sync;
#[cfg(test)]
mod tests;
pub mod vercmp;

pub use sync::{sync, RpmConfig, RpmError, RpmReport};
