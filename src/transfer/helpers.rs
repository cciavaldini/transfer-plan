use once_cell::sync::Lazy;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use anyhow::{Result, Context};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::transfer::{PREPARED_PARENT_DIR_CACHE_LIMIT};

static BUFFER_POOL: Lazy<Mutex<Vec<Vec<u8>>>> = Lazy::new(|| Mutex::new(Vec::new()));
static PREPARED_PARENT_DIRS: Lazy<Mutex<HashSet<PathBuf>>> =
    Lazy::new(|| Mutex::new(HashSet::new()));

pub fn get_buffer(size: usize) -> Vec<u8> {
    BUFFER_POOL
        .lock()
        .pop()
        .filter(|b| b.len() == size)
        .unwrap_or_else(|| vec![0u8; size])
}

pub fn return_buffer(buffer: Vec<u8>) {
    let mut pool = BUFFER_POOL.lock();
    if pool.len() < 10 {
        pool.push(buffer);
    }
}

pub fn atomic_saturating_sub(counter: &AtomicU64, value: u64) {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let updated = current.saturating_sub(value);
        match counter.compare_exchange_weak(current, updated, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
}

pub fn rollback_file_attempt_progress(
    progress: &indicatif::ProgressBar,
    copied_bytes: &std::sync::Arc<AtomicU64>,
    overall_progress: &indicatif::ProgressBar,
) {
    let attempt_bytes = progress.position();
    if attempt_bytes == 0 {
        return;
    }

    atomic_saturating_sub(copied_bytes.as_ref(), attempt_bytes);
    overall_progress.set_position(copied_bytes.load(Ordering::Relaxed));
    progress.set_position(0);
}

pub fn ensure_parent_directory(destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        {
            let cache = PREPARED_PARENT_DIRS.lock();
            if cache.contains(parent) {
                return Ok(());
            }
        }

        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {:?}", parent))?;

        let mut cache = PREPARED_PARENT_DIRS.lock();
        if cache.len() >= PREPARED_PARENT_DIR_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(parent.to_path_buf());
    }
    Ok(())
}

pub fn temp_destination_path(destination: &Path) -> Result<PathBuf> {
    let parent = destination
        .parent()
        .context("Destination has no parent directory")?;
    let file_name = destination
        .file_name()
        .context("Destination has no file name")?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(parent.join(format!(".{}.part.{}", file_name, nonce)))
}

pub fn finalize_atomic_destination(temp_destination: &Path, destination: &Path) -> Result<()> {
    match fs::rename(temp_destination, destination) {
        Ok(()) => Ok(()),
        Err(initial_err) => {
            if destination.exists() {
                fs::remove_file(destination).with_context(|| {
                    format!("Failed to replace existing destination file {:?}", destination)
                })?;
                fs::rename(temp_destination, destination).with_context(|| {
                    format!("Failed to atomically move {:?} to {:?}", temp_destination, destination)
                })?;
                Ok(())
            } else {
                Err(initial_err).with_context(|| {
                    format!("Failed to atomically move {:?} to {:?}", temp_destination, destination)
                })
            }
        }
    }
}
