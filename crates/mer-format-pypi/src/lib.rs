//! Python package indexes (PyPI and compatible).
//!
//! The collector reads the upstream Simple API (PEP 691 JSON), picks one
//! version per wanted project that fits the configured Python targets,
//! follows `Requires-Dist` to the dependencies, downloads the files against
//! the hashes the index states, and writes a static Simple index (PEP 503)
//! of that subset into one snapshot.
mod requirement;
#[cfg(test)]
mod truth;
mod version;

pub use requirement::{evaluate, normalize_name, parse_requirement, valid_name, Env, Requirement};
pub use version::{Specifier, SpecifierSet, Version};
