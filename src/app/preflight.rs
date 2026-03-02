//! Preflight validation routines executed before beginning transfers.

use anyhow::{Context, Result};
use colored::Colorize;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::UserConfig;
use crate::queue::TransferItem;
use crate::transfer::format_size;

pub(crate) fn print_config_validation(config: &UserConfig) {
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

fn ensure_parent_writable(parent: &Path) -> Result<()> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let probe_path = parent.join(format!(
        ".transferplan-preflight-{}-{}",
        std::process::id(),
        nonce
    ));

    let mut probe = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe_path)
        .with_context(|| format!("Destination not writable: {}", parent.display()))?;
    probe.flush()?;
    drop(probe);
    let _ = fs::remove_file(&probe_path);
    Ok(())
}

#[cfg(target_family = "unix")]
fn filesystem_space_info(path: &Path) -> Result<(u64, u64)> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("Invalid path for filesystem check: {}", path.display()))?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Failed to read filesystem stats for {}", path.display()));
    }

    let fs_id = stat.f_fsid as u64;
    let available = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
    Ok((fs_id, available))
}

pub(crate) fn preflight_pending_destinations(items: &[TransferItem]) -> Result<()> {
    let mut required_by_parent: HashMap<PathBuf, u64> = HashMap::new();
    #[cfg(target_family = "unix")]
    let mut required_by_fs: HashMap<u64, (PathBuf, u64, u64)> = HashMap::new();

    for item in items {
        if item.destination.file_name().is_none() {
            anyhow::bail!("Invalid destination path: {}", item.destination.display());
        }

        if item.destination.exists() && item.destination.is_dir() {
            anyhow::bail!(
                "Destination path points to a directory, expected a file path: {}",
                item.destination.display()
            );
        }

        let parent = item
            .destination
            .parent()
            .context("Destination has no parent directory")?
            .to_path_buf();
        fs::create_dir_all(&parent).with_context(|| {
            format!(
                "Failed to prepare destination directory {}",
                parent.display()
            )
        })?;
        *required_by_parent.entry(parent).or_insert(0) += item.size;
    }

    for (parent, required_bytes) in required_by_parent {
        ensure_parent_writable(&parent)?;

        #[cfg(target_family = "unix")]
        {
            let (fs_id, available) = filesystem_space_info(&parent)?;
            let entry = required_by_fs
                .entry(fs_id)
                .or_insert_with(|| (parent.clone(), 0, available));
            entry.1 = entry.1.saturating_add(required_bytes);
            if entry.2 < entry.1 {
                anyhow::bail!(
                    "Insufficient free space on filesystem for {}: need {}, available {}",
                    entry.0.display(),
                    format_size(entry.1),
                    format_size(entry.2)
                );
            }
        }
    }

    Ok(())
}
