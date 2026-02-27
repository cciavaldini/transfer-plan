use crate::config::UserConfig;
use anyhow::Result;
use colored::*;
use rustyline::completion::FilenameCompleter;
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{Completer, Config, Editor, Helper, Highlighter, Hinter, Validator};
use rustyline::{CompletionType, EditMode};
use std::path::PathBuf;
use std::sync::Arc;

pub fn print_banner() {
    println!(
        "{}",
        r#"
    ╔═══════════════════════════════════╗
    ║   TransferPlan - Rust v2.1        ║
    ║    (copy_file_range Edition)      ║
    ╚═══════════════════════════════════╝
    "#
        .cyan()
        .bold()
    );
}

#[derive(Clone)]
pub struct TransferMapping {
    pub source: PathBuf,
    pub destination: PathBuf,
}

// Custom helper that combines filename completion
#[derive(Helper, Completer, Hinter, Validator, Highlighter)]
struct PathCompleter {
    #[rustyline(Completer)]
    completer: FilenameCompleter,
}

impl PathCompleter {
    fn new() -> Self {
        Self {
            completer: FilenameCompleter::new(),
        }
    }
}

fn new_path_editor() -> Result<Editor<PathCompleter, DefaultHistory>> {
    #[cfg(unix)]
    let completion_type = CompletionType::Fuzzy;
    #[cfg(not(unix))]
    let completion_type = CompletionType::List;

    let config = Config::builder()
        .completion_type(completion_type)
        .edit_mode(EditMode::Emacs)
        .build();
    let mut editor = Editor::with_config(config)?;
    editor.set_helper(Some(PathCompleter::new()));
    Ok(editor)
}

