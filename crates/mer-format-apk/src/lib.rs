//! Alpine Linux repositories (apk).
//!
//! The collector reads each repository's `APKINDEX.tar.gz`, checks its
//! signature against Alpine's keys, follows what the wanted packages need,
//! and downloads each package against the index. The index itself is kept
//! unchanged, still signed by Alpine, so stock clients check everything with
//! the keys they already have and need no key from this site. Packages that
//! are not mirrored are still listed in it; asking the vault for one is a
//! plain 404, and lands on the wishlist.
mod index;
mod resolve;
mod sync;
#[cfg(test)]
mod tests;
pub mod version;

pub use index::{bare, check_package, parse_index, read_index, rsa_key_from_pem, valid_name, Entry, Keys};
pub use resolve::{resolve, Pool, Selection};
pub use sync::{load_keys, sync, ApkConfig, ApkError, ApkReport};
