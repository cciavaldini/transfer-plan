//! Library root for the transfer-plan crate. Provides core modules used by
//! both the binary and any potential downstream consumers.

pub mod queue;
pub mod transfer;
pub mod worker;

// Re-export commonly used std types so that downstream modules can import from
// the crate root.  In particular we need `Command` in various unmount helpers.
pub use std::process::Command;
