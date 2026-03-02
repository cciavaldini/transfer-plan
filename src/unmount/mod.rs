// Top-level `unmount` module exposes a simple API.  Internals are
// split into platform-specific submodules and a shared `busy` helper.

// On Linux we have a full implementation; other platforms simply noop.
#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
mod busy;

#[cfg(target_os = "linux")]
pub use linux::safe_unmount;

#[cfg(not(target_os = "linux"))]
pub fn unmount_drive(_mount_point: &Path) -> Result<()> {
    // nothing to do on non-Linux platforms
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn safe_unmount(_mount_point: &Path) -> Result<()> {
    Ok(())
}
