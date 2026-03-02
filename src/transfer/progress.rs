//! Progress-tracking reader wrapper and helpers used during file transfer.

use std::io::{self, Read};
use std::time::{Duration, Instant};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use indicatif::ProgressBar;

use crate::transfer::{PROGRESS_UPDATE_BYTES, PROGRESS_UPDATE_INTERVAL_MS};

pub struct ProgressReader<R> {
    pub inner: R,
    pub progress: ProgressBar,
    pub overall_progress: ProgressBar,
    pub copied_bytes: Arc<AtomicU64>,
    pub pending_bytes: u64,
    pub last_flush: Instant,
}

impl<R> ProgressReader<R> {
    pub fn flush_pending(&mut self) {
        if self.pending_bytes == 0 {
            return;
        }

        let bytes = self.pending_bytes;
        self.pending_bytes = 0;
        self.last_flush = Instant::now();
        self.copied_bytes.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        self.progress.inc(bytes);
        self.overall_progress.inc(bytes);
    }
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 {
            self.flush_pending();
            return Ok(0);
        }

        self.pending_bytes += n as u64;
        if self.pending_bytes >= PROGRESS_UPDATE_BYTES
            || self.last_flush.elapsed() >= Duration::from_millis(PROGRESS_UPDATE_INTERVAL_MS)
        {
            self.flush_pending();
        }

        Ok(n)
    }
}

pub fn finish_with_overall_sync(
    progress: &ProgressBar,
    overall_progress: &ProgressBar,
    copied_bytes: &Arc<AtomicU64>,
) {
    if let Some(total) = progress.length() {
        let current = progress.position();
        let remaining = total.saturating_sub(current);
        if remaining > 0 {
            copied_bytes.fetch_add(remaining, std::sync::atomic::Ordering::Relaxed);
            overall_progress.inc(remaining);
            progress.inc(remaining);
        }
    }
    progress.finish();
}
