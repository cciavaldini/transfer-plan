use anyhow::{Context, Result};
use indicatif::ProgressBar;
use nix::errno::Errno;
use nix::fcntl::copy_file_range;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::task;

// Optimized buffer sizes based on file size
const SMALL_BUFFER: usize = 64 * 1024; // 64KB for files < 1MB
const MEDIUM_BUFFER: usize = 512 * 1024; // 512KB for files 1-10MB
const LARGE_BUFFER: usize = 2 * 1024 * 1024; // 2MB for files 10-100MB
const XLARGE_BUFFER: usize = 8 * 1024 * 1024; // 8MB for files > 100MB

// copy_file_range configuration
const COPY_FILE_RANGE_THRESHOLD: u64 = 10 * 1024 * 1024; // Use copy_file_range for files ≥ 10MB
const COPY_FILE_RANGE_CHUNK_SIZE: usize = 128 * 1024 * 1024; // 128MB chunks for progress updates

// Verification size limit: don't hash files larger than 10MB
const VERIFICATION_SIZE_LIMIT: u64 = 10 * 1024 * 1024; // 10MB

const MAX_RETRIES: usize = 3;
const RETRY_BASE_DELAY_MS: u64 = 1000;

// Buffer pool to reuse allocations
static BUFFER_POOL: Lazy<Mutex<Vec<Vec<u8>>>> = Lazy::new(|| Mutex::new(Vec::new()));

fn get_buffer(size: usize) -> Vec<u8> {
    BUFFER_POOL
        .lock()
        .pop()
        .filter(|b| b.len() == size)
        .unwrap_or_else(|| vec![0u8; size])
}

fn return_buffer(buffer: Vec<u8>) {
    let mut pool = BUFFER_POOL.lock();
    if pool.len() < 10 {
        // Max 10 buffers in pool
        pool.push(buffer);
    }
}

fn atomic_saturating_sub(counter: &AtomicU64, value: u64) {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let updated = current.saturating_sub(value);
        match counter.compare_exchange_weak(current, updated, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
}

fn rollback_file_attempt_progress(
    progress: &ProgressBar,
    copied_bytes: &Arc<AtomicU64>,
    overall_progress: &ProgressBar,
) {
    let attempt_bytes = progress.position();
    if attempt_bytes == 0 {
        return;
    }

    atomic_saturating_sub(copied_bytes.as_ref(), attempt_bytes);
    overall_progress.set_position(copied_bytes.load(Ordering::Relaxed));
    progress.set_position(0);
}

fn finish_with_overall_sync(
    progress: &ProgressBar,
    overall_progress: &ProgressBar,
    copied_bytes: &Arc<AtomicU64>,
) {
    if let Some(total) = progress.length() {
        let current = progress.position();
        let remaining = total.saturating_sub(current);
        if remaining > 0 {
            copied_bytes.fetch_add(remaining, Ordering::Relaxed);
            overall_progress.inc(remaining);
            progress.inc(remaining);
        }
    }
    progress.finish();
}

#[inline]
fn optimal_buffer_size(file_size: u64) -> usize {
    match file_size {
        0..=1_048_576 => SMALL_BUFFER,            // <1MB
        1_048_577..=10_485_760 => MEDIUM_BUFFER,  // 1-10MB
        10_485_761..=104_857_600 => LARGE_BUFFER, // 10-100MB
        _ => XLARGE_BUFFER,                       // >100MB
    }
}

/// Progress-tracking reader wrapper for io::copy
struct ProgressReader<R> {
    inner: R,
    progress: ProgressBar,
    overall_progress: ProgressBar,
    copied_bytes: Arc<AtomicU64>,
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            let bytes = n as u64;
            self.copied_bytes.fetch_add(bytes, Ordering::Relaxed);
            self.progress.inc(bytes);
            self.overall_progress.inc(bytes);
        }
        Ok(n)
    }
}

