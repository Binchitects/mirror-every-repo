//! Hard caps applied to untrusted input before any allocation.
pub const MAX_PATH_BYTES: usize = 1024;
pub const MAX_DESCRIPTOR_BODY: usize = 16 * 1024;
pub const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_REPOS: usize = 4096;
pub const MAX_STEPS_PER_REPO: usize = 64;
pub const MIN_SEGMENT_SIZE: u32 = 4096;
pub const MAX_SEGMENT_SIZE: u32 = 64 * 1024 * 1024;
pub const MAX_SIGNATURES: usize = 8;
/// A vault accepts bundle seq in (imported, imported + ACCEPT_WINDOW].
pub const ACCEPT_WINDOW: u64 = 64;
