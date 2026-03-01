use anyhow::{Context, Result};
use crossbeam::channel::{self, Receiver, RecvTimeoutError, Sender};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

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
    items_tx: Sender<TransferItem>,
    items_rx: Receiver<TransferItem>,
    snapshot_items: Arc<Mutex<VecDeque<TransferItem>>>,
    total_size: Arc<AtomicU64>,
    pending_items: Arc<AtomicUsize>,
    revision: Arc<AtomicU64>,
    cmd_tx: Sender<QueueCommand>,
    cmd_rx: Receiver<QueueCommand>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct QueueAddSummary {
    pub queued_files: usize,
    pub skipped_files: usize,
}

const UPDATE_MTIME_TOLERANCE_SECS: u64 = 2;

fn mtime_close_enough(src: SystemTime, dst: SystemTime) -> bool {
    let diff = if src >= dst {
        src.duration_since(dst)
    } else {
        dst.duration_since(src)
    };
    diff.map(|d| d.as_secs() <= UPDATE_MTIME_TOLERANCE_SECS)
        .unwrap_or(false)
}

fn destination_is_unchanged(source_meta: &fs::Metadata, destination: &Path) -> bool {
    let Ok(dest_meta) = fs::metadata(destination) else {
        return false;
    };
    if !dest_meta.is_file() || source_meta.len() != dest_meta.len() {
        return false;
    }

    match (source_meta.modified(), dest_meta.modified()) {
        (Ok(src_mtime), Ok(dst_mtime)) => mtime_close_enough(src_mtime, dst_mtime),
        _ => false,
    }
}

impl TransferQueue {
    pub fn new() -> Self {
        let (items_tx, items_rx) = channel::unbounded();
        let (cmd_tx, cmd_rx) = channel::unbounded();
        Self {
            items_tx,
            items_rx,
            snapshot_items: Arc::new(Mutex::new(VecDeque::new())),
            total_size: Arc::new(AtomicU64::new(0)),
            pending_items: Arc::new(AtomicUsize::new(0)),
            revision: Arc::new(AtomicU64::new(0)),
            cmd_tx,
            cmd_rx,
        }
    }

    pub fn get_sender(&self) -> Sender<QueueCommand> {
        self.cmd_tx.clone()
    }

    pub fn try_recv_command(&self) -> Option<QueueCommand> {
        self.cmd_rx.try_recv().ok()
    }

    fn enqueue_batch(&self, batch: Vec<TransferItem>, batch_size: u64) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        {
            let mut guard = self.snapshot_items.lock();
            guard.extend(batch.iter().cloned());
        }

        let batch_len = batch.len();
        for item in batch {
            self.items_tx
                .send(item)
                .map_err(|_| anyhow::anyhow!("Transfer queue receiver disconnected"))?;
        }

        self.total_size.fetch_add(batch_size, Ordering::Relaxed);
        self.pending_items.fetch_add(batch_len, Ordering::Release);
        self.revision.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn add_file_with_policy(
        &self,
        source: PathBuf,
        dest_root: &Path,
        update_mode: bool,
    ) -> Result<QueueAddSummary> {
        let metadata = fs::metadata(&source)
            .with_context(|| format!("Failed to read metadata for {:?}", source))?;
        let file_name = source.file_name().context("Invalid file name")?;
        let destination = dest_root.join(file_name);

        let size = metadata.len();
        if update_mode && destination_is_unchanged(&metadata, &destination) {
            return Ok(QueueAddSummary {
                skipped_files: 1,
                ..QueueAddSummary::default()
            });
        }

        let item = TransferItem {
            source,
            destination,
            size,
            cleanup_root: None,
        };
        self.enqueue_batch(vec![item], size)?;
        Ok(QueueAddSummary {
            queued_files: 1,
            ..QueueAddSummary::default()
        })
    }

    pub fn add_directory_with_policy(
        &self,
        source: &Path,
        dest_root: &Path,
        update_mode: bool,
    ) -> Result<QueueAddSummary> {
        let dir_name = source.file_name().context("Invalid directory name")?;
        let dest_dir = dest_root.join(dir_name);

        // Batch collection - one queue update for the entire directory.
        let mut batch = Vec::new();
        let mut batch_size = 0u64;
        let mut skipped_files = 0usize;
        let cleanup_root = source.to_path_buf();

        for entry in walkdir::WalkDir::new(source) {
            let entry = entry?;
            if entry.file_type().is_file() {
                let relative_path = entry.path().strip_prefix(source)?;
                let destination = dest_dir.join(relative_path);
                let metadata = entry.metadata()?;
                let size = metadata.len();

                if update_mode && destination_is_unchanged(&metadata, &destination) {
                    skipped_files += 1;
                    continue;
                }

                let item = TransferItem {
                    source: entry.path().to_path_buf(),
                    destination,
                    size,
                    cleanup_root: Some(cleanup_root.clone()),
                };

                batch_size += size;
                batch.push(item);
            }
        }

        // Keep directory transfers deterministic: alphabetical by source path
        batch.sort_unstable_by(|a, b| a.source.cmp(&b.source));

        let queued_files = batch.len();
        self.enqueue_batch(batch, batch_size)?;
        Ok(QueueAddSummary {
            queued_files,
            skipped_files,
        })
    }

    /// Snapshot current pending items in queue order.
    pub fn snapshot_items(&self) -> Vec<TransferItem> {
        self.snapshot_items.lock().iter().cloned().collect()
    }

    /// Replace queue content (used to restore an interrupted session).
    pub fn restore_items(&self, items: Vec<TransferItem>) {
        while self.items_rx.try_recv().is_ok() {}

        let total_size = items.iter().map(|item| item.size).sum::<u64>();
        let item_count = items.len();

        {
            let mut guard = self.snapshot_items.lock();
            *guard = items.iter().cloned().collect();
        }

        for item in items {
            if self.items_tx.send(item).is_err() {
                break;
            }
        }

        self.total_size.store(total_size, Ordering::Relaxed);
        self.pending_items.store(item_count, Ordering::Release);
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Option<TransferItem> {
        let item = match self.items_rx.recv_timeout(timeout) {
            Ok(item) => item,
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return None,
        };

        self.total_size.fetch_sub(item.size, Ordering::Relaxed);
        self.pending_items.fetch_sub(1, Ordering::AcqRel);
        self.revision.fetch_add(1, Ordering::Relaxed);

        let mut guard = self.snapshot_items.lock();
        let _ = guard.pop_front();

        Some(item)
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.pending_items.load(Ordering::Acquire)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn total_size(&self) -> u64 {
        // O(1) instead of O(n) - cached!
        self.total_size.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }
}

impl Default for TransferQueue {
    fn default() -> Self {
        Self::new()
    }
}
