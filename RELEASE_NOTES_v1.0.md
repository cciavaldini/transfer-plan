# Release Notes v1.0

## Summary

This baseline delivers a full interactive transfer flow with reliability features, performance-focused copy paths, and Linux-friendly unmount handling.

## Highlights

- Parallel transfer worker pool with dynamic queue processing
- Transfer statistics with average throughput and total elapsed time
- Post-transfer `sync` step included in final timing metrics
- Retry logic with progress rollback to avoid inflated totals
- Queue checkpoint persistence and resume (`pending_queue.json`)
- Mount watcher that can auto-queue mappings when removable media appears
- Optional cleanup mode to delete sources after successful copy
- Optional unmount with multi-strategy attempts:
  - `udisksctl`
  - `umount`
  - direct syscall fallback
- Busy mount diagnostics using `fuser` and `ps`

## Verification and testing

- Optional SHA-256 verification for files up to 10 MB
- Integration tests added for end-to-end transfer scenarios
- Clippy and test suite pass for the current baseline

## Notes

- Linux is the primary supported environment for mount/unmount features.
- Configuration is persisted through `defaults.json` with fallback config locations.
