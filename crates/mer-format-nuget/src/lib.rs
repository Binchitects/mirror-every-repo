//! NuGet feeds (.NET packages).
//!
//! The collector reads a V3 feed, resolves the wanted packages and their
//! dependencies for the configured target frameworks the way `dotnet restore`
//! does (lowest version that satisfies each range), downloads each package
//! against the hash the feed's catalog states, and writes a static V3 feed of
//! that subset into one snapshot.
mod framework;
mod sync;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod truth;
mod version;

pub use framework::Framework;
pub use sync::{sync, valid_id, NugetConfig, NugetError, NugetReport, BASE_PLACEHOLDER};
pub use version::{Range, Version};
