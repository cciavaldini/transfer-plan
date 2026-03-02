//! Synchronization estimation and flushing utilities used to determine how much
//! data still needs to be synced to disk.

use anyhow::Result;
use indicatif::ProgressBar;
use std::collections::HashSet;
use std::fs;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::progress_ui::format_eta;

const SPEED_SMOOTHING_ALPHA: f64 = 0.35;
const MAX_PENDING_SYNC_ESTIMATE_SECS: f64 = 1800.0;
const SYNC_ESTIMATE_SPEED_FACTOR: f64 = 0.35;
const SYNC_ESTIMATE_MAX_FLUSH_SPEED_BPS: f64 = 120.0 * 1_000_000.0;
const SYNC_ESTIMATE_FALLBACK_SPEED_BPS: f64 = 25.0 * 1_000_000.0;
const SYNC_ESTIMATE_BASE_OVERHEAD_SECS: f64 = 3.0;
const SYNC_ESTIMATE_SAFETY_MULTIPLIER: f64 = 1.5;
const SYNC_MONITOR_INTERVAL_MS: u64 = 300;

pub(crate) fn estimated_flush_speed_bps(smoothed_speed_bps: f64) -> f64 {
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

pub(crate) fn estimate_pending_sync_secs(smoothed_speed_bps: f64) -> Option<f64> {
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
pub(crate) fn smooth_speed(previous: f64, current_sample: f64) -> f64 {
    if previous == 0.0 {
        current_sample
    } else {
        SPEED_SMOOTHING_ALPHA * current_sample + (1.0 - SPEED_SMOOTHING_ALPHA) * previous
    }
}

pub(crate) fn push_sync_target(
    sync_targets: &Arc<Mutex<Vec<std::path::PathBuf>>>,
    destination: &std::path::Path,
) {
    let target = destination.parent().unwrap_or(destination).to_path_buf();
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
fn run_scoped_sync(sync_paths: &[std::path::PathBuf]) -> Result<()> {
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

        if let Err(e) = handle.sync_all() {
            if first_error.is_none() {
                first_error = Some(anyhow::anyhow!(
                    "sync_all failed for {}: {}",
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

#[cfg(target_os = "linux")]
fn sync_strategy_label() -> &'static str {
    "syncfs (by device)"
}

#[cfg(not(target_os = "linux"))]
fn sync_strategy_label() -> &'static str {
    "sync_all (by path)"
}

pub(crate) fn run_post_transfer_sync(
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
                        "⚠️  Warning: {} failed during finalization: {}",
                        sync_strategy_label(),
                        e
                    );
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
                        "Finalizing writes ({})... Remaining: {}",
                        sync_strategy_label(),
                        format_eta(eta_secs)
                    ));
                } else {
                    overall_pb.set_message(format!(
                        "Finalizing writes ({})... Elapsed: {}",
                        sync_strategy_label(),
                        format_eta(sync_start.elapsed().as_secs())
                    ));
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!(
                    "⚠️  Warning: sync monitor channel closed unexpectedly (strategy: {}).",
                    sync_strategy_label()
                );
                break;
            }
        }
    }

    sync_start.elapsed()
}
