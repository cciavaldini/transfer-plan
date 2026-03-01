use anyhow::{Context, Result};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;

use crate::queue::{TransferItem, TransferQueue};

const QUEUE_CHECKPOINT_FILE: &str = "pending_queue.json";

#[derive(Debug, Serialize, Deserialize)]
struct QueueCheckpoint {
    items: Vec<TransferItem>,
}

pub(crate) fn clear_queue_checkpoint() -> Result<()> {
    match fs::remove_file(QUEUE_CHECKPOINT_FILE) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn save_queue_checkpoint(
    queue: &TransferQueue,
    last_saved_revision: &mut u64,
) -> Result<()> {
    let revision = queue.revision();
    if revision == *last_saved_revision {
        return Ok(());
    }

    let items = queue.snapshot_items();
    if items.is_empty() {
        clear_queue_checkpoint()?;
        *last_saved_revision = revision;
        return Ok(());
    }

    let checkpoint = QueueCheckpoint { items };
    let json = serde_json::to_vec(&checkpoint)?;
    fs::write(QUEUE_CHECKPOINT_FILE, json)
        .with_context(|| format!("Failed to write {}", QUEUE_CHECKPOINT_FILE))?;
    *last_saved_revision = revision;
    Ok(())
}

pub(crate) fn load_queue_checkpoint() -> Result<Option<Vec<TransferItem>>> {
    let path = Path::new(QUEUE_CHECKPOINT_FILE);
    if !path.exists() {
        return Ok(None);
    }

    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) => {
            eprintln!(
                "{} Failed to read {}: {}",
                "Warning:".yellow(),
                QUEUE_CHECKPOINT_FILE,
                e
            );
            return Ok(None);
        }
    };
    let checkpoint: QueueCheckpoint = match serde_json::from_str(&content) {
        Ok(data) => data,
        Err(e) => {
            eprintln!(
                "{} Invalid {} (ignored): {}",
                "Warning:".yellow(),
                QUEUE_CHECKPOINT_FILE,
                e
            );
            clear_queue_checkpoint()?;
            return Ok(None);
        }
    };

    if checkpoint.items.is_empty() {
        clear_queue_checkpoint()?;
        return Ok(None);
    }

    Ok(Some(checkpoint.items))
}
