mod checkpoint;
mod mounts;
mod preflight;

use anyhow::Result;
use colored::Colorize;
use indicatif::MultiProgress;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::config::UserConfig;
use crate::queue::TransferQueue;
use crate::ui::{
    add_more_files, get_transfer_mappings, get_transfer_options, print_banner, TransferMapping,
};
use crate::unmount::safe_unmount;
use crate::worker::transfer_worker_pool;

use checkpoint::{clear_queue_checkpoint, load_queue_checkpoint, save_queue_checkpoint};
use mounts::{print_mounted_destination_speeds, watch_mounts_for_queueing};
use preflight::{preflight_pending_destinations, print_config_validation};

fn prompt_yes_no(prompt: &str, default_yes: bool) -> Result<bool> {
    print!("{} ", prompt);
    io::stdout().flush()?;

    let mut response = String::new();
    io::stdin().read_line(&mut response)?;

    let value = response.trim().to_lowercase();
    if value.is_empty() {
        return Ok(default_yes);
    }

    Ok(matches!(value.as_str(), "y" | "yes" | "o" | "oui"))
}

fn install_ctrl_c_handler(queue: Arc<TransferQueue>, stop_requested: Arc<AtomicBool>) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let already_requested = stop_requested.swap(true, Ordering::SeqCst);
            if !already_requested {
                println!(
                    "\n{}",
                    "Ctrl+C detected: stopping after current file(s)..."
                        .yellow()
                        .bold()
                );
                let _ = queue.get_sender().send(crate::queue::QueueCommand::Stop);
            }
        }
    });
}

fn enqueue_initial_mappings(
    queue: &Arc<TransferQueue>,
    mappings: &[TransferMapping],
    update_mode: bool,
) -> Result<bool> {
    let any_destination_present = mappings.iter().any(|m| m.destination.exists());
    if !any_destination_present {
        return Ok(false);
    }

    println!("\n{}", "Building transfer queue...".cyan().bold());
    for mapping in mappings.iter().cloned() {
        if mapping.source.is_file() {
            let summary = queue.add_file_with_policy(
                mapping.source.clone(),
                &mapping.destination,
                update_mode,
            )?;
            if summary.queued_files > 0 {
                println!(
                    "  {} {} → {}",
                    "✓".green(),
                    mapping.source.display(),
                    mapping.destination.display()
                );
            } else if summary.skipped_files > 0 {
                println!(
                    "  {} Skipped unchanged: {}",
                    "↷".yellow(),
                    mapping.source.display()
                );
            }
        } else if mapping.source.is_dir() {
            println!(
                "  {} Scanning: {} → {}",
                "📁".cyan(),
                mapping.source.display(),
                mapping.destination.display()
            );
            let summary = queue.add_directory_with_policy(
                &mapping.source,
                &mapping.destination,
                update_mode,
            )?;
            if summary.queued_files > 0 {
                println!(
                    "    {} Directory added ({} file(s))",
                    "✓".green(),
                    summary.queued_files
                );
            }
            if summary.skipped_files > 0 {
                println!(
                    "    {} Skipped unchanged file(s): {}",
                    "↷".yellow(),
                    summary.skipped_files
                );
            }
        }
    }

    Ok(true)
}

