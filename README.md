# TransferPlan

*This repository was built as “vibe coding” – a fast‑paced, iterative
hackathon style effort rather than a polished design project.*

TransferPlan is a Rust CLI for moving files and directories to removable
storage with parallel workers, progress reporting, checkpoint resume, and
optional safe unmount.

## Current baseline

This repository state is treated as the first baseline version.

## Features

- Parallel transfer workers (`1..=8`)
- Per-file and global progress bars
- `copy_file_range` for large files with automatic fallback
- Retry with exponential backoff
- Optional SHA-256 verification for files up to 10 MB
- Queue checkpoint persistence to `pending_queue.json`
- Resume interrupted transfer sessions
- Auto-queue when a removable destination mount appears
- Optional source cleanup mode (`none` or `delete`)
- Final filesystem sync after transfer (included in total time and average rate)
- Optional unmount flow with busy-process diagnostics

## Requirements

- Rust toolchain (Cargo)
- Linux is the primary target for mount detection and unmount behavior

## Build and run

```bash
cargo build --release
cargo run --release
```

## Typical workflow

1. Start the app.
2. Configure source/destination mappings.
3. Choose transfer options:
   - worker count
   - verification on/off
   - auto-unmount on/off
   - cleanup mode
4. Let the transfer run.
5. Optionally add more files for another batch.

## Configuration

`defaults.json` is loaded from:

1. project root (`./defaults.json`)
2. `$HOME/.config/transfer-plan/defaults.json`
3. `$HOME/.config/usb-transfer/defaults.json` (legacy)

Key config fields:

- `default_source_folder`
- `default_destination_folder`
- `default_unmount_drives`

Example:

```json
{
  "version": 1,
  "default_source_folder": "/home/user",
  "default_destination_folder": "/media/usb",
  "default_unmount_drives": ["/media/8To", "/media/16To"]
}
```

## Testing and quality checks

```bash
cargo test
cargo clippy --all-targets --all-features
```

Integration tests are in `tests/end_to_end_transfer.rs`.
