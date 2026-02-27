mod config;
mod queue;
mod transfer;
mod ui;
mod unmount;
mod worker;

use anyhow::{Context, Result};
use colored::*;
use indicatif::MultiProgress;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use config::UserConfig;
use notify::{EventKind, RecursiveMode, Watcher};
use queue::{QueueCommand, TransferItem, TransferQueue};
use ui::{
    add_more_files, get_transfer_mappings, get_transfer_options, print_banner, TransferMapping,
};
use unmount::safe_unmount;
use worker::transfer_worker_pool;

const QUEUE_CHECKPOINT_FILE: &str = "pending_queue.json";

#[derive(Debug, Serialize, Deserialize)]
struct QueueCheckpoint {
    items: Vec<TransferItem>,
}

fn watch_paths_for_user(user: Option<&str>) -> Vec<PathBuf> {
    let mut watch_paths = vec![Path::new("/media").to_path_buf()];
    if let Some(user) = user {
        watch_paths.push(Path::new("/run/media").join(user));
    }
    watch_paths
}

fn mounts_contains_path(content: &str, mount_point: &Path) -> bool {
    let mount_point = mount_point.to_string_lossy();
    content.lines().any(|line| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        parts.len() >= 2 && parts[1] == mount_point
    })
}

fn is_mount_point(p: &Path) -> bool {
    std::fs::read_to_string("/proc/mounts")
        .map(|content| mounts_contains_path(&content, p))
        .unwrap_or(false)
}

fn auto_queue_mappings_for_mount(
    queue: &Arc<TransferQueue>,
    mappings: &[TransferMapping],
    default_destination: &Path,
    mount_path: &Path,
) {
    if !mount_path.is_dir() || !is_mount_point(mount_path) {
        return;
    }

    println!(
        "\n{} Detected device mounted at {}",
        "ℹ".cyan(),
        mount_path.display()
    );

    // For each mapping that targeted the default_destination,
    // translate to the newly-mounted path and add to queue
    for mapping in mappings {
        // Only auto-queue mappings that used the generic default destination
        if !mapping.destination.starts_with(default_destination) {
            continue;
        }

        // Compute relative suffix after default_destination.
        let rel = mapping
            .destination
            .strip_prefix(default_destination)
            .unwrap_or_else(|_| Path::new(""));
        let new_dest = mount_path.join(rel);

        if mapping.source.is_file() {
            if let Err(e) = queue.add_file(mapping.source.clone(), &new_dest) {
                eprintln!(
                    "Failed to add file {} -> {}: {}",
                    mapping.source.display(),
                    new_dest.display(),
                    e
                );
            } else {
                println!(
                    "  {} Auto-queued: {} → {}",
                    "→".cyan().bold(),
                    mapping.source.display(),
                    new_dest.display()
                );
            }
        } else if mapping.source.is_dir() {
            if let Err(e) = queue.add_directory(&mapping.source, &new_dest) {
                eprintln!(
                    "Failed to add directory {} -> {}: {}",
                    mapping.source.display(),
                    new_dest.display(),
                    e
                );
            } else {
                println!(
                    "  {} Auto-queued directory: {} → {}",
                    "→".cyan().bold(),
                    mapping.source.display(),
                    new_dest.display()
                );
            }
        }
    }
}

fn mounted_paths_in_watch_roots(watch_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut mounts = Vec::new();

    for root in watch_paths {
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && is_mount_point(&path) {
                mounts.push(path);
            }
        }
    }

    mounts
}

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

fn clear_queue_checkpoint() -> Result<()> {
    match fs::remove_file(QUEUE_CHECKPOINT_FILE) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn save_queue_checkpoint(queue: &TransferQueue, last_saved_revision: &mut u64) -> Result<()> {
    let revision = queue.revision();
    if revision == *last_saved_revision {
        return Ok(());
    }

    let items = queue.snapshot_items();
    if items.is_empty() {
        clear_queue_checkpoint()?;
        *last_saved_revision = revision;
        return Ok(());
    }

    let checkpoint = QueueCheckpoint { items };
    let json = serde_json::to_vec(&checkpoint)?;
    fs::write(QUEUE_CHECKPOINT_FILE, json)
        .with_context(|| format!("Failed to write {}", QUEUE_CHECKPOINT_FILE))?;
    *last_saved_revision = revision;
    Ok(())
}

fn load_queue_checkpoint() -> Result<Option<Vec<TransferItem>>> {
    let path = Path::new(QUEUE_CHECKPOINT_FILE);
    if !path.exists() {
        return Ok(None);
    }

    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) => {
            eprintln!(
                "{} Failed to read {}: {}",
                "Warning:".yellow(),
                QUEUE_CHECKPOINT_FILE,
                e
            );
            return Ok(None);
        }
    };
    let checkpoint: QueueCheckpoint = match serde_json::from_str(&content) {
        Ok(data) => data,
        Err(e) => {
            eprintln!(
                "{} Invalid {} (ignored): {}",
                "Warning:".yellow(),
                QUEUE_CHECKPOINT_FILE,
                e
            );
            clear_queue_checkpoint()?;
            return Ok(None);
        }
    };

    if checkpoint.items.is_empty() {
        clear_queue_checkpoint()?;
        return Ok(None);
    }

    Ok(Some(checkpoint.items))
}

