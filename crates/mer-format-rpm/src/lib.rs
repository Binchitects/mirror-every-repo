//! RPM repositories (yum and dnf: RHEL, Rocky, AlmaLinux, Fedora, openSUSE).
//!
//! The collector reads `repomd.xml` (checked against the distribution key
//! when one is given), `primary.xml` and `filelists.xml` of one or more
//! repositories, follows the requirements of the wanted packages, downloads
//! each package against the SHA-256 in `primary.xml`, and writes a
//! repository of that subset whose package entries are upstream's own.
mod meta;
mod sync;
#[cfg(test)]
mod tests;
mod version;

pub use meta::{parse_primary, parse_repomd, parse_rich, Package, Rich};
pub use sync::{sync, RpmConfig, RpmError, RpmReport};
pub use version::{vercmp, Evr};
