use anyhow::{Context, Result};
use colored::Colorize;
use procfs::process::Process;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::queue::{TransferItem, TransferQueue};
use crate::ui::TransferMapping;

#[derive(Debug, Clone)]
struct MountEntry {
    mount_point: PathBuf,
    source: String,
}

#[derive(Debug)]
struct MountedDestinationLink {
    mount_point: PathBuf,
    usb_speed_mbps: Option<u32>,
}

pub(crate) fn watch_paths_for_user(user: Option<&str>) -> Vec<PathBuf> {
    let mut watch_paths = vec![Path::new("/media").to_path_buf()];
    if let Some(user) = user {
        watch_paths.push(Path::new("/run/media").join(user));
    }
    watch_paths
}

fn parse_mount_entries() -> Result<Vec<MountEntry>> {
    let mounts = Process::myself()
        .context("Failed to inspect current process for mount parsing")?
        .mountinfo()
        .context("Failed to read mount table via procfs")?;

    Ok(mounts
        .into_iter()
        .map(|mount| MountEntry {
            mount_point: mount.mount_point,
            source: mount.mount_source.unwrap_or_default(),
        })
        .collect())
}

fn root_block_device_name_from_source(source: &str) -> Option<String> {
    if !source.starts_with("/dev/") {
        return None;
    }

    let canonical = fs::canonicalize(source).unwrap_or_else(|_| PathBuf::from(source));
    let device_name = canonical.file_name()?.to_string_lossy();
    Some(
        device_name
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .trim_end_matches('p')
            .to_string(),
    )
}

fn usb_bus_dev_for_source(source: &str) -> Option<(u32, u32)> {
    let root_device_name = root_block_device_name_from_source(source)?;
    let sys_block = Path::new("/sys/class/block").join(root_device_name);
    let canonical_sys_block = fs::canonicalize(sys_block).ok()?;

    for ancestor in canonical_sys_block.ancestors() {
        let busnum_path = ancestor.join("busnum");
        let devnum_path = ancestor.join("devnum");
        let vendor_path = ancestor.join("idVendor");
        if busnum_path.is_file() && devnum_path.is_file() && vendor_path.is_file() {
            let busnum = fs::read_to_string(busnum_path)
                .ok()?
                .trim()
                .parse::<u32>()
                .ok()?;
            let devnum = fs::read_to_string(devnum_path)
                .ok()?
                .trim()
                .parse::<u32>()
                .ok()?;
            return Some((busnum, devnum));
        }
    }

    None
}

fn speed_from_sysfs_for_source(source: &str) -> Option<u32> {
    let root_device_name = root_block_device_name_from_source(source)?;
    let sys_block = Path::new("/sys/class/block").join(root_device_name);
    let canonical_sys_block = fs::canonicalize(sys_block).ok()?;

    for ancestor in canonical_sys_block.ancestors() {
        let speed_path = ancestor.join("speed");
        let vendor_path = ancestor.join("idVendor");
        if speed_path.is_file() && vendor_path.is_file() {
            let speed = fs::read_to_string(speed_path).ok()?;
            let parsed_speed = speed.trim().parse::<f64>().ok()?;
            return Some(parsed_speed.round() as u32);
        }
    }

    None
}

fn speed_mbps_from_cyme_device(device: &cyme::profiler::Device) -> Option<u32> {
    let speed = device.device_speed.as_ref()?;
    let speed = match speed {
        cyme::profiler::DeviceSpeed::SpeedValue(value) => value.to_lsusb_speed(),
        cyme::profiler::DeviceSpeed::Description(_) => return None,
    };
    let mbps = speed.strip_suffix('M')?.trim().parse::<u32>().ok()?;
    (mbps > 0).then_some(mbps)
}

