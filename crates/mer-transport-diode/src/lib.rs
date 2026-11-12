//! Data diode transport: strictly one-way UDP. The sender never hears back,
//! so it repeats itself in passes and adds error-correction symbols; the
//! receiver rebuilds each segment from whatever subset arrives.
//!
//! Every packet carries a MAC under a shared link key. The receiver does no
//! work, and allocates nothing, for a packet that fails it.
mod packet;
mod receiver;
mod sender;
#[cfg(test)]
mod tests;

pub use receiver::{Receiver, ReceiverConfig, RxEvent, RxStats};
pub use sender::{PacketOut, PassStats, Sender, SenderConfig, VecOut};

#[derive(Debug, thiserror::Error)]
pub enum DiodeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Format(#[from] mer_bundle_format::FormatError),
    #[error(transparent)]
    Fec(#[from] mer_fec::FecError),
    #[error("descriptor needs {0} fragments, the limit is 48; use a larger symbol size")]
    DescriptorTooLarge(usize),
}
