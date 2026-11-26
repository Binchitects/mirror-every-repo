//! Maven repositories (Java, Kotlin, Scala: Maven Central and compatible).
//!
//! Two ways to say what to mirror. Exact: the artifacts a local repository
//! (`~/.m2/repository`) holds after a build on a connected machine, which
//! covers build plugins too. Resolved: coordinates whose POMs are followed
//! (parents, properties, dependency management, imported BOMs, scopes,
//! optional dependencies, exclusions, ranges, relocations). Every file is
//! checked against the strongest checksum upstream serves, and the vault
//! repository gets version metadata and `.sha1` files for what it holds.
mod pom;
mod sync;
#[cfg(test)]
mod tests;
mod version;

pub use pom::{parse_pom, Pom};
pub use sync::{file_path, sync, MavenConfig, MavenError, MavenReport};
pub use version::{compare, Range};
