//! Helpers for detecting and handling processes keeping a mount point busy.
//! Uses `/proc` rather than external `fuser`/`ps`.

use std::io::{self, Write};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use procfs::process::FDTarget;

#[derive(Debug, Clone)]
pub struct BusyProcess {
    pub pid: u32,
    pub user: String,
    pub command: String,
    pub args: String,
}

/// Detect any processes holding open handles to the mount point using procfs.
/// This replaces the previous approach of calling external `fuser` and `ps` commands.
pub fn detect_busy_processes(mount_point: &Path) -> std::result::Result<Vec<BusyProcess>, String> {
    let mut results = Vec::new();

    let all = procfs::process::all_processes()
        .map_err(|e| format!("Failed to list processes: {}", e))?;

    for proc_res in all {
        let process = match proc_res {
            Ok(p) => p,
            Err(_) => continue,
        };

        // skip ourselves
        if process.pid as u32 == std::process::id() {
            continue;
        }

        // examine open file descriptors for anything under the mount point
        if let Ok(fds) = process.fd() {
            let mut interested = false;
            for fd in fds {
                if let Ok(fd) = fd {
                    if let FDTarget::Path(ref path) = fd.target {
                        if path.starts_with(mount_point) {
                            interested = true;
                            break;
                        }
                    }
                }
            }
            if !interested {
                continue;
            }
        } else {
            continue;
        }

        // gather metadata via procfs
        let command = process
            .stat()
            .map(|s| s.comm.clone())
            .unwrap_or_default();

        let args = process.cmdline().unwrap_or_default().join(" ");
        let uid = process
            .status()
            .map(|s| s.ruid)
            .unwrap_or(0);
        let user = uid.to_string();

        results.push(BusyProcess {
            pid: process.pid as u32,
            user,
            command,
            args,
        });
    }

    Ok(results)
}

pub fn prompt_yes_no(prompt: &str) -> bool {
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

pub fn close_busy_processes(processes: &[BusyProcess]) {
    let mut nautilus_quit_attempted = false;
    let mut nautilus_quit_succeeded = false;

    for process in processes {
        if process.pid == std::process::id() {
            continue;
        }

        if process.command == "nautilus" {
            if !nautilus_quit_attempted {
                nautilus_quit_attempted = true;
                // Try graceful quit first - using Command::new here is acceptable
                // since Nautilus is a GUI app with a -q (quit) option
                match Command::new("nautilus").arg("-q").status() {
                    Ok(status) if status.success() => {
                        nautilus_quit_succeeded = true;
                        std::thread::sleep(Duration::from_millis(500));
                    }
                    Ok(status) => {
                        eprintln!(
                            "nautilus -q failed (status: {}), killing Nautilus...",
                            status
                        );
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

/// Terminate a process using libc signal calls instead of external `kill` command.
fn terminate_process(pid: u32, command: &str) {
    let pid_c = pid as libc::pid_t;
    unsafe {
        if libc::kill(pid_c, libc::SIGTERM) != 0 {
            let e = std::io::Error::last_os_error();
            eprintln!(
                "Failed to stop {} (pid {}) with SIGTERM: {}",
                command, pid, e
            );
            return;
        }
    }

    std::thread::sleep(Duration::from_millis(400));
    if process_is_alive(pid) {
        let _ = unsafe { libc::kill(pid_c, libc::SIGKILL) };
    }
}

/// Check if a process is alive using libc::kill with signal 0 (no-op).
/// Replacements: was `kill -0 <pid>` command, now using libc signal(0) syscall.
fn process_is_alive(pid: u32) -> bool {
    let pid_c = pid as libc::pid_t;
    unsafe { libc::kill(pid_c, 0) == 0 }
}