fn print_config_validation(config: &UserConfig) {
    let warnings = config.validate_startup();
    if warnings.is_empty() {
        return;
    }

    println!("\n{}", "Startup config warnings:".yellow().bold());
    for warning in warnings {
        println!("  {} {}", "•".yellow(), warning);
    }
    println!(
        "{}",
        "You can continue, but fixing these paths is recommended."
            .yellow()
            .dimmed()
    );
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
                let _ = queue.get_sender().send(QueueCommand::Stop);
            }
        }
    });
}

#[tokio::main]
async fn main() -> Result<()> {
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

    if !resumed_from_checkpoint {
        // Get source → destination mappings (includes default destination)
        let (mappings, selected_default_destination) = get_transfer_mappings()?;
        default_destination = selected_default_destination;

        // Add all initial mappings to queue if at least one configured destination exists.
        // If none of the mapping destinations exist yet (no USB mounted), spawn a watcher
        // that will automatically queue the mappings when a device is mounted.
        let any_destination_present = mappings.iter().any(|m| m.destination.exists());
        if any_destination_present {
            println!("\n{}", "Building transfer queue...".cyan().bold());
            for mapping in mappings.clone() {
                if mapping.source.is_file() {
                    queue.add_file(mapping.source.clone(), &mapping.destination)?;
                    println!(
                        "  {} {} → {}",
                        "✓".green(),
                        mapping.source.display(),
                        mapping.destination.display()
                    );
                } else if mapping.source.is_dir() {
                    println!(
                        "  {} Scanning: {} → {}",
                        "📁".cyan(),
                        mapping.source.display(),
                        mapping.destination.display()
                    );
                    queue.add_directory(&mapping.source, &mapping.destination)?;
                    println!("    {} Directory added", "✓".green());
                }
            }
        } else {
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

            thread::spawn(move || {
                // Directories to watch
                let user = std::env::var("USER").ok();
                let watch_paths = watch_paths_for_user(user.as_deref());

                let (tx, rx) = channel();
                let mut watcher = notify::recommended_watcher(
                    move |res: Result<notify::Event, notify::Error>| {
                        let _ = tx.send(res);
                    },
                )
                .expect("Failed to create watcher");

                for p in &watch_paths {
                    let _ = watcher.watch(p, RecursiveMode::NonRecursive);
                }

                let mut seen_mount_paths = HashSet::new();

                // Catch devices that are already mounted before watcher starts.
                for path in mounted_paths_in_watch_roots(&watch_paths) {
                    if seen_mount_paths.insert(path.clone()) {
                        auto_queue_mappings_for_mount(
                            &queue_clone,
                            &mappings_clone,
                            &default_dest_clone,
                            &path,
                        );
                    }
                }

                for event in rx.iter().flatten() {
                    if matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                        for path in event.paths {
                            if path.is_dir()
                                && is_mount_point(&path)
                                && seen_mount_paths.insert(path.clone())
                            {
                                auto_queue_mappings_for_mount(
                                    &queue_clone,
                                    &mappings_clone,
                                    &default_dest_clone,
                                    &path,
                                );
                            }
                        }
                    }

                    // Also rescan watch roots to catch mounts on pre-existing directories.
                    for path in mounted_paths_in_watch_roots(&watch_paths) {
                        if seen_mount_paths.insert(path.clone()) {
                            auto_queue_mappings_for_mount(
                                &queue_clone,
                                &mappings_clone,
                                &default_dest_clone,
                                &path,
                            );
                        }
                    }
                }
            });
        }

        save_queue_checkpoint(queue.as_ref(), &mut last_saved_revision)?;
    }

    // Get transfer options
    let (num_workers, verify, should_unmount, cleanup_mode) = get_transfer_options()?;

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
            if let Err(e) = add_more_files(queue.clone(), default_destination.clone()).await {
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
        if queue.is_empty() {
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
        let mut seen = HashSet::new();
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
