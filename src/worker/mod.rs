//! Worker-related submodules and re-exports used by `app` to start transfers.

mod cleanup;
mod monitor;
mod pool;
mod progress_ui;
mod space;
mod sync;
mod types;
mod worker_loop;

pub use pool::transfer_worker_pool;
#[allow(unused_imports)]
pub use types::TransferOutcome;