pub fn get_transfer_mappings() -> Result<(Vec<TransferMapping>, PathBuf)> {
    // Load user configuration
    let mut config = UserConfig::load();

    // Create editor with file path completion
    let mut editor = new_path_editor()?;

    println!("\n{}", "Transfer Configuration".cyan().bold());
    println!("{}", "═".repeat(60));
    println!(
        "Use {} to open fuzzy file/folder matching",
        "Tab".green().bold()
    );
    println!("{}", "═".repeat(60));

    // Get/set default source folder
    let default_source = if let Some(ref source) = config.default_source_folder {
        println!(
            "\n{} {}",
            "Default source folder:".yellow().bold(),
            source.display().to_string().cyan()
        );
        source.clone()
    } else {
        println!("\n{}", "DEFAULT SOURCE FOLDER:".yellow().bold());
        println!("This will be used as the starting point for browsing");

        let home = std::env::var("HOME").unwrap_or_else(|_| String::from("."));
        let default_path = home.clone();

        match editor.readline_with_initial("Enter default source folder: ", (&default_path, "")) {
            Ok(line) => {
                let path = if line.trim().is_empty() {
                    PathBuf::from(default_path)
                } else {
                    PathBuf::from(shellexpand::tilde(&line).to_string())
                };

                if path.exists() {
                    println!("  {} Saved: {}", "✓".green(), path.display());
                } else {
                    println!("  {} Will use: {}", "⚠".yellow(), path.display());
                }
                config.set_default_source(path.clone())?;
                path
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                return Err(anyhow::anyhow!("Cancelled by user"));
            }
            Err(e) => {
                return Err(e.into());
            }
        }
    };

    // Get/set default destination folder
    let default_destination = if let Some(ref dest) = config.default_destination_folder {
        println!(
            "{} {}",
            "Default destination folder:".yellow().bold(),
            dest.display().to_string().cyan()
        );
        dest.clone()
    } else {
        println!(
            "\n{}",
            "DEFAULT DESTINATION FOLDER (USB drive):".yellow().bold()
        );
        println!("This is the root of your USB drive");

        let suggested = String::from("/media/usb");

        match editor.readline_with_initial("Enter USB drive path: ", (&suggested, "")) {
            Ok(line) => {
                let path = if line.trim().is_empty() {
                    PathBuf::from(suggested)
                } else {
                    PathBuf::from(shellexpand::tilde(&line).to_string())
                };

                if !path.exists() {
                    println!("  {} Creating: {}", "⚠".yellow(), path.display());
                    std::fs::create_dir_all(&path)?;
                }

                println!("  {} Saved: {}", "✓".green(), path.display());
                config.set_default_destination(path.clone())?;
                path
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                return Err(anyhow::anyhow!("Cancelled by user"));
            }
            Err(e) => {
                return Err(e.into());
            }
        }
    };

    // Now get source → destination mappings
    println!("\n{}", "═".repeat(60));
    println!("{}", "SOURCE → DESTINATION MAPPINGS:".yellow().bold());
    println!(
        "Both paths are {} with defaults",
        "pre-filled".green().bold()
    );
    println!("• {} to complete paths", "Tab".green());
    println!("• {} to use pre-filled value", "Enter".green());
    println!("• {} to finish adding files", "Ctrl+D".green());
    println!("{}", "─".repeat(60));

    let mut mappings = Vec::new();
    let mut mapping_number = 1;

    loop {
        println!("\n{}", format!("Mapping #{}", mapping_number).cyan().bold());

        // Get source path with auto-completion, pre-filled with default source
        let source_default = default_source.to_string_lossy().to_string();
        let source = match editor.readline_with_initial("Source path: ", (&source_default, "")) {
            Ok(line) => {
                let trimmed = line.trim();

                // If empty or unchanged, ask if user wants to finish
                if trimmed == source_default {
                    if mappings.is_empty() {
                        println!(
                            "{}",
                            "  ℹ  Edit the path or press Ctrl+D to cancel".yellow()
                        );
                        continue;
                    }
                    // User didn't change it, probably wants to finish
                    break;
                }

                // Expand ~ and handle relative paths
                let expanded = shellexpand::tilde(trimmed);
                let path = PathBuf::from(expanded.to_string());

                // Make absolute if relative
                let abs_path = if path.is_relative() {
                    default_source.join(&path)
                } else {
                    path
                };

                if !abs_path.exists() {
                    println!(
                        "{}",
                        format!("  ✗ Path does not exist: {}", abs_path.display()).red()
                    );
                    continue;
                }

                // Check read permissions for source files
                if abs_path.is_file() {
                    match std::fs::File::open(&abs_path) {
                        Ok(_) => {}
                        Err(e) => {
                            println!(
                                "{}",
                                format!("  ✗ Cannot read file (permission denied): {}", e).red()
                            );
                            continue;
                        }
                    }
                }

                abs_path
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                if mappings.is_empty() {
                    println!("\n{}", "No mappings configured. Exiting.".yellow());
                    return Err(anyhow::anyhow!("No files to transfer"));
                }
                break;
            }
            Err(e) => {
                return Err(e.into());
            }
        };

        // Display source info
        if source.is_file() {
            let size = std::fs::metadata(&source)?.len();
            println!(
                "  {} File: {} ({:.2} MB)",
                "✓".green(),
                source.display(),
                size as f64 / 1_000_000.0
            );
        } else if source.is_dir() {
            println!("  {} Directory: {}", "✓".green(), source.display());
        }

        // Get destination (subfolder or default) - pre-fill with default destination
        let dest_default = default_destination.to_string_lossy().to_string();
        let dest_prompt = "Destination path: ";

        println!(
            "  {} Press Enter for default: {}",
            "ℹ".cyan(),
            default_destination.display().to_string().yellow()
        );
        println!(
            "  {} Or edit to specify subfolder (e.g., work/reports)",
            "ℹ".cyan()
        );

        let destination = match editor.readline_with_initial(dest_prompt, (&dest_default, "")) {
            Ok(line) => {
                let trimmed = line.trim();

                // If unchanged or empty, use default
                if trimmed.is_empty() || trimmed == dest_default {
                    println!(
                        "  {} Using default: {}",
                        "✓".green(),
                        default_destination.display()
                    );
                    default_destination.clone()
                } else {
                    // User edited - check if it's a full path or subfolder
                    let expanded = shellexpand::tilde(trimmed);
                    let path = PathBuf::from(expanded.to_string());

                    // If absolute path, use as-is; if relative, join with default
                    let final_path = if path.is_absolute() {
                        path
                    } else {
                        default_destination.join(&path)
                    };

                    if !final_path.exists() {
                        std::fs::create_dir_all(&final_path)?;
                        println!("  {} Created: {}", "✓".green(), final_path.display());
                    } else {
                        println!("  {} Using: {}", "✓".green(), final_path.display());
                    }
                    final_path
                }
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                println!("  {} Using default", "→".cyan());
                default_destination.clone()
            }
            Err(e) => {
                return Err(e.into());
            }
        };

        println!(
            "  {} {} → {}",
            "→".cyan().bold(),
            source
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .cyan(),
            destination.display().to_string().yellow()
        );

        mappings.push(TransferMapping {
            source,
            destination,
        });
        mapping_number += 1;
    }

    println!("\n{}", "═".repeat(60));
    println!(
        "{}",
        format!("✓ {} transfer mapping(s) configured", mappings.len())
            .green()
            .bold()
    );
    println!("{}", "═".repeat(60));

    Ok((mappings, default_destination))
}

