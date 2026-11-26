//! Go modules.
//!
//! The collector reads modules from a Go module proxy (proxy.golang.org by
//! default), follows `go.mod` requirements the way minimal version selection
//! does, or takes exactly what a `go.sum` lists, checks every `.zip` and
//! `.mod` against its `h1:` hash, and writes a static module proxy tree of
//! that subset into one snapshot. The vault serves it as `GOPROXY`.
mod hash;
mod modfile;
mod sync;
#[cfg(test)]
mod tests;
mod version;

pub use hash::{gomod_hash, zip_hash};
pub use modfile::{escape, parse_gomod, parse_gosum, unescape, valid_path, GoMod};
pub use sync::{sync, GoConfig, GoError, GoReport, Mismatch};
pub use version::{compare, is_canonical, is_valid};
