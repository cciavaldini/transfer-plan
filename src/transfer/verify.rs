use anyhow::{Result, Context};
use sha2::{Digest, Sha256};
use std::io::{self, Read};
use std::fs::File;
use std::path::Path;

use crate::transfer::VERIFICATION_SIZE_LIMIT;
use crate::transfer::helpers::{get_buffer, return_buffer};

pub fn hash_file(path: &Path, file_size: u64) -> Result<Vec<u8>> {
    if file_size > VERIFICATION_SIZE_LIMIT {
        anyhow::bail!(
            "File too large for verification ({}MB > 10MB limit)",
            file_size / 1_048_576
        );
    }

    let file = File::open(path).with_context(|| format!("Failed to open file for hashing: {:?}", path))?;
    let mut reader = io::BufReader::with_capacity(1024 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buffer = get_buffer(1024 * 1024);

    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }

    return_buffer(buffer);
    Ok(hasher.finalize().to_vec())
}

pub fn verify_transfer(source: &Path, dest: &Path, file_size: u64) -> Result<bool> {
    if file_size > VERIFICATION_SIZE_LIMIT {
        return Ok(true);
    }

    let source_hash = hash_file(source, file_size)?;
    let dest_hash = hash_file(dest, file_size)?;
    Ok(source_hash == dest_hash)
}
