//! Shared types used across worker modules, such as transfer statistics and
//! context structs.

use crate::queue::TransferQueue;
use indicatif::{MultiProgress, ProgressBar};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct TransferOutcome {
    pub stopped_by_user: bool,
    pub files_completed: u64,
    pub files_failed: u64,
    pub stop_reason: Option<String>,
}

pub(crate) struct MonitorOutcome {
    pub(crate) stopped_by_user: bool,
    pub(crate) smoothed_speed_bps: f64,
}

pub(crate) struct TransferStats {
    bytes_transferred: AtomicU64,
    files_completed: AtomicU64,
    files_failed: AtomicU64,
    pub(crate) start_time: Instant,
}

#[derive(Clone)]
pub(crate) struct WorkerContext {
    pub(crate) queue: Arc<TransferQueue>,
    pub(crate) multi_progress: MultiProgress,
    pub(crate) overall_pb: ProgressBar,
    pub(crate) stop_requested: Arc<AtomicBool>,
    pub(crate) stop_reason: Arc<Mutex<Option<String>>>,
    pub(crate) stats: Arc<TransferStats>,
    pub(crate) io_bytes: Arc<AtomicU64>,
    pub(crate) sync_targets: Arc<Mutex<Vec<std::path::PathBuf>>>,
    pub(crate) space_reservations: Arc<Mutex<HashMap<u64, u64>>>,
    pub(crate) cleanup_roots: Arc<Mutex<HashSet<std::path::PathBuf>>>,
    pub(crate) verify: bool,
    pub(crate) cleanup_mode: Arc<str>,
}

impl TransferStats {
    pub(crate) fn new() -> Self {
        Self {
            bytes_transferred: AtomicU64::new(0),
            files_completed: AtomicU64::new(0),
            files_failed: AtomicU64::new(0),
            start_time: Instant::now(),
        }
    }

    pub(crate) fn add_bytes(&self, bytes: u64) {
        self.bytes_transferred.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(crate) fn inc_completed(&self) {
        self.files_completed.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn inc_failed(&self) {
        self.files_failed.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn get_stats(&self) -> (u64, u64, u64) {
        (
            self.files_completed.load(Ordering::Relaxed),
            self.files_failed.load(Ordering::Relaxed),
            self.bytes_transferred.load(Ordering::Relaxed),
        )
    }
}

impl Default for TransferStats {
    fn default() -> Self {
        Self::new()
    }
}
