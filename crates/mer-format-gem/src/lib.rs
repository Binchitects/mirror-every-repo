//! RubyGems sources, for `gem` and `bundle`.
//!
//! The collector reads the compact index (`/info/<gem>`) of a gem source,
//! picks versions for the configured Ruby and platforms, follows their
//! dependencies, and checks every `.gem` against the SHA-256 the index
//! states. The vault serves a compact index of exactly what is mirrored
//! (`/versions` and `/info/<gem>`, upstream's lines unchanged), the `.gem`
//! files, and the specifications `gem install` reads
//! (`quick/Marshal.4.8/*.gemspec.rz`). Both `gem install` and Bundler use
//! the compact index once `/versions` answers, so the older Marshal indexes
//! (`specs.4.8.gz`) are not needed.
mod info;
mod sync;
#[cfg(test)]
mod tests;
pub mod version;

pub use info::{parse_info, parse_lockfile, valid_name, Line};
pub use sync::{sync, GemConfig, GemError, GemReport};
