use anyhow::Result;
use colored::*;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::UserConfig;
use crate::queue::TransferQueue;
use crate::ui::editor::PathCompleter;
use crate::ui::editor::new_path_editor;
use crate::ui::printing::{print_mapping, print_rule, print_source_info};
use crate::ui::prompts::{prompt_additional_source_path, prompt_destination_path, prompt_mapping_source_path, load_or_prompt_default_destination, load_or_prompt_default_source, SourceInput};
use crate::ui::types::TransferMapping;
use rustyline::history::DefaultHistory;
use rustyline::Editor;

pub fn get_transfer_mappings() -> Result<(Vec<TransferMapping>, PathBuf)> {
    // Load user configuration
    let mut config = UserConfig::load();

    // Create editor with file path completion
    let mut editor = new_path_editor()?;

    println!("\n{}", "Transfer Configuration".cyan().bold());
    print_rule('═');
    println!(
        "Use {} to open fuzzy file/folder matching",
        "Tab".green().bold()
    );
    print_rule('═');

    let default_source = load_or_prompt_default_source(&mut config, &mut editor)?;
    let default_destination = load_or_prompt_default_destination(&mut config, &mut editor)?;

    // Now get source → destination mappings
    print_rule('═');
    println!("{}", "SOURCE → DESTINATION MAPPINGS:".yellow().bold());
    println!(
        "Both paths are {} with defaults",
        "pre-filled".green().bold()
    );
    println!("• {} to complete paths", "Tab".green());
    println!("• {} to use pre-filled value", "Enter".green());
    println!("• {} to finish adding files", "Ctrl+D".green());
    print_rule('─');

    let mut mappings = Vec::new();
    let mut mapping_number = 1;

    loop {
        let source = match prompt_mapping_source_path(&mut editor, &default_source, !mappings.is_empty())? {
            SourceInput::Retry => continue,
            SourceInput::Finish => break,
            SourceInput::Path(path) => path,
        };

        println!("\n{}", format!("Mapping #{}", mapping_number).cyan().bold());
        print_source_info(&source)?;
        let destination = prompt_destination_path(&mut editor, &default_destination, true, true)?;
        print_mapping(&source, &destination, false);

        mappings.push(TransferMapping { source, destination });
        mapping_number += 1;
    }

    print_rule('═');
    println!(
        "{}",
        format!("✓ {} transfer mapping(s) configured", mappings.len())
            .green()
            .bold()
    );
    print_rule('═');

    Ok((mappings, default_destination))
}

pub fn get_transfer_options() -> Result<(usize, bool, bool, String, bool)> {
    let mut editor: Editor<PathCompleter, DefaultHistory> = new_path_editor()?;

    println!("\n{}", "Transfer Options:".cyan().bold());
    print_rule('─');

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

    // Update mode (skip unchanged destination files)
    let update_mode = match editor
        .readline_with_initial("Skip unchanged files (update mode)? (Y/n): ", ("Y", ""))
    {
        Ok(line) => {
            let response = line.trim();
            response.is_empty() || response.eq_ignore_ascii_case("y")
        }
        Err(_) => true,
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

    print_rule('─');
    println!(
        "{} {} workers | Verify: {} | Update: {} | Unmount: {} | Cleanup: {}",
        "✓".green().bold(),
        num_workers,
        if verify { "Yes" } else { "No" },
        if update_mode { "Yes" } else { "No" },
        if unmount { "Yes" } else { "No" },
        cleanup_mode
    );
    print_rule('═');

    Ok((num_workers, verify, unmount, cleanup_mode, update_mode))
}

/// Add more files after a transfer batch completes
pub async fn add_more_files(
    queue: Arc<TransferQueue>,
    default_destination: PathBuf,
    update_mode: bool,
) -> Result<()> {
    use crate::config::UserConfig;

    // Load config to get default source
    let config = UserConfig::load();
    let default_source = config.get_default_source();

    // Create editor with file path completion
    let mut editor = new_path_editor()?;

    print_rule('═');
    println!("{}", "ADD MORE FILES".cyan().bold());
    println!("Add files and they'll be transferred in the next batch");
    println!(
        "Press {} on empty source to finish",
        "Ctrl+D or Enter".green()
    );
    print_rule('─');

    let mut files_added = 0;

    loop {
        let source = match prompt_additional_source_path(&mut editor, &default_source, files_added)? {
            SourceInput::Retry => continue,
            SourceInput::Finish => break,
            SourceInput::Path(path) => path,
        };

        print_source_info(&source)?;
        let destination = prompt_destination_path(&mut editor, &default_destination, false, false)?;

        // Add to queue
        if source.is_file() {
            let summary = queue.add_file_with_policy(source.clone(), &destination, update_mode)?;
            if summary.queued_files > 0 {
                print_mapping(&source, &destination, true);
                files_added += 1;
            } else if summary.skipped_files > 0 {
                println!("  {} Skipped unchanged: {}", "↷".yellow(), source.display());
            }
        } else if source.is_dir() {
            println!("  {} Scanning directory...", "📁".cyan());
            let summary = queue.add_directory_with_policy(&source, &destination, update_mode)?;
            if summary.queued_files > 0 {
                print_mapping(&source, &destination, true);
                files_added += 1;
            }
            if summary.skipped_files > 0 {
                println!(
                    "  {} Skipped unchanged files: {}",
                    "↷".yellow(),
                    summary.skipped_files
                );
            }
        }
    }

    if files_added > 0 {
        print_rule('═');
        println!(
            "{}",
            format!("✓ {} file(s)/folder(s) added to queue", files_added)
                .green()
                .bold()
        );
        print_rule('═');
    }

    Ok(())
}
