use std::path::PathBuf;

#[derive(Clone)]
pub struct TransferMapping {
    pub source: PathBuf,
    pub destination: PathBuf,
}
