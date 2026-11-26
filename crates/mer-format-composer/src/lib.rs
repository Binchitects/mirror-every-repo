//! Composer repositories (packagist.org), for PHP.
//!
//! The collector reads each package's metadata (`p2/<vendor>/<name>.json`,
//! expanded from packagist's minified form), picks versions for the PHP
//! inside the air gap, follows `require`, and downloads each version's zip.
//! The vault serves `packages.json` and metadata of exactly what is
//! mirrored, pointing at its own copies of the archives, each with the
//! SHA-1 Composer checks.
mod sync;
#[cfg(test)]
mod tests;
pub mod version;

pub use sync::{expand, sync, valid_name, ComposerConfig, ComposerError, ComposerReport, BASE_PLACEHOLDER};