fn build_cyme_speed_index() -> HashMap<(u32, u32), u32> {
    let mut speeds = HashMap::new();
    let Ok(spusb) = cyme::profiler::get_spusb() else {
        return speeds;
    };

    for device in spusb.flattened_devices() {
        let Some(speed_mbps) = speed_mbps_from_cyme_device(device) else {
            continue;
        };
        speeds
            .entry((
                u32::from(device.location_id.bus),
                u32::from(device.location_id.number),
            ))
            .or_insert(speed_mbps);
    }

    speeds
}

fn collect_mounted_destination_links(
    items: &[TransferItem],
) -> Result<Vec<MountedDestinationLink>> {
    let mount_entries = parse_mount_entries()?;
    let cyme_speeds = build_cyme_speed_index();
    let mut by_mount: HashMap<PathBuf, Option<u32>> = HashMap::new();

    for item in items {
        let best_match = mount_entries
            .iter()
            .filter(|entry| item.destination.starts_with(&entry.mount_point))
            .max_by_key(|entry| entry.mount_point.components().count());

        if let Some(entry) = best_match {
            let Some((busnum, devnum)) = usb_bus_dev_for_source(&entry.source) else {
                continue;
            };
            by_mount
                .entry(entry.mount_point.clone())
                .or_insert_with(|| {
                    cyme_speeds
                        .get(&(busnum, devnum))
                        .copied()
                        .or_else(|| speed_from_sysfs_for_source(&entry.source))
                });
        }
    }

    let mut links: Vec<MountedDestinationLink> = by_mount
        .into_iter()
        .map(|(mount_point, usb_speed_mbps)| MountedDestinationLink {
            mount_point,
            usb_speed_mbps,
        })
        .collect();
    links.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    Ok(links)
}

fn format_usb_speed_label(speed_mbps: u32) -> String {
    match speed_mbps {
        12 => "12M (USB 1.1 Full-Speed)".to_string(),
        480 => "480M (USB 2.0 High-Speed)".to_string(),
        5_000 => "5000M (USB 3.2 Gen 1 / USB 3.0)".to_string(),
        10_000 => "10000M (USB 3.2 Gen 2 / USB 3.1 Gen 2)".to_string(),
        20_000 => "20000M (USB 3.2 Gen 2x2)".to_string(),
        _ => format!("{speed_mbps}M (USB standard unknown)"),
    }
}

pub(crate) fn print_mounted_destination_speeds(items: &[TransferItem]) {
    match collect_mounted_destination_links(items) {
        Ok(links) if !links.is_empty() => {
            println!("\n{}", "Mounted destination link speeds:".cyan().bold());
            for link in links {
                let speed_display = link
                    .usb_speed_mbps
                    .map(format_usb_speed_label)
                    .unwrap_or_else(|| "USB speed unknown".to_string());
                println!(
                    "  {} {} [{}] {}",
                    "•".cyan(),
                    link.mount_point.display(),
                    "Mounted".green(),
                    speed_display
                );
            }
        }
        Ok(_) => {}
        Err(e) => {
            eprintln!(
                "{} Could not inspect mounted destination link speeds: {}",
                "Warning:".yellow(),
                e
            );
        }
    }
}

fn mounts_contains_path(content: &str, mount_point: &Path) -> bool {
    let mount_point = mount_point.to_string_lossy();
    content.lines().any(|line| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        parts.len() >= 2 && parts[1] == mount_point
    })
}

pub(crate) fn is_mount_point(p: &Path) -> bool {
    std::fs::read_to_string("/proc/mounts")
        .map(|content| mounts_contains_path(&content, p))
        .unwrap_or(false)
}

