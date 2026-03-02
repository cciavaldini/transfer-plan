//! Shared transfer utilities and constants.

pub mod helpers;
pub mod progress;
pub mod copy;
pub mod verify;
pub mod format;

pub use copy::copy_file_optimized;
pub use format::format_size;

// Shared constants
pub(crate) const SMALL_BUFFER: usize = 64 * 1024;
pub(crate) const MEDIUM_BUFFER: usize = 512 * 1024;
pub(crate) const LARGE_BUFFER: usize = 2 * 1024 * 1024;
pub(crate) const XLARGE_BUFFER: usize = 8 * 1024 * 1024;

pub(crate) const COPY_FILE_RANGE_THRESHOLD: u64 = 4 * 1024 * 1024;
pub(crate) const COPY_FILE_RANGE_CHUNK_SIZE: usize = 128 * 1024 * 1024;

pub(crate) const VERIFICATION_SIZE_LIMIT: u64 = 10 * 1024 * 1024;

pub(crate) const PROGRESS_UPDATE_BYTES: u64 = 2 * 1024 * 1024;
pub(crate) const PROGRESS_UPDATE_INTERVAL_MS: u64 = 250;
pub(crate) const PREPARED_PARENT_DIR_CACHE_LIMIT: usize = 8192;

pub(crate) const MAX_RETRIES: usize = 3;
pub(crate) const RETRY_BASE_DELAY_MS: u64 = 1000;
pub(crate) const ZERO_COPY_UNAVAILABLE: &str = "zero-copy unavailable";
