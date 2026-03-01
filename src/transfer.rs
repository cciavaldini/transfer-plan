use anyhow::{Context, Result};
use indicatif::ProgressBar;
use nix::errno::Errno;
use nix::fcntl::copy_file_range;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Optimized buffer sizes based on file size
const SMALL_BUFFER: usize = 64 * 1024; // 64KB for files < 1MB
const MEDIUM_BUFFER: usize = 512 * 1024; // 512KB for files 1-10MB
const LARGE_BUFFER: usize = 2 * 1024 * 1024; // 2MB for files 10-100MB
const XLARGE_BUFFER: usize = 8 * 1024 * 1024; // 8MB for files > 100MB

// copy_file_range configuration
const COPY_FILE_RANGE_THRESHOLD: u64 = 4 * 1024 * 1024; // Use copy_file_range for files ≥ 4MB
const COPY_FILE_RANGE_CHUNK_SIZE: usize = 128 * 1024 * 1024; // 128MB chunks for progress updates

// Verification size limit: don't hash files larger than 10MB
const VERIFICATION_SIZE_LIMIT: u64 = 10 * 1024 * 1024; // 10MB

// Progress update throttling to reduce UI overhead on small files.
const PROGRESS_UPDATE_BYTES: u64 = 2 * 1024 * 1024;
const PROGRESS_UPDATE_INTERVAL_MS: u64 = 250;
const PREPARED_PARENT_DIR_CACHE_LIMIT: usize = 8192;

const MAX_RETRIES: usize = 3;
const RETRY_BASE_DELAY_MS: u64 = 1000;
const ZERO_COPY_UNAVAILABLE: &str = "zero-copy unavailable";

// Buffer pool to reuse allocations
static BUFFER_POOL: Lazy<Mutex<Vec<Vec<u8>>>> = Lazy::new(|| Mutex::new(Vec::new()));
static PREPARED_PARENT_DIRS: Lazy<Mutex<HashSet<PathBuf>>> =
    Lazy::new(|| Mutex::new(HashSet::new()));

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

fn ensure_parent_directory(destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        {
            let cache = PREPARED_PARENT_DIRS.lock();
            if cache.contains(parent) {
                return Ok(());
            }
        }

        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {:?}", parent))?;

        let mut cache = PREPARED_PARENT_DIRS.lock();
        if cache.len() >= PREPARED_PARENT_DIR_CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(parent.to_path_buf());
    }
    Ok(())
}

fn temp_destination_path(destination: &Path) -> Result<PathBuf> {
    let parent = destination
        .parent()
        .context("Destination has no parent directory")?;
    let file_name = destination
        .file_name()
        .context("Destination has no file name")?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(parent.join(format!(".{}.part.{}", file_name, nonce)))
}

fn finalize_atomic_destination(temp_destination: &Path, destination: &Path) -> Result<()> {
    match fs::rename(temp_destination, destination) {
        Ok(()) => Ok(()),
        Err(initial_err) => {
            if destination.exists() {
                fs::remove_file(destination).with_context(|| {
                    format!(
                        "Failed to replace existing destination file {:?}",
                        destination
                    )
                })?;
                fs::rename(temp_destination, destination).with_context(|| {
                    format!(
                        "Failed to atomically move {:?} to {:?}",
                        temp_destination, destination
                    )
                })?;
                Ok(())
            } else {
                Err(initial_err).with_context(|| {
                    format!(
                        "Failed to atomically move {:?} to {:?}",
                        temp_destination, destination
                    )
                })
            }
        }
    }
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
    pending_bytes: u64,
    last_flush: Instant,
}

impl<R> ProgressReader<R> {
    fn flush_pending(&mut self) {
        if self.pending_bytes == 0 {
            return;
        }

        let bytes = self.pending_bytes;
        self.pending_bytes = 0;
        self.last_flush = Instant::now();
        self.copied_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.progress.inc(bytes);
        self.overall_progress.inc(bytes);
    }
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 {
            self.flush_pending();
            return Ok(0);
        }

        self.pending_bytes += n as u64;
        if self.pending_bytes >= PROGRESS_UPDATE_BYTES
            || self.last_flush.elapsed() >= Duration::from_millis(PROGRESS_UPDATE_INTERVAL_MS)
        {
            self.flush_pending();
        }

