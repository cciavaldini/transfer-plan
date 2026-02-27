## [1.0.0] - 2026-02-26

### Added

- Interactive transfer mapping and option prompts
- Parallel worker pool for file transfer
- Queue checkpoint save/resume with `pending_queue.json`
- Optional SHA-256 verification for files up to 10 MB
- Optional cleanup mode (`none`, `delete`)
- Optional unmount with fallback strategies
- Busy-process reporting for failed unmount attempts
- Integration test suite for end-to-end transfer flow

### Changed

- Average throughput and total time include final sync duration
- Live progress shows pending sync estimate
- Retry paths roll back per-attempt progress to keep totals accurate

### Notes

- This codebase state is treated as the first baseline version.
