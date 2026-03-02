use indicatif::ProgressStyle;
use once_cell::sync::Lazy;

pub(crate) const FILE_PROGRESS_BAR_THRESHOLD: u64 = 8 * 1024 * 1024;
const FILE_PROGRESS_LABEL_WIDTH: usize = 48;

pub(crate) static FILE_PROGRESS_STYLE: Lazy<ProgressStyle> = Lazy::new(|| {
    ProgressStyle::default_bar()
        .template(
            "{prefix} [{bar:30.green/white}] {bytes}/{total_bytes} ({eta}) {binary_bytes_per_sec:.cyan} {msg}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("█▓▒░-")
});

pub(crate) fn format_eta(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, secs)
}

fn truncate_for_display(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }

    let len = input.chars().count();
    if len <= max_chars {
        return input.to_string();
    }

    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }

    let mut truncated = input.chars().take(max_chars - 3).collect::<String>();
    truncated.push_str("...");
    truncated
}

pub(crate) fn format_file_progress_label(worker_id: usize, file_name: &str) -> String {
    let prefix = format!("[W{}] ", worker_id);
    let max_name_chars = FILE_PROGRESS_LABEL_WIDTH.saturating_sub(prefix.chars().count());
    let short_name = truncate_for_display(file_name, max_name_chars);
    let label = format!("{}{}", prefix, short_name);
    format!("{:<width$}", label, width = FILE_PROGRESS_LABEL_WIDTH)
}