pub(crate) fn auto_queue_mappings_for_mount(
    queue: &Arc<TransferQueue>,
    mappings: &[TransferMapping],
    default_destination: &Path,
    mount_path: &Path,
    update_mode: bool,
) {
    if !mount_path.is_dir() || !is_mount_point(mount_path) {
        return;
    }

    println!(
        "\n{} Detected device mounted at {}",
        "ℹ".cyan(),
        mount_path.display()
    );

    // For each mapping that targeted the default_destination,
    // translate to the newly-mounted path and add to queue
    for mapping in mappings {
        // Only auto-queue mappings that used the generic default destination
        if !mapping.destination.starts_with(default_destination) {
            continue;
        }

        // Compute relative suffix after default_destination.
        let rel = mapping
            .destination
            .strip_prefix(default_destination)
            .unwrap_or_else(|_| Path::new(""));
        let new_dest = mount_path.join(rel);

        if mapping.source.is_file() {
            match queue.add_file_with_policy(mapping.source.clone(), &new_dest, update_mode) {
                Ok(summary) if summary.queued_files > 0 => {
                    println!(
                        "  {} Auto-queued: {} → {}",
                        "→".cyan().bold(),
                        mapping.source.display(),
                        new_dest.display()
                    );
                }
                Ok(summary) if summary.skipped_files > 0 => {
                    println!(
                        "  {} Skipped unchanged: {}",
                        "↷".yellow(),
                        mapping.source.display()
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!(
                        "Failed to add file {} -> {}: {}",
                        mapping.source.display(),
                        new_dest.display(),
                        e
                    );
                }
            }
        } else if mapping.source.is_dir() {
            match queue.add_directory_with_policy(&mapping.source, &new_dest, update_mode) {
                Ok(summary) => {
                    if summary.queued_files > 0 {
                        println!(
                            "  {} Auto-queued directory: {} → {} ({} files)",
                            "→".cyan().bold(),
                            mapping.source.display(),
                            new_dest.display(),
                            summary.queued_files
                        );
                    }
                    if summary.skipped_files > 0 {
                        println!(
                            "  {} Skipped unchanged files in directory: {}",
                            "↷".yellow(),
                            summary.skipped_files
                        );
                    }
                }
                Err(e) => {
                    eprintln!(
                        "Failed to add directory {} -> {}: {}",
                        mapping.source.display(),
                        new_dest.display(),
                        e
                    );
                }
            };
        }
    }
}

pub(crate) fn mounted_paths_in_watch_roots(watch_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut mounts = Vec::new();

    for root in watch_paths {
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && is_mount_point(&path) {
                mounts.push(path);
            }
        }
    }

    mounts
}

pub(crate) fn watch_mounts_for_queueing(
    queue: Arc<TransferQueue>,
    mappings: Vec<TransferMapping>,
    default_destination: PathBuf,
    update_mode: bool,
) {
    let user = std::env::var("USER").ok();
    let watch_paths = watch_paths_for_user(user.as_deref());

    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            let _ = tx.send(res);
        })
        .expect("Failed to create file watcher for mount monitoring");

    for p in &watch_paths {
        let _ = notify::Watcher::watch(&mut watcher, p, notify::RecursiveMode::NonRecursive);
    }

    let mut seen_mount_paths = HashSet::new();

    // Catch devices that are already mounted before watcher starts.
    for path in mounted_paths_in_watch_roots(&watch_paths) {
        if seen_mount_paths.insert(path.clone()) {
            auto_queue_mappings_for_mount(
                &queue,
                &mappings,
                &default_destination,
                &path,
                update_mode,
            );
        }
    }

    for event in rx.iter().flatten() {
        if matches!(
            event.kind,
            notify::EventKind::Create(_) | notify::EventKind::Modify(_)
        ) {
            for path in event.paths {
                if path.is_dir() && is_mount_point(&path) && seen_mount_paths.insert(path.clone()) {
                    auto_queue_mappings_for_mount(
                        &queue,
                        &mappings,
                        &default_destination,
                        &path,
                        update_mode,
                    );
                }
            }
        }

        // Also rescan watch roots to catch mounts on pre-existing directories.
        for path in mounted_paths_in_watch_roots(&watch_paths) {
            if seen_mount_paths.insert(path.clone()) {
                auto_queue_mappings_for_mount(
                    &queue,
                    &mappings,
                    &default_destination,
                    &path,
                    update_mode,
                );
            }
        }
    }
}
