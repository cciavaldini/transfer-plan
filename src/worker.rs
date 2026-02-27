use crate::queue::{QueueCommand, TransferQueue};
use crate::transfer::{copy_file_optimized, format_size};
use anyhow::{Context, Result};
use colored::Colorize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use once_cell::sync::Lazy;
use std::collections::HashSet;
use std::fs;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const QUEUE_POLL_INTERVAL_MS: u64 = 50;
const MONITOR_INTERVAL_MS: u64 = 1500;
const QUEUE_SETTLE_MS: u64 = 500;
const SPEED_SMOOTHING_ALPHA: f64 = 0.35;
const MAX_PENDING_SYNC_ESTIMATE_SECS: f64 = 1800.0;
const SYNC_ESTIMATE_SPEED_FACTOR: f64 = 0.35;
const SYNC_ESTIMATE_MAX_FLUSH_SPEED_BPS: f64 = 120.0 * 1_000_000.0;
const SYNC_ESTIMATE_FALLBACK_SPEED_BPS: f64 = 25.0 * 1_000_000.0;
const SYNC_ESTIMATE_BASE_OVERHEAD_SECS: f64 = 3.0;
const SYNC_ESTIMATE_SAFETY_MULTIPLIER: f64 = 1.5;
const SYNC_MONITOR_INTERVAL_MS: u64 = 300;
const FILE_PROGRESS_BAR_THRESHOLD: u64 = 8 * 1024 * 1024;

static FILE_PROGRESS_STYLE: Lazy<ProgressStyle> = Lazy::new(|| {
    ProgressStyle::default_bar()
        .template(
            "  {msg} [{bar:30.green/white}] {bytes}/{total_bytes} ({eta}) {binary_bytes_per_sec:.cyan}",
        )
        .unwrap()
        .progress_chars("█▓▒░-")
});

pub struct TransferOutcome {
    pub stopped_by_user: bool,
    pub files_completed: u64,
    pub files_failed: u64,
}

struct MonitorOutcome {
    stopped_by_user: bool,
    smoothed_speed_bps: f64,
}

pub struct TransferStats {
    bytes_transferred: AtomicU64,
    files_completed: AtomicU64,
    files_failed: AtomicU64,
    start_time: Instant,
}

#[derive(Clone)]
struct WorkerContext {
    queue: Arc<TransferQueue>,
    multi_progress: MultiProgress,
    overall_pb: ProgressBar,
    stop_requested: Arc<AtomicBool>,
    stats: Arc<TransferStats>,
    io_bytes: Arc<AtomicU64>,
    sync_targets: Arc<Mutex<Vec<std::path::PathBuf>>>,
    verify: bool,
    cleanup_mode: Arc<str>,
}

impl TransferStats {
    pub fn new() -> Self {
        Self {
            bytes_transferred: AtomicU64::new(0),
            files_completed: AtomicU64::new(0),
            files_failed: AtomicU64::new(0),
            start_time: Instant::now(),
        }
    }

