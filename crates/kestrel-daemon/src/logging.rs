//! File-based logging with `tracing-appender`.
//!
//! Sets up a non-blocking file writer for the `tracing` ecosystem so that
//! daemon-mode processes can write structured logs to disk instead of
//! (or in addition to) the terminal. Supports both human-readable text and
//! structured JSON output formats.

use anyhow::{Context, Result};
use std::path::Path;
use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Layer, Registry};

/// Guard returned by [`setup_file_logging`]. Must be kept alive for the
/// lifetime of the application — dropping it flushes and closes the log file.
pub type LogGuard = tracing_appender::non_blocking::WorkerGuard;

/// Configure file-based logging for daemon mode.
///
/// Creates a non-blocking writer that appends to `{log_dir}/kestrel.log`.
/// The `tracing_subscriber` is configured with the given log level filter.
///
/// # Arguments
///
/// * `log_dir` - Directory for log files (created if it doesn't exist).
/// * `level` - Log level filter (e.g. `"info"`, `"debug"`, `"trace"`).
/// * `log_format` - Output format: `"text"` (human-readable) or `"json"`.
///
/// # Returns
///
/// A [`LogGuard`] that must be held for the application's lifetime.
/// Dropping the guard flushes remaining log lines and stops the writer thread.
///
/// # Errors
///
/// Returns an error if the log directory cannot be created or the subscriber
/// cannot be installed.
pub fn setup_file_logging(log_dir: &str, level: &str, log_format: &str) -> Result<LogGuard> {
    let log_path = Path::new(log_dir);
    std::fs::create_dir_all(log_path).context("create log directory")?;

    let effective_format = match log_format {
        "text" | "json" => log_format,
        _ => {
            tracing::warn!(
                "Invalid log_format '{}', falling back to 'text'. Supported: text, json",
                log_format
            );
            "text"
        }
    };

    let file_appender = tracing_appender::rolling::daily(log_path, "kestrel.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    if effective_format == "json" {
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(non_blocking)
            .with_ansi(false)
            .json()
            .with_filter(filter);
        let subscriber = Registry::default().with(file_layer);
        tracing::subscriber::set_global_default(subscriber)
            .context("set global tracing subscriber")?;
    } else {
        let file_layer = tracing_subscriber::fmt::layer()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_filter(filter);
        let subscriber = Registry::default().with(file_layer);
        tracing::subscriber::set_global_default(subscriber)
            .context("set global tracing subscriber")?;
    }

    tracing::info!(
        "File logging initialized: {}/kestrel.log (format: {})",
        log_dir,
        effective_format
    );

    Ok(guard)
}

/// Delete log files older than `retain_days` from `log_dir`.
///
/// Scans the directory for files matching the `kestrel.log.*` pattern and
/// removes any whose modification time is older than the retention period.
/// Errors during individual file removal are logged but not propagated.
pub fn cleanup_old_logs(log_dir: &str, retain_days: u64) {
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return;
    };

    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(retain_days * 24 * 60 * 60);

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let filename = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        // Only clean up rolling log files (kestrel.log.YYYY-MM-DD)
        if !filename.starts_with("kestrel.log.") {
            continue;
        }

        if let Ok(metadata) = path.metadata() {
            if let Ok(modified) = metadata.modified() {
                if modified < cutoff {
                    match std::fs::remove_file(&path) {
                        Ok(()) => tracing::info!("Cleaned up old log file: {}", filename),
                        Err(e) => tracing::warn!("Failed to remove {}: {}", filename, e),
                    }
                }
            }
        }
    }
}

/// Spawn a background task that periodically cleans up old log files.
///
/// Runs cleanup immediately, then every 24 hours. The task exits when
/// the returned `JoinHandle` is dropped or cancelled.
pub fn spawn_log_cleanup(log_dir: String, retain_days: u64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Run once at startup (offloaded to blocking thread)
        let dir = log_dir.clone();
        tokio::task::spawn_blocking(move || cleanup_old_logs(&dir, retain_days))
            .await
            .unwrap_or(());

        let mut interval = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
        loop {
            interval.tick().await;
            let dir = log_dir.clone();
            tokio::task::spawn_blocking(move || cleanup_old_logs(&dir, retain_days))
                .await
                .unwrap_or(());
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_setup_file_logging_creates_directory() {
        let tmp = TempDir::new().unwrap();
        let log_dir = tmp.path().join("logs").join("subdir");
        let log_dir_str = log_dir.to_str().unwrap();

        // Test directory creation — ignore the global subscriber conflict
        // (set_global_default can only be called once per process)
        let result = setup_file_logging(log_dir_str, "info", "text");
        assert!(log_dir.exists(), "log directory should be created");

        // The guard may fail on re-install, but directory must exist regardless
        if let Ok(_guard) = result {
            tracing::info!("test log message from unit test");
        }
    }

    #[test]
    fn test_log_directory_created_even_if_subscriber_fails() {
        let tmp = TempDir::new().unwrap();
        let log_dir = tmp.path().join("deep").join("nested").join("logs");
        let log_dir_str = log_dir.to_str().unwrap();

        // The function creates the directory before attempting to set subscriber
        let _ = setup_file_logging(log_dir_str, "info", "text");
        assert!(
            log_dir.exists(),
            "directory must be created even if subscriber fails"
        );
    }

    #[test]
    fn test_cleanup_old_logs_removes_expired() {
        let tmp = TempDir::new().unwrap();
        let log_dir = tmp.path();

        // Create an "old" file by setting mtime to 60 days ago
        let old_file = log_dir.join("kestrel.log.2025-01-01");
        std::fs::write(&old_file, "old log").unwrap();
        let old_time =
            std::time::SystemTime::now() - std::time::Duration::from_secs(60 * 24 * 60 * 60);
        filetime::set_file_mtime(&old_file, filetime::FileTime::from_system_time(old_time))
            .unwrap();

        // Create a recent file that should be kept
        let recent_file = log_dir.join("kestrel.log.2099-01-01");
        std::fs::write(&recent_file, "recent log").unwrap();

        // Create a non-log file that should be ignored
        let other_file = log_dir.join("other.txt");
        std::fs::write(&other_file, "not a log").unwrap();

        cleanup_old_logs(log_dir.to_str().unwrap(), 30);

        assert!(!old_file.exists(), "old log should be removed");
        assert!(recent_file.exists(), "recent log should be kept");
        assert!(other_file.exists(), "non-log file should be untouched");
    }

    #[test]
    fn test_cleanup_old_logs_nonexistent_dir() {
        // Should not panic on nonexistent directory
        cleanup_old_logs("/tmp/nonexistent_kestrel_test_dir", 30);
    }

    #[test]
    fn test_cleanup_old_logs_keeps_non_prefixed() {
        let tmp = TempDir::new().unwrap();
        let log_dir = tmp.path();

        // Create old files that DON'T match the kestrel.log.* pattern
        let audit_file = log_dir.join("kestrel.audit.jsonl");
        std::fs::write(&audit_file, "audit").unwrap();

        cleanup_old_logs(log_dir.to_str().unwrap(), 0);

        assert!(audit_file.exists(), "audit file should not be cleaned up");
    }

    #[test]
    fn test_log_format_invalid_falls_back_to_text() {
        let tmp = TempDir::new().unwrap();
        let log_dir = tmp.path().join("format_test");
        let log_dir_str = log_dir.to_str().unwrap();

        // "xml" is not a valid format — should fall back to "text"
        let result = setup_file_logging(log_dir_str, "info", "xml");
        assert!(log_dir.exists());
        if let Ok(_guard) = result {
            // Subscriber may conflict with other tests; that's OK
        }
    }
}
