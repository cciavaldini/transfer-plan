use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferItem {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub size: u64,
    #[serde(default)]
    pub cleanup_root: Option<PathBuf>,
}

/// Queue control commands sent during transfer
#[derive(Debug, Clone)]
pub enum QueueCommand {
    /// Stop all transfers immediately
    Stop,
    /// Internal signal used to end monitor tasks.
    Terminate,
}

pub struct TransferQueue {
    items: Arc<RwLock<VecDeque<TransferItem>>>,
    total_size: Arc<AtomicU64>,
    cmd_tx: mpsc::UnboundedSender<QueueCommand>,
    cmd_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<QueueCommand>>>,
}

impl TransferQueue {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        Self {
            items: Arc::new(RwLock::new(VecDeque::new())),
            total_size: Arc::new(AtomicU64::new(0)),
            cmd_tx,
            cmd_rx: Arc::new(tokio::sync::Mutex::new(cmd_rx)),
        }
    }

    pub fn get_sender(&self) -> mpsc::UnboundedSender<QueueCommand> {
        self.cmd_tx.clone()
    }

    pub async fn try_recv_command(&self) -> Option<QueueCommand> {
        self.cmd_rx.lock().await.try_recv().ok()
    }

    pub fn add_file(&self, source: PathBuf, dest_root: &Path) -> Result<()> {
        let metadata = fs::metadata(&source)
            .with_context(|| format!("Failed to read metadata for {:?}", source))?;
        let file_name = source.file_name().context("Invalid file name")?;
        let destination = dest_root.join(file_name);

        let size = metadata.len();
        let item = TransferItem {
            source,
            destination,
            size,
            cleanup_root: None,
        };

        // Update total size atomically
        self.total_size.fetch_add(size, Ordering::Relaxed);

        // Add to queue - no clone needed!
        self.items.write().unwrap().push_back(item);
        Ok(())
    }

    pub fn add_directory(&self, source: &Path, dest_root: &Path) -> Result<()> {
        let dir_name = source.file_name().context("Invalid directory name")?;
        let dest_dir = dest_root.join(dir_name);

        // Batch collection - lock once instead of N times
        let mut batch = Vec::new();
        let mut batch_size = 0u64;

        for entry in walkdir::WalkDir::new(source) {
            let entry = entry?;
            if entry.file_type().is_file() {
                let relative_path = entry.path().strip_prefix(source)?;
                let destination = dest_dir.join(relative_path);
                let size = entry.metadata()?.len();

                let item = TransferItem {
                    source: entry.path().to_path_buf(),
                    destination,
                    size,
                    cleanup_root: Some(source.to_path_buf()),
                };

                batch_size += size;
                batch.push(item);
            }
        }

        // Keep directory transfers deterministic: alphabetical by source path
        batch.sort_unstable_by(|a, b| a.source.cmp(&b.source));

        // Single lock for entire batch
        self.total_size.fetch_add(batch_size, Ordering::Relaxed);
        self.items.write().unwrap().extend(batch);

        Ok(())
    }

    /// Snapshot current pending items in queue order.
    pub fn snapshot_items(&self) -> Vec<TransferItem> {
        self.items.read().unwrap().iter().cloned().collect()
    }

    /// Replace queue content (used to restore an interrupted session).
    pub fn restore_items(&self, items: Vec<TransferItem>) {
        let total_size = items.iter().map(|item| item.size).sum::<u64>();
        let mut guard = self.items.write().unwrap();
        *guard = items.into_iter().collect();
        self.total_size.store(total_size, Ordering::Relaxed);
    }

    #[inline]
    pub fn pop(&self) -> Option<TransferItem> {
        let item = self.items.write().unwrap().pop_front()?;
        self.total_size.fetch_sub(item.size, Ordering::Relaxed);
        Some(item)
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.items.read().unwrap().len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.items.read().unwrap().is_empty()
    }

    #[inline]
    pub fn total_size(&self) -> u64 {
        // O(1) instead of O(n) - cached!
        self.total_size.load(Ordering::Relaxed)
    }
}

impl Default for TransferQueue {
    fn default() -> Self {
        Self::new()
    }
}