    pub fn add_bytes(&self, bytes: u64) {
        self.bytes_transferred.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn inc_completed(&self) {
        self.files_completed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_failed(&self) {
        self.files_failed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get_stats(&self) -> (u64, u64, u64) {
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

fn format_eta(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, secs)
}

fn fallback_sync() {
    #[cfg(target_family = "unix")]
    unsafe {
        libc::sync();
    }
}

fn estimated_flush_speed_bps(smoothed_speed_bps: f64) -> f64 {
    if smoothed_speed_bps > 0.0 {
        (smoothed_speed_bps * SYNC_ESTIMATE_SPEED_FACTOR).min(SYNC_ESTIMATE_MAX_FLUSH_SPEED_BPS)
    } else {
        SYNC_ESTIMATE_FALLBACK_SPEED_BPS
    }
}

#[cfg(target_os = "linux")]
fn pending_writeback_bytes() -> Option<u64> {
    let content = fs::read_to_string("/proc/meminfo").ok()?;

    let mut dirty_kb = None;
    let mut writeback_kb = None;

    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next()?;
        let value = parts.next()?.parse::<u64>().ok()?;

        match key {
            "Dirty:" => dirty_kb = Some(value),
            "Writeback:" => writeback_kb = Some(value),
            _ => {}
        }
    }

    Some((dirty_kb.unwrap_or(0) + writeback_kb.unwrap_or(0)) * 1024)
}

#[cfg(not(target_os = "linux"))]
fn pending_writeback_bytes() -> Option<u64> {
    None
}

fn estimate_pending_sync_secs(smoothed_speed_bps: f64) -> Option<f64> {
    let pending_bytes = pending_writeback_bytes()?;
    if pending_bytes == 0 {
        return Some(0.0);
    }

    // Sync flush throughput is usually lower than copy throughput; keep estimate conservative.
    let estimated_flush_speed_bps = estimated_flush_speed_bps(smoothed_speed_bps);

    let estimated_secs = (SYNC_ESTIMATE_BASE_OVERHEAD_SECS
        + (pending_bytes as f64 / estimated_flush_speed_bps))
        * SYNC_ESTIMATE_SAFETY_MULTIPLIER;
    Some(estimated_secs.clamp(0.0, MAX_PENDING_SYNC_ESTIMATE_SECS))
}

#[inline]
fn smooth_speed(previous: f64, current_sample: f64) -> f64 {
    if previous == 0.0 {
        current_sample
    } else {
        SPEED_SMOOTHING_ALPHA * current_sample + (1.0 - SPEED_SMOOTHING_ALPHA) * previous
    }
}

fn push_sync_target(sync_targets: &Arc<Mutex<Vec<std::path::PathBuf>>>, destination: &std::path::Path) {
    let target = destination
        .parent()
        .unwrap_or(destination)
        .to_path_buf();
    let mut guard = sync_targets.lock().unwrap_or_else(|e| e.into_inner());
    guard.push(target);
}

fn open_sync_handle(path: &std::path::Path) -> std::io::Result<fs::File> {
    let mut current = Some(path);
    while let Some(p) = current {
        match fs::File::open(p) {
            Ok(file) => return Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                current = p.parent();
            }
            Err(e) => return Err(e),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("No existing path found for sync target {}", path.display()),
    ))
}

#[cfg(target_os = "linux")]
fn run_scoped_sync(sync_paths: &[std::path::PathBuf]) -> Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    let mut synced_devices = HashSet::new();
    let mut synced_any = false;
    let mut first_error: Option<anyhow::Error> = None;

    for path in sync_paths {
        let handle = match open_sync_handle(path) {
            Ok(handle) => handle,
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(anyhow::anyhow!(
                        "Failed to open sync target {}: {}",
                        path.display(),
                        e
                    ));
                }
                continue;
            }
        };

        let dev_id = match handle.metadata() {
            Ok(meta) => meta.dev(),
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(anyhow::anyhow!(
                        "Failed to read metadata for sync target {}: {}",
                        path.display(),
                        e
                    ));
                }
                continue;
            }
        };

        if !synced_devices.insert(dev_id) {
            continue;
        }

        let rc = unsafe { libc::syncfs(handle.as_raw_fd()) };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            if first_error.is_none() {
                first_error = Some(anyhow::anyhow!(
                    "syncfs failed for {}: {}",
                    path.display(),
                    e
                ));
            }
            continue;
        }

        synced_any = true;
    }

    if synced_any {
        Ok(())
    } else if let Some(err) = first_error {
        Err(err)
    } else {
        Err(anyhow::anyhow!(
            "No valid sync target found for scoped sync"
        ))
    }
}

#[cfg(not(target_os = "linux"))]
fn run_scoped_sync(_sync_paths: &[std::path::PathBuf]) -> Result<()> {
    fallback_sync();
    Ok(())
}

