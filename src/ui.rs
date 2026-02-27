use crate::config::UserConfig;
use anyhow::Result;
use colored::*;
use rustyline::completion::FilenameCompleter;
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{Completer, Config, Editor, Helper, Highlighter, Hinter, Validator};
use rustyline::{CompletionType, EditMode};
use std::path::{Path, PathBuf};
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

enum SourceInput {
    Retry,
    Finish,
    Path(PathBuf),
}

fn resolve_user_path(input: &str, default_base: &Path) -> PathBuf {
    let expanded = shellexpand::tilde(input);
    let path = PathBuf::from(expanded.to_string());
    if path.is_relative() {
        default_base.join(path)
    } else {
        path
    }
}

fn validate_source_path(source: &Path) -> Result<()> {
    if !source.exists() {
        anyhow::bail!("Path does not exist: {}", source.display());
    }

    if source.is_file() {
        std::fs::File::open(source)
            .map_err(|e| anyhow::anyhow!("Cannot read file (permission denied): {}", e))?;
    }

    Ok(())
}

fn print_source_info(source: &Path) -> Result<()> {
    if source.is_file() {
        let size = std::fs::metadata(source)?.len();
        println!(
            "  {} File: {} ({:.2} MB)",
            "✓".green(),
            source.display(),
            size as f64 / 1_000_000.0
        );
    } else if source.is_dir() {
        println!("  {} Directory: {}", "✓".green(), source.display());
    }

    Ok(())
}

fn print_source_destination(source: &Path, destination: &Path) {
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
}

fn print_added_source_destination(source: &Path, destination: &Path) {
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
}

fn prompt_destination_path(
    editor: &mut Editor<PathCompleter, DefaultHistory>,
    default_destination: &Path,
    show_subfolder_hint: bool,
    show_interrupt_default_hint: bool,
) -> Result<PathBuf> {
    let dest_default = default_destination.to_string_lossy().to_string();

    println!(
        "  {} Press Enter for{}: {}",
        "ℹ".cyan(),
        if show_subfolder_hint { " default" } else { "" },
        default_destination.display().to_string().yellow()
    );
    if show_subfolder_hint {
        println!(
            "  {} Or edit to specify subfolder (e.g., work/reports)",
            "ℹ".cyan()
        );
    }

    match editor.readline_with_initial("Destination path: ", (&dest_default, "")) {
        Ok(line) => {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed == dest_default {
                println!(
                    "  {} Using default: {}",
                    "✓".green(),
                    default_destination.display()
                );
                return Ok(default_destination.to_path_buf());
            }

            let final_path = resolve_user_path(trimmed, default_destination);
            if !final_path.exists() {
                std::fs::create_dir_all(&final_path)?;
                println!("  {} Created: {}", "✓".green(), final_path.display());
            } else {
                println!("  {} Using: {}", "✓".green(), final_path.display());
            }

            Ok(final_path)
        }
        Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
            if show_interrupt_default_hint {
                println!("  {} Using default", "→".cyan());
            }
            Ok(default_destination.to_path_buf())
        }
        Err(e) => Err(e.into()),
    }
}

fn prompt_mapping_source_path(
    editor: &mut Editor<PathCompleter, DefaultHistory>,
    default_source: &Path,
    has_existing_mappings: bool,
) -> Result<SourceInput> {
    let source_default = default_source.to_string_lossy().to_string();

    match editor.readline_with_initial("Source path: ", (&source_default, "")) {
        Ok(line) => {
            let trimmed = line.trim();
            if trimmed == source_default {
                if has_existing_mappings {
                    return Ok(SourceInput::Finish);
                }
                println!(
                    "{}",
                    "  ℹ  Edit the path or press Ctrl+D to cancel".yellow()
                );
                return Ok(SourceInput::Retry);
            }

            let abs_path = resolve_user_path(trimmed, default_source);
            if let Err(e) = validate_source_path(&abs_path) {
                println!("{}", format!("  ✗ {}", e).red());
                return Ok(SourceInput::Retry);
            }

            Ok(SourceInput::Path(abs_path))
        }
        Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
            if has_existing_mappings {
                Ok(SourceInput::Finish)
            } else {
                println!("\n{}", "No mappings configured. Exiting.".yellow());
                Err(anyhow::anyhow!("No files to transfer"))
            }
        }
        Err(e) => Err(e.into()),
    }
}