pub fn get_transfer_options() -> Result<(usize, bool, bool, String)> {
    let mut editor: Editor<(), DefaultHistory> = Editor::new()?;

    println!("\n{}", "Transfer Options:".cyan().bold());
    println!("{}", "─".repeat(60));

    // Number of workers
    let suggested_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 8);
    let default_workers = suggested_workers.to_string();
    let workers_prompt = format!("Parallel workers (1-8) [{}]: ", suggested_workers);
    let num_workers = match editor.readline_with_initial(&workers_prompt, (&default_workers, "")) {
        Ok(line) => line
            .trim()
            .parse::<usize>()
            .unwrap_or(suggested_workers)
            .clamp(1, 8),
        Err(_) => suggested_workers,
    };

    // Verification
    let verify = match editor
        .readline_with_initial("Verify with SHA-256 (files ≤10MB)? (y/N): ", ("N", ""))
    {
        Ok(line) => line.trim().eq_ignore_ascii_case("y"),
        Err(_) => false,
    };

    // Unmount
    let unmount = match editor
        .readline_with_initial("Auto-unmount USB after completion? (y/N): ", ("N", ""))
    {
        Ok(line) => {
            let response = line.trim();
            response.eq_ignore_ascii_case("y")
        }
        Err(_) => false,
    };

    // Cleanup mode after successful transfer
    // Options: none, delete
    let cleanup_mode = match editor.readline_with_initial(
        "Cleanup after successful transfer? (1.none 2.Delete) [1]: ",
        ("1", ""),
    ) {
        Ok(line) => {
            let v = line.trim().to_lowercase();
            match v.as_str() {
                "2" => "delete".to_string(),
                _ => "none".to_string(),
            }
        }
        Err(_) => "none".to_string(),
    };

    println!("{}", "─".repeat(60));
    println!(
        "{} {} workers | Verify: {} | Unmount: {} | Cleanup: {}",
        "✓".green().bold(),
        num_workers,
        if verify { "Yes" } else { "No" },
        if unmount { "Yes" } else { "No" },
        cleanup_mode
    );
    println!("{}", "═".repeat(60));

    Ok((num_workers, verify, unmount, cleanup_mode))
}