fn run_post_transfer_sync(
    overall_pb: &ProgressBar,
    sync_paths: &[std::path::PathBuf],
    flush_speed_hint_bps: f64,
) -> Duration {
    let sync_start = Instant::now();
    let sync_paths = sync_paths.to_vec();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let _ = tx.send(run_scoped_sync(&sync_paths));
    });

    let mut last_pending = pending_writeback_bytes();
    let mut last_pending_sample = Instant::now();
    let mut observed_flush_speed_bps = 0.0;

    loop {
        match rx.recv_timeout(Duration::from_millis(SYNC_MONITOR_INTERVAL_MS)) {
            Ok(result) => {
                if let Err(e) = result {
                    eprintln!(
                        "⚠️  Warning: scoped sync failed ({}). Falling back to libc::sync().",
                        e
                    );
                    fallback_sync();
                }
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                let pending_now = pending_writeback_bytes();

                if let (Some(prev), Some(current)) = (last_pending, pending_now) {
                    let dt = now.duration_since(last_pending_sample).as_secs_f64();
                    if dt > 0.0 {
                        let drained_bytes = prev.saturating_sub(current) as f64;
                        if drained_bytes > 0.0 {
                            let sample_speed = drained_bytes / dt;
                            observed_flush_speed_bps =
                                smooth_speed(observed_flush_speed_bps, sample_speed);
                        }
                    }

                    last_pending = Some(current);
                    last_pending_sample = now;

                    let effective_flush_speed_bps = if observed_flush_speed_bps > 0.0 {
                        observed_flush_speed_bps
                    } else {
                        flush_speed_hint_bps
                    };
                    let eta_secs = if effective_flush_speed_bps > 0.0 {
                        (current as f64 / effective_flush_speed_bps).ceil() as u64
                    } else {
                        0
                    };
                    overall_pb.set_message(format!(
                        "Finalizing writes (sync)... Remaining: {}",
                        format_eta(eta_secs)
                    ));
                } else {
                    overall_pb.set_message(format!(
                        "Finalizing writes (sync)... Elapsed: {}",
                        format_eta(sync_start.elapsed().as_secs())
                    ));
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!(
                    "⚠️  Warning: sync monitor channel closed unexpectedly. Falling back to libc::sync()."
                );
                fallback_sync();
                break;
            }
        }
    }

    sync_start.elapsed()
}

fn finish_successful_transfer(
    file_pb: &ProgressBar,
    worker_prefix: &str,
    file_name: &str,
    show_feedback: bool,
    cleanup_mode: &str,
    source: &std::path::Path,
    cleanup_root: Option<&std::path::Path>,
) {
    match cleanup_mode {
        "delete" => match fs::remove_file(source) {
            Ok(_) => {
                remove_empty_source_directories(source, cleanup_root);
                if show_feedback {
                    file_pb.finish_with_message(format!(
                        "{}✓ {} (removed)",
                        worker_prefix,
                        file_name.green()
                    ));
                } else {
                    file_pb.finish();
                }
            }
            Err(e) => {
                if show_feedback {
                    file_pb.finish_with_message(format!(
                        "{}✓ {} (transferred, but failed to remove: {})",
                        worker_prefix,
                        file_name.yellow(),
                        e
                    ));
                } else {
                    eprintln!(
                        "{}⚠ {} transferred, but failed to remove source: {}",
                        worker_prefix, file_name, e
                    );
                    file_pb.finish();
                }
            }
        },
        _ => {
            // none: leave source intact
            if show_feedback {
                file_pb.finish_with_message(format!(
                    "{}✓ {} (complete)",
                    worker_prefix,
                    file_name.green()
                ));
            } else {
                file_pb.finish();
            }
        }
    }
}

fn remove_empty_source_directories(
    source_file: &std::path::Path,
    cleanup_root: Option<&std::path::Path>,
) {
    let Some(root) = cleanup_root else {
        return;
    };

    if !source_file.starts_with(root) {
        return;
    }

    let mut current = source_file.parent();

    while let Some(dir) = current {
        if !dir.starts_with(root) {
            break;
        }

        match fs::remove_dir(dir) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
            Err(_) => break,
        }

        if dir == root {
            break;
        }

        current = dir.parent();
    }
}

/// Single worker that processes files from the queue
fn transfer_worker_single(worker_id: usize, ctx: WorkerContext) {
    let WorkerContext {
        queue,
        multi_progress,
        overall_pb,
        stop_requested,
        stats,
        io_bytes,
        sync_targets,
        verify,
        cleanup_mode,
    } = ctx;

    loop {
        if stop_requested.load(Ordering::Relaxed) {
            break;
        }

        // Get next item
        let Some(item) = queue.recv_timeout(Duration::from_millis(QUEUE_POLL_INTERVAL_MS)) else {
            if queue.is_empty() || stop_requested.load(Ordering::Relaxed) {
                break;
            }
            continue;
        };

        let show_file_progress = item.size >= FILE_PROGRESS_BAR_THRESHOLD;
        let file_pb = if show_file_progress {
            let pb = multi_progress.insert_before(&overall_pb, ProgressBar::new(item.size));
            pb.set_style(FILE_PROGRESS_STYLE.clone());
            pb
        } else {
            let pb = ProgressBar::hidden();
            pb.set_length(item.size);
            pb
        };

        let file_name = item
            .source
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let worker_prefix = if worker_id > 0 {
            format!("[W{}] ", worker_id)
        } else {
            String::new()
        };

        if show_file_progress {
            file_pb.set_message(format!("{}📄 {}", worker_prefix, file_name));
        }

        push_sync_target(&sync_targets, &item.destination);

        // Transfer the file
        match copy_file_optimized(
            &item.source,
            &item.destination,
            item.size,
            file_pb.clone(),
            verify,
            io_bytes.clone(),
            overall_pb.clone(),
        ) {
            Ok(_) => {
                stats.add_bytes(item.size);
                stats.inc_completed();
                finish_successful_transfer(
                    &file_pb,
                    &worker_prefix,
                    &file_name,
                    show_file_progress,
                    cleanup_mode.as_ref(),
                    &item.source,
                    item.cleanup_root.as_deref(),
                );
            }
            Err(e) => {
                stats.inc_failed();
                if show_file_progress {
                    file_pb.finish_with_message(format!(
                        "{}✗ {} - Error: {}",
                        worker_prefix,
                        file_name.red(),
                        e
                    ));
                } else {
                    eprintln!("{}✗ {} - Error: {}", worker_prefix, file_name, e);
                    file_pb.finish();
                }
            }
        }
    }
}

