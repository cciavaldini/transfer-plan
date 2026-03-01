use crate::queue::{QueueCommand, TransferQueue};
use crate::transfer::format_size;
use indicatif::ProgressBar;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::progress_ui::format_eta;
use super::sync::{estimate_pending_sync_secs, smooth_speed};
use super::types::{MonitorOutcome, TransferStats};

const MONITOR_INTERVAL_MS: u64 = 1500;
const QUEUE_SETTLE_MS: u64 = 500;

pub(crate) async fn run_progress_monitor(
    queue: Arc<TransferQueue>,
    overall_pb: ProgressBar,
    stats: Arc<TransferStats>,
    io_bytes: Arc<AtomicU64>,
    stop_requested: Arc<AtomicBool>,
    total_size: u64,
    initial_total_files: u64,
) -> MonitorOutcome {
    let mut last_sample_time = Instant::now();
    let mut last_io_bytes = 0u64;
    let mut smoothed_speed_bps = 0.0;
    let mut max_total_files = initial_total_files;
    let mut stopped_by_user = false;

    loop {
        // Update overall progress
        let current_io_bytes = io_bytes.load(Ordering::Relaxed);
        let transferred_bytes = current_io_bytes.min(total_size);
        let (completed, failed, _) = stats.get_stats();
        overall_pb.set_position(transferred_bytes);

        let now = Instant::now();
        let sample_secs = now.duration_since(last_sample_time).as_secs_f64();
        if sample_secs > 0.0 {
            let delta = current_io_bytes.saturating_sub(last_io_bytes) as f64;
            let current_speed_bps = delta / sample_secs;
            smoothed_speed_bps = smooth_speed(smoothed_speed_bps, current_speed_bps);
            last_sample_time = now;
            last_io_bytes = current_io_bytes;
        }

        let discovered_total = completed + failed + queue.len() as u64;
        if discovered_total > max_total_files {
            max_total_files = discovered_total;
        }

        let elapsed_secs = stats.start_time.elapsed().as_secs_f64();
        let pending_sync_secs = estimate_pending_sync_secs(smoothed_speed_bps).unwrap_or(0.0);
        let avg_window_secs = elapsed_secs + pending_sync_secs;
        let throughput_avg = if avg_window_secs > 0.0 {
            (transferred_bytes as f64 / avg_window_secs) / 1_000_000.0
        } else {
            0.0
        };

        let remaining_bytes = total_size.saturating_sub(transferred_bytes);
        let remaining_files = max_total_files.saturating_sub(completed + failed);
        let remaining_transfer_secs = if smoothed_speed_bps > 0.0 {
            Some(remaining_bytes as f64 / smoothed_speed_bps)
        } else {
            None
        };
        let total_remaining = remaining_transfer_secs
            .map(|secs| format_eta((secs + pending_sync_secs).ceil() as u64))
            .unwrap_or_else(|| "estimating...".to_string());
        overall_pb.set_message(format!(
            "Files: {}/{} | Remaining: {} ({} files) | Total remaining: {} | Avg: {:.1} MB/s",
            completed,
            max_total_files,
            format_size(remaining_bytes),
            remaining_files,
            total_remaining,
            throughput_avg,
        ));

        // Check for commands
        if let Some(cmd) = queue.try_recv_command() {
            match cmd {
                QueueCommand::Stop => {
                    stopped_by_user = true;
                    break;
                }
                QueueCommand::Terminate => {
                    break;
                }
            }
        }

        if stop_requested.load(Ordering::Relaxed) {
            // Stop request is active; workers will finish current file and exit.
            overall_pb.set_message("Stopping after current file(s)...");
        }

        if queue.is_empty() && completed + failed > 0 {
            tokio::time::sleep(Duration::from_millis(QUEUE_SETTLE_MS)).await;
            if queue.is_empty() {
                break;
            }
        }

        tokio::time::sleep(Duration::from_millis(MONITOR_INTERVAL_MS)).await;
    }

    MonitorOutcome {
        stopped_by_user,
        smoothed_speed_bps,
    }
}
