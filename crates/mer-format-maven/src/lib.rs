//! Maven repositories (the layout Maven Central serves).
//!
//! The collector reads POMs the way Maven does (parents, properties,
//! dependency management, imported BOMs), follows compile and runtime
//! dependencies, and mirrors every POM that was read, every artifact that
//! was reached, their checksum files, and a `maven-metadata.xml` per
//! artifact that lists what is mirrored.
//!
//! Maven settles on one version where a dependency is reached at several;
//! which one depends on the project that is being built. Here every version
//! that is reached is mirrored, so whichever Maven settles on is there.
mod pom;
mod sync;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod truth;
pub mod version;

pub use pom::{parse_pom, Gav};
pub use sync::{sync, MavenConfig, MavenError, MavenReport};
