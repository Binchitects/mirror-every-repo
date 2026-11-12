//! Content-addressed blob store. One store is shared by every repo format,
//! so identical content is kept once.
mod fs;

pub use fs::FsStore;
use mer_core::Digest;
use std::io::Read;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("digest mismatch: expected {expected}, got {got}")]
    DigestMismatch { expected: Digest, got: Digest },
    #[error("size mismatch: expected {expected}, got {got}")]
    SizeMismatch { expected: u64, got: u64 },
    #[error("blob not found: {0}")]
    NotFound(Digest),
    #[error("store format: {0}")]
    Format(String),
    #[error("range out of bounds")]
    BadRange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobStat {
    pub digest: Digest,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PutOutcome {
    pub digest: Digest,
    pub size: u64,
    /// True when the content was already stored (nothing new written).
    pub existed: bool,
}

/// What the caller claims about the content being written. Verified on commit.
#[derive(Clone, Copy, Debug, Default)]
pub struct Expect {
    pub digest: Option<Digest>,
    pub size: Option<u64>,
}

impl Expect {
    pub fn exact(digest: Digest, size: u64) -> Self {
        Expect { digest: Some(digest), size: Some(size) }
    }
}

pub type BlobReader = Box<dyn Read + Send>;

pub trait BlobStore: Send + Sync + 'static {
    /// Stream content in. It becomes visible atomically, only after its hash
    /// has been verified and its bytes are durable.
    fn put(&self, src: &mut dyn Read, expect: Expect) -> Result<PutOutcome, StoreError>;
    fn stat(&self, d: &Digest) -> Result<Option<BlobStat>, StoreError>;
    /// Open for reading. `range` is (offset, length).
    fn open(&self, d: &Digest, range: Option<(u64, u64)>) -> Result<BlobReader, StoreError>;
    /// Idempotent.
    fn delete(&self, d: &Digest) -> Result<(), StoreError>;
    fn list(&self) -> Result<Vec<BlobStat>, StoreError>;
    /// Remove leftovers of interrupted writes. Returns how many were removed.
    fn sweep_tmp(&self) -> Result<u64, StoreError>;

    fn put_bytes(&self, data: &[u8]) -> Result<PutOutcome, StoreError> {
        let mut r = data;
        self.put(&mut r, Expect::default())
    }
    fn read_all(&self, d: &Digest) -> Result<Vec<u8>, StoreError> {
        let mut out = Vec::new();
        self.open(d, None)?.read_to_end(&mut out)?;
        Ok(out)
    }
}