        Ok(n)
    }
}

/// Copy file using io::copy with progress tracking (for small files)
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
        let reader = File::open(source)
            .with_context(|| format!("Failed to open source file {:?}", source))?;
        let mut reader = ProgressReader {
            inner: io::BufReader::with_capacity(buffer_size, reader),
            progress: progress.clone(),
            overall_progress: overall_progress.clone(),
            copied_bytes: copied_bytes.clone(),
            pending_bytes: 0,
            last_flush: Instant::now(),
        };

        let writer = File::create(&temp_destination)
            .with_context(|| format!("Failed to create destination file {:?}", temp_destination))?;
        let mut writer = io::BufWriter::with_capacity(buffer_size, writer);

        // Use kernel-optimized copy
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

/// Copy file using copy_file_range() for kernel-assisted transfers (for large files)
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
        let src_file = File::open(source)
            .with_context(|| format!("Failed to open source file {:?}", source))?;
        let dst_file = File::create(&temp_destination)
            .with_context(|| format!("Failed to create destination file {:?}", temp_destination))?;

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
fn verify_transfer(source: &Path, dest: &Path, file_size: u64) -> Result<bool> {
    if file_size > VERIFICATION_SIZE_LIMIT {
        // Skip verification for large files
        return Ok(true);
    }

    let source_hash = hash_file(source, file_size)?;
    let dest_hash = hash_file(dest, file_size)?;
    Ok(source_hash == dest_hash)
}

enum AttemptOutcome {
    Completed,
    Retry(Option<anyhow::Error>),
}

fn copy_attempt(
    source: &Path,
    destination: &Path,
    file_size: u64,
    progress: &ProgressBar,
    copied_bytes: &Arc<AtomicU64>,
    overall_progress: &ProgressBar,
    use_copy_file_range: &mut bool,
) -> Result<()> {
    if *use_copy_file_range {
        match copy_file_copy_file_range(
            source,
            destination,
            progress.clone(),
            copied_bytes.clone(),
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
                    copied_bytes.clone(),
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
            copied_bytes.clone(),
            overall_progress.clone(),
            file_size,
        )
    }
}

fn verify_attempt(
    source: &Path,
    destination: &Path,
    file_size: u64,
    can_verify: bool,
    attempt: usize,
    progress: &ProgressBar,
    copied_bytes: &Arc<AtomicU64>,
    overall_progress: &ProgressBar,
) -> Result<AttemptOutcome> {
    if !can_verify {
        return Ok(AttemptOutcome::Completed);
    }

    match verify_transfer(source, destination, file_size) {
        Ok(true) => Ok(AttemptOutcome::Completed),
        Ok(false) => {
            rollback_file_attempt_progress(progress, copied_bytes, overall_progress);
            if attempt == MAX_RETRIES {
                anyhow::bail!("Verification failed after {} attempts", MAX_RETRIES);
            }
            Ok(AttemptOutcome::Retry(None))
        }
        Err(e) => {
            rollback_file_attempt_progress(progress, copied_bytes, overall_progress);
            if attempt == MAX_RETRIES {
                return Err(e);
            }
            Ok(AttemptOutcome::Retry(Some(e)))
        }
    }
}

fn apply_retry_backoff(progress: &ProgressBar, attempt: usize) {
    let delay = Duration::from_millis(RETRY_BASE_DELAY_MS * (1 << (attempt - 1)));
    std::thread::sleep(delay);
    progress.reset();
}

/// Copy file with retry logic and verification (HYBRID APPROACH - Option 3)
pub fn copy_file_optimized(
    source: &Path,
    destination: &Path,
    file_size: u64,
    progress: ProgressBar,
    verify: bool,
    copied_bytes: Arc<AtomicU64>,
    overall_progress: ProgressBar,
) -> Result<()> {
    let can_verify = verify && file_size <= VERIFICATION_SIZE_LIMIT;

    let mut attempt = 0;
    let mut last_error = None;
    let mut use_copy_file_range = file_size >= COPY_FILE_RANGE_THRESHOLD;

    while attempt <= MAX_RETRIES {
        match copy_attempt(
            source,
            destination,
            file_size,
            &progress,
            &copied_bytes,
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
                &copied_bytes,
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
