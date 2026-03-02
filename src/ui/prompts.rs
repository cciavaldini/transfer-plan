use anyhow::Result;
use colored::*;
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::Editor;
use std::path::{Path, PathBuf};

use crate::config::UserConfig;
use crate::ui::editor::{PathCompleter, resolve_user_path, validate_source_path};

pub enum SourceInput {
    Retry,
    Finish,
    Path(PathBuf),
}

pub fn prompt_destination_path(
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

pub fn prompt_mapping_source_path(
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

pub fn prompt_additional_source_path(
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

pub fn load_or_prompt_default_source(
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

pub fn load_or_prompt_default_destination(
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

    println!("\n{}", "DEFAULT DESTINATION FOLDER (USB drive):".yellow().bold());
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