fn prompt_additional_source_path(
    editor: &mut Editor<PathCompleter, DefaultHistory>,
    default_source: &Path,
    files_added: usize,
) -> Result<SourceInput> {
    let source_default = default_source.to_string_lossy().to_string();

    match editor.readline_with_initial("\nSource path: ", (&source_default, "")) {
        Ok(line) => {
            let trimmed = line.trim();
            if trimmed == source_default || trimmed.is_empty() {
                if files_added == 0 {
                    println!("{}", "  ℹ No files added".yellow());
                }
                return Ok(SourceInput::Finish);
            }

            let abs_path = resolve_user_path(trimmed, default_source);
            if let Err(e) = validate_source_path(&abs_path) {
                println!("{}", format!("  ✗ {}", e).red());
                return Ok(SourceInput::Retry);
            }

            Ok(SourceInput::Path(abs_path))
        }
        Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => Ok(SourceInput::Finish),
        Err(e) => Err(e.into()),
    }
}

fn load_or_prompt_default_source(
    config: &mut UserConfig,
    editor: &mut Editor<PathCompleter, DefaultHistory>,
) -> Result<PathBuf> {
    if let Some(ref source) = config.default_source_folder {
        println!(
            "\n{} {}",
            "Default source folder:".yellow().bold(),
            source.display().to_string().cyan()
        );
        return Ok(source.clone());
    }

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
            Ok(path)
        }
        Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
            Err(anyhow::anyhow!("Cancelled by user"))
        }
        Err(e) => Err(e.into()),
    }
}

fn load_or_prompt_default_destination(
    config: &mut UserConfig,
    editor: &mut Editor<PathCompleter, DefaultHistory>,
) -> Result<PathBuf> {
    if let Some(ref dest) = config.default_destination_folder {
        println!(
            "{} {}",
            "Default destination folder:".yellow().bold(),
            dest.display().to_string().cyan()
        );
        return Ok(dest.clone());
    }

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
            Ok(path)
        }
        Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => {
            Err(anyhow::anyhow!("Cancelled by user"))
        }
        Err(e) => Err(e.into()),
    }
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

    let default_source = load_or_prompt_default_source(&mut config, &mut editor)?;
    let default_destination = load_or_prompt_default_destination(&mut config, &mut editor)?;

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
        let source =
            match prompt_mapping_source_path(&mut editor, &default_source, !mappings.is_empty())? {
                SourceInput::Retry => continue,
                SourceInput::Finish => break,
                SourceInput::Path(path) => path,
            };

        println!("\n{}", format!("Mapping #{}", mapping_number).cyan().bold());
        print_source_info(&source)?;
        let destination = prompt_destination_path(&mut editor, &default_destination, true, true)?;
        print_source_destination(&source, &destination);

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
    let max_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let suggested_workers = 2usize.min(max_workers);
    let default_workers = suggested_workers.to_string();
    let workers_prompt = format!(
        "Parallel workers (1-{}) [{}]: ",
        max_workers, suggested_workers
    );
    let num_workers = match editor.readline_with_initial(&workers_prompt, (&default_workers, "")) {
        Ok(line) => line
            .trim()
            .parse::<usize>()
            .unwrap_or(suggested_workers)
            .clamp(1, max_workers),
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
        let source = match prompt_additional_source_path(&mut editor, &default_source, files_added)?
        {
            SourceInput::Retry => continue,
            SourceInput::Finish => break,
            SourceInput::Path(path) => path,
        };

        print_source_info(&source)?;
        let destination = prompt_destination_path(&mut editor, &default_destination, false, false)?;

        // Add to queue
        if source.is_file() {
            queue.add_file(source.clone(), &destination)?;
            print_added_source_destination(&source, &destination);
            files_added += 1;
        } else if source.is_dir() {
            println!("  {} Scanning directory...", "📁".cyan());
            queue.add_directory(&source, &destination)?;
            print_added_source_destination(&source, &destination);
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
