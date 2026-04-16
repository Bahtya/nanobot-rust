//! File-based logging with `tracing-appender`.
//!
//! Sets up a non-blocking file writer for the `tracing` ecosystem so that
//! daemon-mode processes can write structured logs to disk instead of
//! (or in addition to) the terminal.

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
pub fn setup_file_logging(log_dir: &str, level: &str) -> Result<LogGuard> {
    let log_path = Path::new(log_dir);
    std::fs::create_dir_all(log_path).context("create log directory")?;

    let file_appender = tracing_appender::rolling::daily(log_path, "kestrel.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_filter(filter);

    let subscriber = Registry::default().with(file_layer);
    tracing::subscriber::set_global_default(subscriber).context("set global tracing subscriber")?;

    tracing::info!("File logging initialized: {}/kestrel.log", log_dir);

    Ok(guard)
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
        let result = setup_file_logging(log_dir_str, "info");
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
        let _ = setup_file_logging(log_dir_str, "info");
        assert!(
            log_dir.exists(),
            "directory must be created even if subscriber fails"
        );
    }
}
