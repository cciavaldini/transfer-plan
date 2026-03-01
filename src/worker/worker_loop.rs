use crate::queue::{QueueCommand, TransferQueue};
use crate::transfer::copy_file_optimized;
use colored::Colorize;
use indicatif::ProgressBar;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::cleanup::finish_successful_transfer;
use super::progress_ui::{
    format_file_progress_label, FILE_PROGRESS_BAR_THRESHOLD, FILE_PROGRESS_STYLE,
};
use super::space::{
    ensure_destination_parent_cached, release_reserved_space, reserve_destination_space,
    SpaceProbeCache,
};
use super::sync::push_sync_target;
use super::types::{TransferStats, WorkerContext};

const QUEUE_POLL_INTERVAL_MS: u64 = 50;

fn set_stop_reason_once(stop_reason: &Arc<Mutex<Option<String>>>, reason: String) {
    let mut guard = stop_reason.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Some(reason);
    }
}

fn transfer_failure_stop(
    queue: &TransferQueue,
    stats: &TransferStats,
    stop_reason: &Arc<Mutex<Option<String>>>,
    stop_requested: &Arc<AtomicBool>,
    file_pb: &ProgressBar,
    worker_prefix: &str,
    file_name: &str,
    show_file_progress: bool,
    reason: String,
) {
    stats.inc_failed();
    set_stop_reason_once(stop_reason, reason.clone());
    stop_requested.store(true, Ordering::SeqCst);
    let _ = queue.get_sender().send(QueueCommand::Stop);

    if show_file_progress {
        file_pb.finish_with_message(format!(
            "{}✗ {} - {}",
            worker_prefix,
            file_name.red(),
            reason
        ));
    } else {
        eprintln!("{}✗ {} - {}", worker_prefix, file_name, reason);
        file_pb.finish();
    }
}

/// Single worker that processes files from the queue
pub(crate) fn transfer_worker_single(worker_id: usize, ctx: WorkerContext) {
    let WorkerContext {
        queue,
        multi_progress,
        overall_pb,
        stop_requested,
        stop_reason,
        stats,
        io_bytes,
        sync_targets,
        space_reservations,
        cleanup_roots,
        verify,
        cleanup_mode,
    } = ctx;

    let mut prepared_destination_parents = HashSet::new();
    let mut space_probe_cache = SpaceProbeCache::new();

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

        let worker_prefix = format!("[W{}] ", worker_id);
        let progress_label = format_file_progress_label(worker_id, &file_name);

        if show_file_progress {
            file_pb.set_prefix(progress_label);
            file_pb.set_message("");
        }

        if let Err(e) =
            ensure_destination_parent_cached(&item.destination, &mut prepared_destination_parents)
        {
            transfer_failure_stop(
                &queue,
                &stats,
                &stop_reason,
                &stop_requested,
                &file_pb,
                &worker_prefix,
                &file_name,
                show_file_progress,
                e.to_string(),
            );
            break;
        }

        let reserved_space = match reserve_destination_space(
            &item.destination,
            item.size,
            &space_reservations,
            &mut space_probe_cache,
        ) {
            Ok(token) => token,
            Err(e) => {
                transfer_failure_stop(
                    &queue,
                    &stats,
                    &stop_reason,
                    &stop_requested,
                    &file_pb,
                    &worker_prefix,
                    &file_name,
                    show_file_progress,
                    e.to_string(),
                );
                break;
            }
        };

        push_sync_target(&sync_targets, &item.destination);

        // Transfer the file
        let copy_result = copy_file_optimized(
            &item.source,
            &item.destination,
            item.size,
            file_pb.clone(),
            verify,
            io_bytes.clone(),
            overall_pb.clone(),
        );
        release_reserved_space(&space_reservations, reserved_space);

        match copy_result {
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
                    &cleanup_roots,
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