pub async fn run() -> Result<()> {
    // Print welcome banner
    print_banner();

    // Create transfer queue
    let queue = Arc::new(TransferQueue::new());
    let stop_requested = Arc::new(AtomicBool::new(false));
    let mut last_saved_revision = u64::MAX;

    let startup_config = UserConfig::load();
    print_config_validation(&startup_config);
    let mut default_destination = startup_config
        .default_destination_folder
        .clone()
        .unwrap_or_else(|| PathBuf::from("/media/usb"));

    install_ctrl_c_handler(queue.clone(), stop_requested.clone());

    let mut resumed_from_checkpoint = false;
    let mut waiting_for_mount = false;
    if let Some(saved_items) = load_queue_checkpoint()? {
        println!(
            "\n{} Found interrupted session: {} pending item(s).",
            "ℹ".cyan().bold(),
            saved_items.len()
        );

        if prompt_yes_no("Resume pending transfer? (Y/n):", true)? {
            queue.restore_items(saved_items);
            resumed_from_checkpoint = true;
            println!("{}", "Resuming from saved queue.".green().bold());
        } else {
            clear_queue_checkpoint()?;
            println!("{}", "Saved queue ignored.".yellow());
        }
    }

    // Get transfer options
    let (num_workers, verify, should_unmount, cleanup_mode, update_mode) = get_transfer_options()?;

    if !resumed_from_checkpoint {
        // Get source -> destination mappings (includes default destination)
        let (mappings, selected_default_destination) = get_transfer_mappings()?;
        default_destination = selected_default_destination;

        if !enqueue_initial_mappings(&queue, &mappings, update_mode)? {
            println!(
                "\n{}",
                "No configured destination is present — watching for device mounts..."
                    .yellow()
                    .bold()
            );
            waiting_for_mount = true;

            // Spawn a background thread to watch common mount points and auto-queue
            let queue_clone = queue.clone();
            let mappings_clone = mappings.clone();
            let default_dest_clone = default_destination.clone();
            let update_mode_clone = update_mode;

            thread::spawn(move || {
                watch_mounts_for_queueing(
                    queue_clone,
                    mappings_clone,
                    default_dest_clone,
                    update_mode_clone,
                )
            });
        }

        save_queue_checkpoint(queue.as_ref(), &mut last_saved_revision)?;
    }

    let mut run_stop_reason: Option<String> = None;

    // Transfer loop - allows adding files between batches
    loop {
        if stop_requested.load(Ordering::Relaxed) {
            break;
        }

        let total_items = queue.len();
        let total_size = queue.total_size();

        if total_items == 0 {
            if waiting_for_mount {
                println!(
                    "\n{}",
                    "No queued files yet. Waiting for a device mount... (Ctrl+C to cancel)"
                        .yellow()
                        .bold()
                );

                while queue.is_empty() && !stop_requested.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }

                if stop_requested.load(Ordering::Relaxed) {
                    break;
                }

                if !queue.is_empty() {
                    waiting_for_mount = false;
                    println!(
                        "\n{}",
                        "✓ Device detected and files queued. Starting transfer..."
                            .green()
                            .bold()
                    );
                    continue;
                }
            }

            println!("\n{}", "✓ Queue is empty!".green().bold());
            break;
        }

        println!(
            "\n{} {} items ({:.2} MB) ready for transfer",
            "✓".green().bold(),
            total_items,
            total_size as f64 / 1_000_000.0
        );

        let pending_items = queue.snapshot_items();
        preflight_pending_destinations(&pending_items)?;
        print_mounted_destination_speeds(&pending_items);

        // Start transfer with worker pool
        let multi_progress = MultiProgress::new();

        println!(
            "\n{} Starting transfer with {} parallel workers...\n",
            "🚀".bold(),
            num_workers.to_string().green().bold()
        );

        save_queue_checkpoint(queue.as_ref(), &mut last_saved_revision)?;

        let outcome = transfer_worker_pool(
            queue.clone(),
            multi_progress,
            stop_requested.clone(),
            num_workers,
            verify,
            cleanup_mode.clone(),
        )
        .await?;

        save_queue_checkpoint(queue.as_ref(), &mut last_saved_revision)?;

        if outcome.stopped_by_user || stop_requested.load(Ordering::Relaxed) {
            if let Some(reason) = &outcome.stop_reason {
                run_stop_reason = Some(reason.clone());
                println!(
                    "\n{} {} | {} succeeded, {} failed in this batch.",
                    "Transfer stopped:".red().bold(),
                    reason,
                    outcome.files_completed,
                    outcome.files_failed
                );
                break;
            }

            let pending_items = queue.len();
            if pending_items > 0 {
                println!(
                    "\n{} {} succeeded, {} failed in this batch ({} item(s) still queued).",
                    "Transfer interrupted:".yellow().bold(),
                    outcome.files_completed,
                    outcome.files_failed,
                    pending_items
                );
            } else {
                println!(
                    "\n{} {} succeeded, {} failed in this batch.",
                    "Batch complete (stop requested at end):".yellow().bold(),
                    outcome.files_completed,
                    outcome.files_failed
                );
            }
            break;
        }

        println!("\n{}", "✓ Batch complete!".green().bold());

        // Ask if user wants to add more files
        print!("\n{} ", "Add more files? (y/N):".yellow());
        io::stdout().flush()?;

        let mut response = String::new();
        io::stdin().read_line(&mut response)?;

        if response.trim().eq_ignore_ascii_case("y") {
            // Add more files
            if let Err(e) =
                add_more_files(queue.clone(), default_destination.clone(), update_mode).await
            {
                println!("{} {}", "Warning:".yellow(), e);
                break;
            }
            save_queue_checkpoint(queue.as_ref(), &mut last_saved_revision)?;
            // Loop continues, will transfer the new files
        } else {
            // User doesn't want to add more, exit loop
            break;
        }
    }

    if queue.is_empty() {
        clear_queue_checkpoint()?;
    } else {
        save_queue_checkpoint(queue.as_ref(), &mut last_saved_revision)?;
    }

    if stop_requested.load(Ordering::Relaxed) {
        if let Some(reason) = run_stop_reason {
            if queue.is_empty() {
                println!(
                    "\n{}",
                    format!("Stopped: {}. No pending items left in queue.", reason)
                        .red()
                        .bold()
                );
            } else {
                println!(
                    "\n{}",
                    format!(
                        "Stopped: {}. Pending queue saved to pending_queue.json.",
                        reason
                    )
                    .red()
                    .bold()
                );
            }
        } else if queue.is_empty() {
            println!(
                "\n{}",
                "Stopped by user. No pending items left in queue."
                    .yellow()
                    .bold()
            );
        } else {
            println!(
                "\n{}",
                "Stopped by user. Pending queue saved to pending_queue.json."
                    .yellow()
                    .bold()
            );
        }
        return Ok(());
    }

    println!("\n{}", "✓ All transfers complete!".green().bold());

    // Unmount if requested
    if should_unmount {
        let config = UserConfig::load();
        let mut seen = std::collections::HashSet::new();
        for mount_point in config.get_default_unmount_drives() {
            if seen.insert(mount_point.clone()) {
                safe_unmount(&mount_point)?;
            }
        }
    }

    println!("\n{}", "Press Enter to exit.".cyan());
    let mut dummy = String::new();
    io::stdin().read_line(&mut dummy)?;

    Ok(())
}
