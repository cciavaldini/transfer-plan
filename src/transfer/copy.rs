use anyhow::{Result, Context};
use indicatif::ProgressBar;
use nix::errno::Errno;
use nix::fcntl::copy_file_range as nix_copy_file_range;
use std::fs::{self, File};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::io::{self, Write};
use std::time::Instant;

use crate::transfer::{COPY_FILE_RANGE_CHUNK_SIZE, COPY_FILE_RANGE_THRESHOLD, MAX_RETRIES, ZERO_COPY_UNAVAILABLE, RETRY_BASE_DELAY_MS};
use crate::transfer::helpers::{ensure_parent_directory, temp_destination_path, finalize_atomic_destination, rollback_file_attempt_progress};
use crate::transfer::progress::{ProgressReader, finish_with_overall_sync};
use crate::transfer::verify::{verify_transfer};
use crate::transfer::{SMALL_BUFFER, MEDIUM_BUFFER, LARGE_BUFFER, XLARGE_BUFFER};

/// Pick a read/write buffer capacity based on total file size.
/// Small files use a modest buffer to avoid wasting memory, while very large
/// transfers benefit from a larger I/O window.
fn optimal_buffer_size(file_size: u64) -> usize {
    match file_size {
        0..=1_048_576 => SMALL_BUFFER,
        1_048_577..=10_485_760 => MEDIUM_BUFFER,
        10_485_761..=104_857_600 => LARGE_BUFFER,
        _ => XLARGE_BUFFER,
    }
}

/// Perform a conventional I/O-based copy from `source` to `destination`.
///
/// The operation writes to a temporary file before atomically renaming. A
/// `ProgressReader` wraps the source reader to periodically update the
/// supplied progress bars and maintain an atomic count of bytes copied. After
/// completion the write buffers are flushed and the temporary file finalized.
fn copy_file_iocopy(
    source: &Path,
    destination: &Path,
    progress: ProgressBar,
    copied_bytes: Arc<AtomicU64>,
    overall_progress: ProgressBar,
    file_size: u64,
) -> Result<()> {
    ensure_parent_directory(destination)?;
    let temp_destination = temp_destination_path(destination)?;

    let copy_result = (|| -> Result<()> {
        let buffer_size = optimal_buffer_size(file_size);
        let reader_file = File::open(source).with_context(|| format!("Failed to open source file {:?}", source))?;
        let mut reader = ProgressReader {
            inner: io::BufReader::with_capacity(buffer_size, reader_file),
            progress: progress.clone(),
            overall_progress: overall_progress.clone(),
            copied_bytes: copied_bytes.clone(),
            pending_bytes: 0,
            last_flush: Instant::now(),
        };

        let writer = File::create(&temp_destination).with_context(|| format!("Failed to create destination file {:?}", temp_destination))?;
        let mut writer = io::BufWriter::with_capacity(buffer_size, writer);

        io::copy(&mut reader, &mut writer)?;
        reader.flush_pending();
        writer.flush()?;
        drop(writer);

        finalize_atomic_destination(&temp_destination, destination)?;
        Ok(())
    })();

    if let Err(e) = copy_result {
        let _ = fs::remove_file(&temp_destination);
        return Err(e);
    }

    finish_with_overall_sync(&progress, &overall_progress, &copied_bytes);
    Ok(())
}

/// Use the Linux `copy_file_range` syscall (via nix) to perform a kernel-
/// assisted zero-copy transfer. Falls back to `ZERO_COPY_UNAVAILABLE` if the
/// syscall is unsupported or fails with certain errno codes.
///
/// Progress bars and byte counters are updated for each chunk successfully
/// transferred. The same atomic rename pattern is used as in the iocopy path.
fn copy_file_copy_file_range(
    source: &Path,
    destination: &Path,
    progress: ProgressBar,
    copied_bytes: Arc<AtomicU64>,
    overall_progress: ProgressBar,
    file_size: u64,
) -> Result<()> {
    ensure_parent_directory(destination)?;
    let temp_destination = temp_destination_path(destination)?;

    let copy_result = (|| -> Result<()> {
        let src_file = File::open(source).with_context(|| format!("Failed to open source file {:?}", source))?;
        let dst_file = File::create(&temp_destination).with_context(|| format!("Failed to create destination file {:?}", temp_destination))?;

        let mut offset: libc::off_t = 0;
        let mut remaining: u64 = file_size;

        while remaining > 0 {
            let chunk_size = std::cmp::min(remaining, COPY_FILE_RANGE_CHUNK_SIZE as u64) as usize;

            match nix_copy_file_range(&src_file, Some(&mut offset), &dst_file, None, chunk_size) {
                Ok(bytes_sent) => {
                    if bytes_sent == 0 {
                        anyhow::bail!("Unexpected EOF: {} bytes remaining", remaining);
                    }
                    let bytes = bytes_sent as u64;
                    remaining -= bytes;
                    copied_bytes.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
                    progress.inc(bytes);
                    overall_progress.inc(bytes);
                }
                Err(Errno::EINVAL) | Err(Errno::ENOSYS) | Err(Errno::EXDEV) => {
                    rollback_file_attempt_progress(&progress, &copied_bytes, &overall_progress);
                    drop(src_file);
                    drop(dst_file);
                    let _ = fs::remove_file(&temp_destination);
                    return Err(anyhow::anyhow!(ZERO_COPY_UNAVAILABLE));
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("copy_file_range error: {}", e));
                }
            }
        }

        drop(dst_file);
        finalize_atomic_destination(&temp_destination, destination)?;
        Ok(())
    })();

    if let Err(e) = copy_result {
        let _ = fs::remove_file(&temp_destination);
        return Err(e);
    }

    finish_with_overall_sync(&progress, &overall_progress, &copied_bytes);
    Ok(())
}