/// Main transfer worker pool with parallel processing
pub async fn transfer_worker_pool(
    queue: Arc<TransferQueue>,
    multi_progress: MultiProgress,
    stop_requested: Arc<AtomicBool>,
    num_workers: usize,
    verify: bool,
    cleanup_mode: String,
) -> Result<TransferOutcome> {
    let stats = Arc::new(TransferStats::new());
    let io_bytes = Arc::new(AtomicU64::new(0));
    let sync_targets = Arc::new(Mutex::new(Vec::new()));

    let overall_pb = multi_progress.add(ProgressBar::new(0));
    overall_pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} {msg}")
            .unwrap()
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
        stats: stats.clone(),
        io_bytes: io_bytes.clone(),
        sync_targets: sync_targets.clone(),
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
    let queue_clone = queue.clone();
    let overall_pb_clone = overall_pb.clone();
    let stats_clone = stats.clone();
    let io_bytes_clone = io_bytes.clone();
    let stop_requested_monitor = stop_requested.clone();
    let monitor_handle = tokio::spawn(async move {
        let mut last_sample_time = Instant::now();
        let mut last_io_bytes = 0u64;
        let mut smoothed_speed_bps = 0.0;
        let mut max_total_files = initial_total_files;
        let mut stopped_by_user = false;

        loop {
            // Update overall progress
            let current_io_bytes = io_bytes_clone.load(Ordering::Relaxed);
            let transferred_bytes = current_io_bytes.min(total_size);
            let (completed, failed, _) = stats_clone.get_stats();
            overall_pb_clone.set_position(transferred_bytes);

            let now = Instant::now();
            let sample_secs = now.duration_since(last_sample_time).as_secs_f64();
            if sample_secs > 0.0 {
                let delta = current_io_bytes.saturating_sub(last_io_bytes) as f64;
                let current_speed_bps = delta / sample_secs;
                smoothed_speed_bps = smooth_speed(smoothed_speed_bps, current_speed_bps);
                last_sample_time = now;
                last_io_bytes = current_io_bytes;
            }

            let discovered_total = completed + failed + queue_clone.len() as u64;
            if discovered_total > max_total_files {
                max_total_files = discovered_total;
            }

            let elapsed_secs = stats_clone.start_time.elapsed().as_secs_f64();
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
            overall_pb_clone.set_message(format!(
                "Files: {}/{} | Remaining: {} ({} files) | Total remaining: {} | Avg: {:.1} MB/s",
                completed,
                max_total_files,
                format_size(remaining_bytes),
                remaining_files,
                total_remaining,
                throughput_avg,
            ));

            // Check for commands
            if let Some(cmd) = queue_clone.try_recv_command() {
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

            if stop_requested_monitor.load(Ordering::Relaxed) {
                // Stop request is active; workers will finish current file and exit.
                overall_pb_clone.set_message("Stopping after current file(s)...");
            }

            if queue_clone.is_empty() && completed + failed > 0 {
                tokio::time::sleep(Duration::from_millis(QUEUE_SETTLE_MS)).await;
                if queue_clone.is_empty() {
                    break;
                }
            }

            tokio::time::sleep(Duration::from_millis(MONITOR_INTERVAL_MS)).await;
        }

        MonitorOutcome {
            stopped_by_user,
            smoothed_speed_bps,
        }
    });

    // Wait for all workers to complete
    for handle in handles {
        if handle.join().is_err() {
            anyhow::bail!("A transfer worker thread panicked");
        }
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

    if stopped_by_user {
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
    })
}
