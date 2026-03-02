use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use procfs::process::Process;

use crate::unmount::busy;

/// Attempt to unmount a drive at `mount_point`. On Linux this performs a
/// `sync` call, tries various unmount strategies and reports any failure.
pub fn unmount_drive(mount_point: &Path) -> Result<()> {
    use std::ffi::CString;

    println!("Syncing filesystem...");
    // Ensure all data is written
    unsafe {
        libc::sync();
    }

    println!("Unmounting {}...", mount_point.display());

    let mount_point_str = mount_point.to_str().context("Invalid mount point path")?;

    if !is_mount_point(mount_point_str) {
        println!(
            "ℹ {} is not currently mounted, skipping.",
            mount_point.display()
        );
        return Ok(());
    }

    let mut attempts = Vec::new();

    // Desktop environments often mount removable drives through udisks.
    // Try that path first because it usually works without root privileges.
    if let Some(device) = mount_source_for_target(mount_point_str) {
        match run_command("udisksctl", &["unmount", "--block-device", &device]) {
            Ok(_) => {
                println!("✓ Successfully unmounted {}", mount_point.display());
                return Ok(());
            }
            Err(e) => attempts.push(e),
        }
    }

    // Fallback: standard umount command
    match run_command("umount", &[mount_point_str]) {
        Ok(_) => {
            println!("✓ Successfully unmounted {}", mount_point.display());
            return Ok(());
        }
        Err(e) => attempts.push(e),
    }

    // Last fallback: direct syscall
    let c_path = CString::new(mount_point_str)?;
    let result = unsafe { libc::umount(c_path.as_ptr()) };
    if result == 0 {
        println!("✓ Successfully unmounted {}", mount_point.display());
        return Ok(());
    }

    let error = std::io::Error::last_os_error();
    attempts.push(format!("libc::umount: {}", error));
    anyhow::bail!(
        "Failed to unmount {}. Attempts: {}",
        mount_point.display(),
        attempts.join(" | ")
    );
}

/// Safe wrapper that trims the error to a warning and optionally attempts to close
/// busy processes before giving up.
pub fn safe_unmount(mount_point: &Path) -> Result<()> {
    match unmount_drive(mount_point) {
        Ok(_) => {
            println!("\n🎉 Drive safely ejected! You can now remove the USB drive.");
            Ok(())
        }
        Err(e) => {
            eprintln!("\n⚠️  Warning: Failed to unmount drive automatically.");
            eprintln!("Error: {}", e);
            if handle_busy_processes_and_retry(mount_point) {
                return Ok(());
            }
            eprintln!("Please manually eject the drive before removing it.");
            Ok(()) // Don't fail the entire program
        }
    }
}

/// If the unmount fails, show any processes keeping the mount busy and optionally
/// close them. Returns `true` if the drive was successfully unmounted on retry.
fn handle_busy_processes_and_retry(mount_point: &Path) -> bool {
    let processes = match busy::detect_busy_processes(mount_point) {
        Ok(processes) => processes,
        Err(e) => {
            eprintln!("{}", e);
            return false;
        }
    };

    if processes.is_empty() {
        eprintln!(
            "No process is currently reported as using {}.",
            mount_point.display()
        );
        return false;
    }

    eprintln!("Processes currently using {}:", mount_point.display());
    for process in &processes {
        if process.args.is_empty() {
            eprintln!(
                "  pid={} user={} cmd={}",
                process.pid, process.user, process.command
            );
        } else {
            eprintln!(
                "  pid={} user={} cmd={} args={}",
                process.pid, process.user, process.command, process.args
            );
        }
    }

    if !busy::prompt_yes_no("Close these applications now? (y/N): ") {
        return false;
    }

    busy::close_busy_processes(&processes);
    std::thread::sleep(Duration::from_millis(700));

    match unmount_drive(mount_point) {
        Ok(_) => {
            println!("\n🎉 Drive safely ejected! You can now remove the USB drive.");
            true
        }
        Err(retry_error) => {
            eprintln!(
                "Unmount retry after closing applications failed: {}",
                retry_error
            );
            false
        }
    }
}

/// Run an external command and capture output as an error string on failure.
/// 
/// Note: This is used for user-friendly unmount attempts (udisksctl, umount). 
/// Core functionality falls back to direct `libc::umount` syscall if these fail,
/// so the program never strictly depends on these external commands.
/// 
/// Future improvements could replace some of these:
/// - udisksctl: Would need dbus crate (complex, adds dependency)
/// - umount: Already have libc::umount fallback (sufficient)
fn run_command(program: &str, args: &[&str]) -> std::result::Result<(), String> {
    match Command::new(program).args(args).output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let details = if !stderr.is_empty() {
                stderr
            } else if !stdout.is_empty() {
                stdout
            } else {
                format!("exit status {}", output.status)
            };
            Err(format!("{} {}: {}", program, args.join(" "), details))
        }
        Err(e) => Err(format!("{} {}: {}", program, args.join(" "), e)),
    }
}

/// Determine the backing source device for a mount point by reading
/// mountinfo from `/proc` via the `procfs` crate.
fn mount_source_for_target(target: &str) -> Option<String> {
    let myself = Process::myself().ok()?;
    let mounts = myself.mountinfo().ok()?;
    for m in mounts {
        if m.mount_point == PathBuf::from(target) {
            if let Some(src) = m.mount_source {
                if src.starts_with("/dev/") {
                    return Some(src);
                }
            }
        }
    }
    None
}

/// Check `/proc/mounts` to see whether the given path is a mount point.
fn is_mount_point(mount_point: &str) -> bool {
    fs::read_to_string("/proc/mounts")
        .map(|content| {
            content.lines().any(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                parts.len() >= 2 && parts[1] == mount_point
            })
        })
        .unwrap_or(false)
}