/// Add more files after a transfer batch completes
pub async fn add_more_files(
    queue: Arc<crate::queue::TransferQueue>,
    default_destination: PathBuf,
) -> Result<()> {
    use crate::config::UserConfig;

    // Load config to get default source
    let config = UserConfig::load();
    let default_source = config.get_default_source();

    // Create editor with file path completion
    let mut editor = new_path_editor()?;

    println!("\n{}", "═".repeat(60));
    println!("{}", "ADD MORE FILES".cyan().bold());
    println!("Add files and they'll be transferred in the next batch");
    println!(
        "Press {} on empty source to finish",
        "Ctrl+D or Enter".green()
    );
    println!("{}", "─".repeat(60));

    let mut files_added = 0;

    loop {
        // Get source path
        let source_default = default_source.to_string_lossy().to_string();
        let source = match editor.readline_with_initial("\nSource path: ", (&source_default, "")) {
            Ok(line) => {
                let trimmed = line.trim();

                // If unchanged or empty, user is done
                if trimmed == source_default || trimmed.is_empty() {
                    if files_added == 0 {
                        println!("{}", "  ℹ No files added".yellow());
                    }
                    break;
                }

                // Expand ~ and handle relative paths
                let expanded = shellexpand::tilde(trimmed);
                let path = PathBuf::from(expanded.to_string());

                // Make absolute if relative
                let abs_path = if path.is_relative() {
                    default_source.join(&path)
                } else {
                    path
                };

                if !abs_path.exists() {
                    println!(
                        "{}",
                        format!("  ✗ Path does not exist: {}", abs_path.display()).red()
                    );
                    continue;
                }

                // Check read permissions for source files
                if abs_path.is_file() {
                    match std::fs::File::open(&abs_path) {
                        Ok(_) => {}
                        Err(e) => {
                            println!(
                                "{}",
                                format!("  ✗ Cannot read file (permission denied): {}", e).red()
                            );
                            continue;
                        }
                    }
                }

                abs_path
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                break;
            }
            Err(e) => {
                return Err(e.into());
            }
        };

        // Display source info
        if source.is_file() {
            let size = std::fs::metadata(&source)?.len();
            println!(
                "  {} File: {} ({:.2} MB)",
                "✓".green(),
                source.display(),
                size as f64 / 1_000_000.0
            );
        } else if source.is_dir() {
            println!("  {} Directory: {}", "✓".green(), source.display());
        }

        // Get destination
        let dest_default = default_destination.to_string_lossy().to_string();
        println!(
            "  {} Press Enter for: {}",
            "ℹ".cyan(),
            default_destination.display().to_string().yellow()
        );

        let destination =
            match editor.readline_with_initial("Destination path: ", (&dest_default, "")) {
                Ok(line) => {
                    let trimmed = line.trim();

                    // If unchanged or empty, use default
                    if trimmed.is_empty() || trimmed == dest_default {
                        println!(
                            "  {} Using default: {}",
                            "✓".green(),
                            default_destination.display()
                        );
                        default_destination.clone()
                    } else {
                        // User edited - check if it's a full path or subfolder
                        let expanded = shellexpand::tilde(trimmed);
                        let path = PathBuf::from(expanded.to_string());

                        // If absolute path, use as-is; if relative, join with default
                        let final_path = if path.is_absolute() {
                            path
                        } else {
                            default_destination.join(&path)
                        };

                        if !final_path.exists() {
                            std::fs::create_dir_all(&final_path)?;
                            println!("  {} Created: {}", "✓".green(), final_path.display());
                        } else {
                            println!("  {} Using: {}", "✓".green(), final_path.display());
                        }
                        final_path
                    }
                }
                Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
                    default_destination.clone()
                }
                Err(e) => {
                    return Err(e.into());
                }
            };

        // Add to queue
        if source.is_file() {
            queue.add_file(source.clone(), &destination)?;
            println!(
                "  {} Added: {} → {}",
                "→".cyan().bold(),
                source
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .cyan(),
                destination.display().to_string().yellow()
            );
            files_added += 1;
        } else if source.is_dir() {
            println!("  {} Scanning directory...", "📁".cyan());
            queue.add_directory(&source, &destination)?;
            println!(
                "  {} Added: {} → {}",
                "→".cyan().bold(),
                source
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .cyan(),
                destination.display().to_string().yellow()
            );
            files_added += 1;
        }
    }

    if files_added > 0 {
        println!("\n{}", "═".repeat(60));
        println!(
            "{}",
            format!("✓ {} file(s)/folder(s) added to queue", files_added)
                .green()
                .bold()
        );
        println!("{}", "═".repeat(60));
    }

    Ok(())
}
