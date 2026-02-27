use anyhow::{Context, Result};
use std::fs;
#[cfg(target_os = "linux")]
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
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

/// Safe wrapper that handles errors gracefully
pub fn safe_unmount(mount_point: &Path) -> Result<()> {
    match unmount_drive(mount_point) {
        Ok(_) => {
            println!("\n🎉 Drive safely ejected! You can now remove the USB drive.");
            Ok(())
        }
        Err(e) => {
            eprintln!("\n⚠️  Warning: Failed to unmount drive automatically.");
            eprintln!("Error: {}", e);
            #[cfg(target_os = "linux")]
            if handle_busy_processes_and_retry(mount_point) {
                return Ok(());
            }
            eprintln!("Please manually eject the drive before removing it.");
            Ok(()) // Don't fail the entire program
        }
    }
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn mount_source_for_target(target: &str) -> Option<String> {
    let output = Command::new("findmnt")
        .args(["--noheadings", "--output", "SOURCE", "--target", target])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let source = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if source.starts_with("/dev/") {
        Some(source)
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
struct BusyProcess {
    pid: u32,
    user: String,
    command: String,
    args: String,
}

#[cfg(target_os = "linux")]
fn handle_busy_processes_and_retry(mount_point: &Path) -> bool {
    let processes = match detect_busy_processes(mount_point) {
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

    if !prompt_yes_no("Close these applications now? (y/N): ") {
        return false;
    }

    close_busy_processes(&processes);
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

#[cfg(target_os = "linux")]
fn detect_busy_processes(mount_point: &Path) -> std::result::Result<Vec<BusyProcess>, String> {
    let mount_point_str = match mount_point.to_str() {
        Some(value) => value,
        None => {
            return Err("Could not inspect busy processes: invalid mount point path.".to_string());
        }
    };

    let output = match Command::new("fuser").args(["-m", mount_point_str]).output() {
        Ok(output) => output,
        Err(e) => {
            return Err(format!(
                "Could not run fuser to inspect busy processes ({}). Install psmisc to enable this check.",
                e
            ));
        }
    };

    match output.status.code() {
        Some(0) => {
            let pids = parse_fuser_pids(&output.stdout);
            if pids.is_empty() {
                return Ok(vec![]);
            }

            let pid_list = pids
                .iter()
                .map(|pid| pid.to_string())
                .collect::<Vec<_>>()
                .join(",");

            let mut processes = match Command::new("ps")
                .args(["-o", "pid,user,comm,args", "-p", &pid_list])
                .output()
            {
                Ok(ps_output) if ps_output.status.success() => {
                    let details = String::from_utf8_lossy(&ps_output.stdout)
                        .trim()
                        .to_string();
                    parse_ps_processes(&details)
                }
                Ok(ps_output) => {
                    let stderr = String::from_utf8_lossy(&ps_output.stderr)
                        .trim()
                        .to_string();
                    return Err(if stderr.is_empty() {
                        format!("Mount is busy (PIDs: {}), but ps failed.", pid_list)
                    } else {
                        format!("Mount is busy (PIDs: {}), but ps failed: {}", pid_list, stderr)
                    });
                }
                Err(e) => {
                    return Err(format!(
                        "Mount is busy (PIDs: {}), but failed to run ps: {}",
                        pid_list, e
                    ));
                }
            };

            if processes.is_empty() {
                processes = pids
                    .into_iter()
                    .map(|pid| BusyProcess {
                        pid,
                        user: "?".to_string(),
                        command: "unknown".to_string(),
                        args: String::new(),
                    })
                    .collect();
            }
            Ok(processes)
        }
        Some(1) => {
            Ok(vec![])
        }
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !stderr.is_empty() {
                Err(format!("Could not inspect busy processes with fuser: {}", stderr))
            } else if !stdout.is_empty() {
                Err(format!("Could not inspect busy processes with fuser: {}", stdout))
            } else {
                Err(format!(
                    "Could not inspect busy processes with fuser: {}",
                    output.status
                ))
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn parse_fuser_pids(stdout: &[u8]) -> Vec<u32> {
    let mut pids = String::from_utf8_lossy(stdout)
        .split_whitespace()
        .filter_map(parse_fuser_pid_token)
        .collect::<Vec<_>>();

    pids.sort_unstable();
    pids.dedup();
    pids
}

#[cfg(target_os = "linux")]
fn parse_ps_processes(output: &str) -> Vec<BusyProcess> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse::<u32>().ok()?;
            let user = parts.next()?.to_string();
            let command = parts.next()?.to_string();
            let args = parts.collect::<Vec<_>>().join(" ");
            Some(BusyProcess {
                pid,
                user,
                command,
                args,
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn parse_fuser_pid_token(token: &str) -> Option<u32> {
    let pid_digits = token
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if pid_digits.is_empty() {
        return None;
    }
    pid_digits.parse::<u32>().ok()
}

#[cfg(target_os = "linux")]
fn prompt_yes_no(prompt: &str) -> bool {
    print!("{}", prompt);
    let _ = io::stdout().flush();

    let mut response = String::new();
    if io::stdin().read_line(&mut response).is_err() {
        return false;
    }

    matches!(
        response.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "o" | "oui"
    )
}

#[cfg(target_os = "linux")]
fn close_busy_processes(processes: &[BusyProcess]) {
    let mut nautilus_quit_attempted = false;
    let mut nautilus_quit_succeeded = false;

    for process in processes {
        if process.pid == std::process::id() {
            continue;
        }

        if process.command == "nautilus" {
            if !nautilus_quit_attempted {
                nautilus_quit_attempted = true;
                match Command::new("nautilus").arg("-q").status() {
                    Ok(status) if status.success() => {
                        nautilus_quit_succeeded = true;
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    Ok(status) => {
                        eprintln!("nautilus -q failed (status: {}), killing Nautilus...", status);
                    }
                    Err(e) => {
                        eprintln!("nautilus -q failed ({}), killing Nautilus...", e);
                    }
                }
            }

            if !nautilus_quit_succeeded || process_is_alive(process.pid) {
                terminate_process(process.pid, &process.command);
            }
        } else {
            terminate_process(process.pid, &process.command);
        }
    }
}

#[cfg(target_os = "linux")]
fn terminate_process(pid: u32, command: &str) {
    let pid_str = pid.to_string();
    match Command::new("kill").args(["-TERM", &pid_str]).status() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            eprintln!(
                "Failed to stop {} (pid {}) with SIGTERM: {}",
                command, pid, status
            );
            return;
        }
        Err(e) => {
            eprintln!(
                "Failed to stop {} (pid {}) with SIGTERM: {}",
                command, pid, e
            );
            return;
        }
    }

    std::thread::sleep(Duration::from_millis(400));
    if process_is_alive(pid) {
        let _ = Command::new("kill").args(["-KILL", &pid_str]).status();
    }
}

#[cfg(target_os = "linux")]
fn process_is_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