/// Copy file using io::copy with progress tracking (for small files)
async fn copy_file_iocopy(
    source: &Path,
    destination: &Path,
    progress: ProgressBar,
    copied_bytes: Arc<AtomicU64>,
    overall_progress: ProgressBar,
) -> Result<()> {
    // Create parent directories if needed
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {:?}", parent))?;
    }

    let source = source.to_path_buf();
    let destination = destination.to_path_buf();

    // Use blocking task for file I/O
    task::spawn_blocking(move || -> Result<()> {
        let file_size = fs::metadata(&source)?.len();
        let buffer_size = optimal_buffer_size(file_size);

        let reader = File::open(&source)
            .with_context(|| format!("Failed to open source file {:?}", source))?;
        let mut reader = ProgressReader {
            inner: io::BufReader::with_capacity(buffer_size, reader),
            progress: progress.clone(),
            overall_progress: overall_progress.clone(),
            copied_bytes: copied_bytes.clone(),
        };

        let writer = File::create(&destination)
            .with_context(|| format!("Failed to create destination file {:?}", destination))?;
        let mut writer = io::BufWriter::with_capacity(buffer_size, writer);

        // Use kernel-optimized copy
        io::copy(&mut reader, &mut writer)?;
        writer.flush()?;

        finish_with_overall_sync(&progress, &overall_progress, &copied_bytes);
        Ok(())
    })
    .await??;

    Ok(())
}

/// Copy file using copy_file_range() for kernel-assisted transfers (for large files)
async fn copy_file_copy_file_range(
    source: &Path,
    destination: &Path,
    progress: ProgressBar,
    copied_bytes: Arc<AtomicU64>,
    overall_progress: ProgressBar,
) -> Result<()> {
    // Create parent directories if needed
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {:?}", parent))?;
    }

    let source = source.to_path_buf();
    let destination = destination.to_path_buf();

    // Use blocking task for file I/O
    task::spawn_blocking(move || -> Result<()> {
        let src_file = File::open(&source)
            .with_context(|| format!("Failed to open source file {:?}", source))?;
        let dst_file = File::create(&destination)
            .with_context(|| format!("Failed to create destination file {:?}", destination))?;

        let file_size = src_file.metadata()?.len();

        let mut offset: libc::off_t = 0;
        let mut remaining: u64 = file_size;

        while remaining > 0 {
            let chunk_size = std::cmp::min(remaining, COPY_FILE_RANGE_CHUNK_SIZE as u64) as usize;

            match copy_file_range(&src_file, Some(&mut offset), &dst_file, None, chunk_size) {
                Ok(bytes_sent) => {
                    if bytes_sent == 0 {
                        // EOF reached unexpectedly
                        anyhow::bail!("Unexpected EOF: {} bytes remaining", remaining);
                    }
                    let bytes = bytes_sent as u64;
                    remaining -= bytes;
                    copied_bytes.fetch_add(bytes, Ordering::Relaxed);
                    progress.inc(bytes);
                    overall_progress.inc(bytes);
                }
                Err(Errno::EINVAL) | Err(Errno::ENOSYS) | Err(Errno::EXDEV) => {
                    // Filesystem/kernel doesn't support copy_file_range, fall back to io::copy
                    progress.set_message("⚠️ copy_file_range not supported, using fallback");
                    rollback_file_attempt_progress(&progress, &copied_bytes, &overall_progress);
                    drop(src_file);
                    drop(dst_file);

                    // Remove partial file
                    let _ = fs::remove_file(&destination);

                    // Fall back to io::copy
                    return Err(anyhow::anyhow!(
                        "copy_file_range not supported, need fallback"
                    ));
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("copy_file_range error: {}", e));
                }
            }
        }

        // Ensure data is written
        drop(dst_file);
        finish_with_overall_sync(&progress, &overall_progress, &copied_bytes);
        Ok(())
    })
    .await??;

    Ok(())
}

/// Hash file for verification (only for files <= 10MB)
fn hash_file(path: &Path, file_size: u64) -> Result<Vec<u8>> {
    if file_size > VERIFICATION_SIZE_LIMIT {
        anyhow::bail!(
            "File too large for verification ({}MB > 10MB limit)",
            file_size / 1_048_576
        );
    }

    let file =
        File::open(path).with_context(|| format!("Failed to open file for hashing: {:?}", path))?;
    let mut reader = io::BufReader::with_capacity(1024 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buffer = get_buffer(1024 * 1024);

    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }

    return_buffer(buffer);
    Ok(hasher.finalize().to_vec())
}

