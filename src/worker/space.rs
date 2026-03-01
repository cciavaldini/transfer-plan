use crate::transfer::format_size;
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const LOW_SPACE_GUARD_MARGIN_BYTES: u64 = 64 * 1024 * 1024;
const SPACE_RECHECK_BYTES: u64 = 512 * 1024 * 1024;
const SPACE_RECHECK_INTERVAL_MS: u64 = 1500;

#[cfg(target_family = "unix")]
fn filesystem_id(path: &std::path::Path) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;

    let metadata =
        fs::metadata(path).with_context(|| format!("Failed to inspect {}", path.display()))?;
    Ok(metadata.dev())
}

#[cfg(target_family = "unix")]
fn filesystem_available_bytes(path: &std::path::Path) -> Result<u64> {
    use std::os::fd::AsRawFd;

    let handle =
        fs::File::open(path).with_context(|| format!("Failed to inspect {}", path.display()))?;

    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::fstatvfs(handle.as_raw_fd(), &mut stat) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Failed to read free space for {}", path.display()));
    }

    Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

#[derive(Clone, Copy)]
pub(crate) struct ReservedSpace {
    fs_id: u64,
    bytes: u64,
}

#[cfg(target_family = "unix")]
#[derive(Clone, Copy)]
pub(crate) struct SpaceProbeCacheEntry {
    available: u64,
    bytes_until_recheck: u64,
    last_probe: Instant,
}

#[cfg(target_family = "unix")]
pub(crate) type SpaceProbeCache = HashMap<u64, SpaceProbeCacheEntry>;
#[cfg(not(target_family = "unix"))]
pub(crate) type SpaceProbeCache = HashMap<u64, ()>;

pub(crate) fn ensure_destination_parent_cached(
    destination: &std::path::Path,
    prepared_parents: &mut HashSet<std::path::PathBuf>,
) -> Result<()> {
    let parent = destination
        .parent()
        .context("Destination has no parent directory")?;
    if prepared_parents.insert(parent.to_path_buf()) {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to prepare destination directory {}",
                parent.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(target_family = "unix")]
pub(crate) fn reserve_destination_space(
    destination: &std::path::Path,
    bytes: u64,
    reservations: &Arc<Mutex<HashMap<u64, u64>>>,
    space_probe_cache: &mut SpaceProbeCache,
) -> Result<Option<ReservedSpace>> {
    let parent = destination
        .parent()
        .context("Destination has no parent directory")?;
    let fs_id = filesystem_id(parent)?;

    let should_refresh_probe = match space_probe_cache.get(&fs_id) {
        Some(entry) => {
            entry.bytes_until_recheck < bytes
                || entry.last_probe.elapsed() >= Duration::from_millis(SPACE_RECHECK_INTERVAL_MS)
        }
        None => true,
    };

    if should_refresh_probe {
        let available = filesystem_available_bytes(parent)?;
        space_probe_cache.insert(
            fs_id,
            SpaceProbeCacheEntry {
                available,
                bytes_until_recheck: SPACE_RECHECK_BYTES,
                last_probe: Instant::now(),
            },
        );
    }

    let mut guard = reservations.lock().unwrap_or_else(|e| e.into_inner());
    let reserved = guard.get(&fs_id).copied().unwrap_or(0);
    let available = space_probe_cache
        .get(&fs_id)
        .map(|entry| entry.available)
        .context("Internal error: missing space probe cache entry")?;
    let required = bytes.saturating_add(LOW_SPACE_GUARD_MARGIN_BYTES);
    let effective_available = available.saturating_sub(reserved);
    if effective_available < required {
        anyhow::bail!(
            "Low disk space on {}: need {} (+{} safety), available {}",
            parent.display(),
            format_size(bytes),
            format_size(LOW_SPACE_GUARD_MARGIN_BYTES),
            format_size(effective_available)
        );
    }

    guard.insert(fs_id, reserved.saturating_add(bytes));
    if let Some(entry) = space_probe_cache.get_mut(&fs_id) {
        entry.bytes_until_recheck = entry.bytes_until_recheck.saturating_sub(bytes);
    }
    Ok(Some(ReservedSpace { fs_id, bytes }))
}

#[cfg(not(target_family = "unix"))]
pub(crate) fn reserve_destination_space(
    _destination: &std::path::Path,
    _bytes: u64,
    _reservations: &Arc<Mutex<HashMap<u64, u64>>>,
    _space_probe_cache: &mut SpaceProbeCache,
) -> Result<Option<ReservedSpace>> {
    Ok(None)
}

pub(crate) fn release_reserved_space(
    reservations: &Arc<Mutex<HashMap<u64, u64>>>,
    token: Option<ReservedSpace>,
) {
    let Some(token) = token else {
        return;
    };

    let mut guard = reservations.lock().unwrap_or_else(|e| e.into_inner());
    let current = guard.get(&token.fs_id).copied().unwrap_or(0);
    let updated = current.saturating_sub(token.bytes);
    if updated == 0 {
        guard.remove(&token.fs_id);
    } else {
        guard.insert(token.fs_id, updated);
    }
}
