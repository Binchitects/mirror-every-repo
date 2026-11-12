//! Plain data types shared by every crate. No I/O.
mod bytes;
pub mod limits;
mod model;
mod wire;

pub use bytes::*;
pub use model::*;
pub use wire::*;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CoreError {
    #[error("invalid hex")]
    BadHex,
    #[error("expected {expected} bytes, got {got}")]
    BadLength { expected: usize, got: usize },
    #[error("invalid path: {0}")]
    BadPath(&'static str),
}
