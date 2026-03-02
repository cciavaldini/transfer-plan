use crate::queue::{QueueCommand, TransferQueue};
use anyhow::{Context, Result};
use colored::Colorize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::cleanup::cleanup_empty_roots;
use super::monitor::run_progress_monitor;
use super::progress_ui::format_eta;
use super::sync::{estimated_flush_speed_bps, run_post_transfer_sync};
use super::types::{TransferOutcome, TransferStats, WorkerContext};
use super::worker_loop::transfer_worker_single;

/// Main transfer worker pool with parallel processing
pub async fn transfer_worker_pool(
    queue: Arc<TransferQueue>,
    multi_progress: MultiProgress,
    stop_requested: Arc<AtomicBool>,
    num_workers: usize,
    verify: bool,
    cleanup_mode: String,
) -> Result<TransferOutcome> {
    tracing::info!(
        num_workers,
        verify,
        cleanup_mode = %cleanup_mode,
        queue_size = queue.len(),
        "Starting transfer worker pool"
    );
    let cleanup_delete_mode = cleanup_mode == "delete";
    let stats = Arc::new(TransferStats::new());
    let io_bytes = Arc::new(AtomicU64::new(0));
    let sync_targets = Arc::new(Mutex::new(Vec::new()));
    let space_reservations = Arc::new(Mutex::new(HashMap::new()));
    let cleanup_roots = Arc::new(Mutex::new(HashSet::new()));
    let stop_reason = Arc::new(Mutex::new(None::<String>));

    let overall_pb = multi_progress.add(ProgressBar::new(0));
    overall_pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("#>-"),
    );

    let total_size = queue.total_size();
    let initial_total_files = queue.len() as u64;
    overall_pb.set_length(total_size);
    overall_pb.set_message("Transferring...");

    let worker_ctx = WorkerContext {
        queue: queue.clone(),
        multi_progress: multi_progress.clone(),
        overall_pb: overall_pb.clone(),
        stop_requested: stop_requested.clone(),
        stop_reason: stop_reason.clone(),
        stats: stats.clone(),
        io_bytes: io_bytes.clone(),
        sync_targets: sync_targets.clone(),
        space_reservations: space_reservations.clone(),
        cleanup_roots: cleanup_roots.clone(),
        verify,
        cleanup_mode: Arc::<str>::from(cleanup_mode),
    };

    // Spawn blocking worker pool threads.
    let mut handles = vec![];
    for worker_id in 0..num_workers {
        let worker_ctx = worker_ctx.clone();
        let handle = thread::Builder::new()
            .name(format!("transfer-worker-{}", worker_id))
            .spawn(move || transfer_worker_single(worker_id, worker_ctx))
            .with_context(|| format!("Failed to spawn worker thread {}", worker_id))?;
        handles.push(handle);
    }

    // Monitor progress and handle commands
    let monitor_handle = tokio::spawn(run_progress_monitor(
        queue.clone(),
        overall_pb.clone(),
        stats.clone(),
        io_bytes.clone(),
        stop_requested.clone(),
        total_size,
        initial_total_files,
    ));

    // Wait for all workers to complete
    for handle in handles {
        if handle.join().is_err() {
            anyhow::bail!("A transfer worker thread panicked");
        }
    }

    if cleanup_delete_mode {
        cleanup_empty_roots(&cleanup_roots);
    }

    // Stop monitor
    let _ = queue.get_sender().send(QueueCommand::Terminate);
    let monitor_outcome = monitor_handle.await?;
    let stopped_from_monitor = monitor_outcome.stopped_by_user;

    let (completed, failed, transferred_bytes) = stats.get_stats();
    let transfer_elapsed = stats.start_time.elapsed();
    let sync_paths: Vec<std::path::PathBuf> = {
        let guard = sync_targets.lock().unwrap_or_else(|e| e.into_inner());
        guard.clone()
    };
    let mut sync_duration = Duration::from_secs(0);
    if completed + failed > 0 {
        sync_duration = run_post_transfer_sync(
            &overall_pb,
            &sync_paths,
            estimated_flush_speed_bps(monitor_outcome.smoothed_speed_bps),
        );
    }

    // Explicit total = transfer duration + post-transfer sync duration.
    let total_elapsed = transfer_elapsed + sync_duration;
    let throughput = if total_elapsed.as_secs_f64() > 0.0 {
        (transferred_bytes as f64 / total_elapsed.as_secs_f64()) / 1_000_000.0
    } else {
        0.0
    };
    let total_time = format_eta(total_elapsed.as_secs());
    let stopped_by_user = stopped_from_monitor || stop_requested.load(Ordering::Relaxed);
    let stop_reason_value = stop_reason
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    if let Some(reason) = &stop_reason_value {
        overall_pb.finish_with_message(format!(
            "Stopped: {} | {} succeeded, {} failed | Avg: {:.1} MB/s | Elapsed: {} (sync: {}s)",
            reason,
            completed.to_string().yellow().bold(),
            failed,
            throughput,
            total_time,
            sync_duration.as_secs()
        ));
    } else if stopped_by_user {
        overall_pb.finish_with_message(format!(
            "Stopped by user. {} succeeded, {} failed | Avg: {:.1} MB/s | Elapsed: {} (sync: {}s)",
            completed.to_string().yellow().bold(),
            failed,
            throughput,
            total_time,
            sync_duration.as_secs()
        ));
    } else {
        overall_pb.finish_with_message(format!(
            "✓ Transfer complete! {} succeeded, {} failed | Avg: {:.1} MB/s | Total time: {} (sync: {}s)",
            completed.to_string().green().bold(),
            failed,
            throughput,
            total_time,
            sync_duration.as_secs()
        ));
    }

    Ok(TransferOutcome {
        stopped_by_user,
        files_completed: completed,
        files_failed: failed,
        stop_reason: stop_reason_value,
    })
}