enum AttemptOutcome {
    Completed,
    Retry(Option<anyhow::Error>),
}

/// Perform a single attempt to transfer the file, choosing between the
/// optimized `copy_file_range` path and conventional I/O. The caller supplies
/// a mutable flag `use_copy_file_range` which is toggled off if zero-copy turns
/// out to be unavailable, ensuring subsequent retries fall back automatically.
fn copy_attempt(
    source: &Path,
    destination: &Path,
    file_size: u64,
    progress: &ProgressBar,
    copied_bytes: Arc<AtomicU64>,
    overall_progress: &ProgressBar,
    use_copy_file_range: &mut bool,
) -> Result<()> {
    if *use_copy_file_range {
        match copy_file_copy_file_range(
            source,
            destination,
            progress.clone(),
            Arc::clone(&copied_bytes),
            overall_progress.clone(),
            file_size,
        ) {
            Ok(()) => Ok(()),
            Err(e) if e.to_string().contains(ZERO_COPY_UNAVAILABLE) => {
                progress.set_message(ZERO_COPY_UNAVAILABLE.to_string());
                *use_copy_file_range = false;
                copy_file_iocopy(
                    source,
                    destination,
                    progress.clone(),
                    Arc::clone(&copied_bytes),
                    overall_progress.clone(),
                    file_size,
                )
            }
            Err(e) => Err(e),
        }
    } else {
        copy_file_iocopy(
            source,
            destination,
            progress.clone(),
            Arc::clone(&copied_bytes),
            overall_progress.clone(),
            file_size,
        )
    }
}

/// After a copy attempt, optionally verify the transfer by checksum. If
/// verification fails the progress bars are rolled back and either an error or
/// a retry indication is returned. The caller is responsible for backoff and
/// stopping after `MAX_RETRIES`.
fn verify_attempt(
    source: &Path,
    destination: &Path,
    file_size: u64,
    can_verify: bool,
    attempt: usize,
    progress: &ProgressBar,
    copied_bytes: Arc<AtomicU64>,
    overall_progress: &ProgressBar,
) -> Result<AttemptOutcome> {
    if !can_verify {
        return Ok(AttemptOutcome::Completed);
    }

    match verify_transfer(source, destination, file_size) {
        Ok(true) => Ok(AttemptOutcome::Completed),
        Ok(false) => {
            rollback_file_attempt_progress(progress, &copied_bytes, overall_progress);
            if attempt == MAX_RETRIES {
                anyhow::bail!("Verification failed after {} attempts", MAX_RETRIES);
            }
            Ok(AttemptOutcome::Retry(None))
        }
        Err(e) => {
            rollback_file_attempt_progress(progress, &copied_bytes, overall_progress);
            if attempt == MAX_RETRIES {
                return Err(e);
            }
            Ok(AttemptOutcome::Retry(Some(e)))
        }
    }
}

fn apply_retry_backoff(progress: &ProgressBar, attempt: usize) {
    let delay = std::time::Duration::from_millis(RETRY_BASE_DELAY_MS * (1 << (attempt - 1)));
    std::thread::sleep(delay);
    progress.reset();
}

/// High-level entry point for copying a single file with retries,
/// verification, and progress reporting. It chooses the appropriate method
/// (zero-copy vs normal) based on file size and fallback conditions, and
/// applies exponential backoff between attempts.
///
/// Parameters:
/// * `source`/`destination` - file paths
/// * `file_size` - size of the file in bytes
/// * `progress`/`overall_progress` - indicatif progress bars
/// * `verify` - whether to checksum after each transfer
/// * `copied_bytes` - atomic counter shared across files for global progress
pub fn copy_file_optimized(
    source: &Path,
    destination: &Path,
    file_size: u64,
    progress: ProgressBar,
    verify: bool,
    copied_bytes: Arc<AtomicU64>,
    overall_progress: ProgressBar,
) -> Result<()> {
    tracing::debug!(
        source = %source.display(),
        destination = %destination.display(),
        file_size,
        verify,
        "Starting optimized file copy"
    );
    let can_verify = verify && file_size <= crate::transfer::VERIFICATION_SIZE_LIMIT;

    let mut attempt = 0;
    let mut last_error = None;
    let mut use_copy_file_range = file_size >= COPY_FILE_RANGE_THRESHOLD;

    while attempt <= MAX_RETRIES {
        match copy_attempt(
            source,
            destination,
            file_size,
            &progress,
            Arc::clone(&copied_bytes),
            &overall_progress,
            &mut use_copy_file_range,
        ) {
            Ok(()) => match verify_attempt(
                source,
                destination,
                file_size,
                can_verify,
                attempt,
                &progress,
                Arc::clone(&copied_bytes),
                &overall_progress,
            )? {
                AttemptOutcome::Completed => return Ok(()),
                AttemptOutcome::Retry(err) => {
                    if err.is_some() {
                        last_error = err;
                    }
                }
            },
            Err(e) => {
                rollback_file_attempt_progress(&progress, &copied_bytes, &overall_progress);
                if attempt == MAX_RETRIES {
                    return Err(e);
                }
                last_error = Some(e);
            }
        }

        attempt += 1;
        if attempt <= MAX_RETRIES {
            apply_retry_backoff(&progress, attempt);
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Transfer failed after retries")))
}