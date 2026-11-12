//! npm registries.
//!
//! The collector reads package documents from the upstream registry, resolves
//! the wanted packages and their dependency ranges to concrete versions,
//! downloads each tarball against the integrity hash the registry states, and
//! writes package documents that list only the mirrored versions.
mod semver;
mod sync;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod truth;

pub use semver::{Range, Version};
pub use sync::{sync, valid_name, NpmConfig, NpmError, NpmReport, BASE_PLACEHOLDER};