/// Verify transfer by comparing hashes (only for files <= 10MB)
async fn verify_transfer(source: &Path, dest: &Path, file_size: u64) -> Result<bool> {
    if file_size > VERIFICATION_SIZE_LIMIT {
        // Skip verification for large files
        return Ok(true);
    }

    let source = source.to_path_buf();
    let dest = dest.to_path_buf();

    task::spawn_blocking(move || -> Result<bool> {
        let source_hash = hash_file(&source, file_size)?;
        let dest_hash = hash_file(&dest, file_size)?;
        Ok(source_hash == dest_hash)
    })
    .await?
}

/// Copy file with retry logic and verification (HYBRID APPROACH - Option 3)
pub async fn copy_file_optimized(
    source: &Path,
    destination: &Path,
    progress: ProgressBar,
    verify: bool,
    copied_bytes: Arc<AtomicU64>,
    overall_progress: ProgressBar,
) -> Result<()> {
    // Get file size once
    let file_size = fs::metadata(source)
        .with_context(|| format!("Failed to get file size for {:?}", source))?
        .len();

    // Check if file is too large for verification
    let can_verify = verify && file_size <= VERIFICATION_SIZE_LIMIT;

    // Update progress message for large files
    if verify && file_size > VERIFICATION_SIZE_LIMIT {
        let file_name = source.file_name().unwrap_or_default().to_string_lossy();
        progress.set_message(format!(
            "📄 {} ({} - verification skipped)",
            file_name,
            format_size(file_size)
        ));
    }

    let mut attempt = 0;
    let mut last_error = None;
    let mut use_copy_file_range = file_size >= COPY_FILE_RANGE_THRESHOLD;

    while attempt <= MAX_RETRIES {
        // Choose copy method based on file size (HYBRID APPROACH)
        let copy_result = if use_copy_file_range {
            // Large file: use copy_file_range() for kernel-assisted transfer
            match copy_file_copy_file_range(
                source,
                destination,
                progress.clone(),
                copied_bytes.clone(),
                overall_progress.clone(),
            )
            .await
            {
                Ok(_) => Ok(()),
                Err(e) if e.to_string().contains("copy_file_range not supported") => {
                    // Fall back to io::copy for this file
                    use_copy_file_range = false;
                    copy_file_iocopy(
                        source,
                        destination,
                        progress.clone(),
                        copied_bytes.clone(),
                        overall_progress.clone(),
                    )
                    .await
                }
                Err(e) => Err(e),
            }
        } else {
            // Small file: use io::copy() for better progress granularity
            copy_file_iocopy(
                source,
                destination,
                progress.clone(),
                copied_bytes.clone(),
                overall_progress.clone(),
            )
            .await
        };

        match copy_result {
            Ok(_) => {
                // Verify if requested and file is small enough
                if can_verify {
                    match verify_transfer(source, destination, file_size).await {
                        Ok(true) => return Ok(()),
                        Ok(false) => {
                            rollback_file_attempt_progress(
                                &progress,
                                &copied_bytes,
                                &overall_progress,
                            );
                            if attempt == MAX_RETRIES {
                                anyhow::bail!("Verification failed after {} attempts", MAX_RETRIES);
                            }
                            progress.set_message(format!(
                                "⚠️ Verification failed, retry {}/{}",
                                attempt + 1,
                                MAX_RETRIES
                            ));
                        }
                        Err(e) => {
                            rollback_file_attempt_progress(
                                &progress,
                                &copied_bytes,
                                &overall_progress,
                            );
                            if attempt == MAX_RETRIES {
                                return Err(e);
                            }
                            last_error = Some(e);
                            progress.set_message(format!(
                                "⚠️ Verification error, retry {}/{}",
                                attempt + 1,
                                MAX_RETRIES
                            ));
                        }
                    }
                } else {
                    return Ok(());
                }
            }
            Err(e) => {
                rollback_file_attempt_progress(&progress, &copied_bytes, &overall_progress);
                if attempt == MAX_RETRIES {
                    return Err(e);
                }
                last_error = Some(e);
                progress.set_message(format!("⚠️ Retry {}/{}", attempt + 1, MAX_RETRIES));
            }
        }

        attempt += 1;
        if attempt <= MAX_RETRIES {
            // Exponential backoff
            let delay = Duration::from_millis(RETRY_BASE_DELAY_MS * (1 << (attempt - 1)));
            tokio::time::sleep(delay).await;
            progress.reset();
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Transfer failed after retries")))
}

/// Get human-readable file size
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} bytes", bytes)
    }
}
