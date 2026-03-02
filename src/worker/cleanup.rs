//! Cleanup helpers run after each successful file transfer.

use colored::Colorize;
use indicatif::ProgressBar;
use std::cmp::Reverse;
use std::collections::HashSet;
use std::fs;
use std::sync::{Arc, Mutex};

pub(crate) fn finish_successful_transfer(
    file_pb: &ProgressBar,
    worker_prefix: &str,
    file_name: &str,
    show_feedback: bool,
    cleanup_mode: &str,
    source: &std::path::Path,
    cleanup_root: Option<&std::path::Path>,
    cleanup_roots: &Arc<Mutex<HashSet<std::path::PathBuf>>>,
) {
    match cleanup_mode {
        "delete" => match fs::remove_file(source) {
            Ok(_) => {
                if let Some(root) = cleanup_root {
                    let mut guard = cleanup_roots.lock().unwrap_or_else(|e| e.into_inner());
                    guard.insert(root.to_path_buf());
                }
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

pub(crate) fn cleanup_empty_roots(cleanup_roots: &Arc<Mutex<HashSet<std::path::PathBuf>>>) {
    let roots: Vec<std::path::PathBuf> = {
        let guard = cleanup_roots.lock().unwrap_or_else(|e| e.into_inner());
        guard.iter().cloned().collect()
    };

    for root in roots {
        if !root.exists() {
            continue;
        }

        let mut dirs = walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_dir())
            .map(|entry| entry.path().to_path_buf())
            .collect::<Vec<_>>();

        dirs.sort_unstable_by_key(|dir| Reverse(dir.components().count()));

        for dir in dirs {
            match fs::remove_dir(&dir) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
                Err(_) => {}
            }
        }
    }
}
