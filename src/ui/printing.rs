//! Helper functions for printing formatted messages in the UI.

use anyhow::Result;
use colored::*;
use std::path::Path;

pub fn print_rule(ch: char) {
    println!("{}", ch.to_string().repeat(60));
}

pub fn print_source_info(source: &Path) -> Result<()> {
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

pub fn print_mapping(source: &Path, destination: &Path, added: bool) {
    let label = if added { "Added:" } else { "" };
    println!(
        "  {} {} {} → {}",
        "→".cyan().bold(),
        label,
        source
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .cyan(),
        destination.display().to_string().yellow()
    );
}
