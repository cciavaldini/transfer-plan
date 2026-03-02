//! Simple data types used by the UI, such as `TransferMapping`.

use std::path::PathBuf;

#[derive(Clone)]
pub struct TransferMapping {
    pub source: PathBuf,
    pub destination: PathBuf,
}
