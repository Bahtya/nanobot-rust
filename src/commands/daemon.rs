//! Daemon subcommand — manage the nanobot-rs background daemon.
//!
//! Provides `start`, `stop`, `restart`, and `status` actions for controlling
//! the daemonized nanobot-rs process.

use anyhow::{bail, Result};
use nanobot_config::Config;

/// Actions for the `daemon` subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonAction {
    /// Daemonize the process and run the gateway.
    Start,
    /// Send SIGTERM to the running daemon and wait for exit.
    Stop,
    /// Stop then start the daemon.
    Restart,
    /// Check if the daemon is running.
    Status,
}

/// Execute a daemon management action.
///
/// **Note**: `Start` performs daemonization and must run BEFORE the tokio
/// runtime is initialized (fork kills threads). The caller is responsible
/// for calling this before `#[tokio::main]` sets up the runtime.
///
/// The `Start` action does NOT actually launch the gateway — it only
/// daemonizes. The caller should proceed to start the gateway after
/// `run(DaemonAction::Start)` returns `Ok(())`.
///
/// # Arguments
///
/// * `action` - Which daemon action to perform.
/// * `config` - The loaded configuration (for PID file paths, etc.).
pub fn run(action: &DaemonAction, config: &Config) -> Result<()> {
    match action {
        DaemonAction::Start => do_start(config),
        DaemonAction::Stop => do_stop(config),
        DaemonAction::Restart => do_restart(config),
        DaemonAction::Status => do_status(config),
    }
}

/// Perform daemonization: double-fork, PID file, redirect stdio.
///
/// This must be called before the tokio runtime starts.
fn do_start(config: &Config) -> Result<()> {
    let pid_file_path = &config.daemon.pid_file;
    let log_dir = &config.daemon.log_dir;
    let working_dir = &config.daemon.working_directory;

    // Ensure log directory exists before daemonize (stderr redirect needs it)
    std::fs::create_dir_all(log_dir)?;

    // Create PID file with flock — prevents double-start
    let log_file_path = std::path::Path::new(log_dir).join("nanobot-rs.err");
    let log_file_str = log_file_path.to_str().unwrap_or("/dev/null");

    // PID file is created and locked here. We leak it intentionally —
    // the lock should persist for the daemon's lifetime, and the file
    // will be cleaned up on process exit via tempfile or explicit stop.
    nanobot_daemon::pid_file::PidFile::create(pid_file_path)?;

    // Daemonize: double-fork, setsid, chdir, redirect stdio
    nanobot_daemon::daemonize::daemonize(working_dir, Some(log_file_str))?;

    // Setup file logging in the daemon process
    let _guard = nanobot_daemon::logging::setup_file_logging(log_dir, "info");
    tracing::info!("Daemon started (pid={})", std::process::id());

    // NOTE: The caller should now start the gateway/serve.
    // The logging guard is intentionally leaked to keep file logging alive.
    std::mem::forget(_guard);

    Ok(())
}

/// Stop the running daemon by sending SIGTERM.
fn do_stop(config: &Config) -> Result<()> {
    let pid = match nanobot_daemon::pid_file::PidFile::read_pid(&config.daemon.pid_file)? {
        Some(pid) => pid,
        None => bail!("No PID file found at {} — is the daemon running?", config.daemon.pid_file),
    };

    if !nanobot_daemon::pid_file::is_process_running(pid) {
        // Stale PID file — clean it up
        let _ = std::fs::remove_file(&config.daemon.pid_file);
        bail!("Process {pid} is not running. Removed stale PID file.");
    }

    println!("Stopping daemon (pid={pid})...");
    let timeout = config.daemon.grace_period_secs;
    nanobot_daemon::signal::send_sigterm_and_wait(pid, timeout)?;
    println!("Daemon stopped.");

    // Clean up PID file
    let _ = std::fs::remove_file(&config.daemon.pid_file);
    Ok(())
}

/// Restart: stop the existing daemon, then start a new one.
fn do_restart(config: &Config) -> Result<()> {
    // Try to stop — ignore errors if not running
    if nanobot_daemon::pid_file::PidFile::read_pid(&config.daemon.pid_file)?.is_some() {
        let _ = do_stop(config);
    }
    do_start(config)
}

/// Check the daemon's status.
fn do_status(config: &Config) -> Result<()> {
    match nanobot_daemon::pid_file::PidFile::read_pid(&config.daemon.pid_file)? {
        Some(pid) => {
            if nanobot_daemon::pid_file::is_process_running(pid) {
                println!("Daemon is running (pid={pid})");
            } else {
                println!("Daemon is NOT running (stale PID file: pid={pid})");
            }
        }
        None => {
            println!("Daemon is NOT running (no PID file)");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_action_equality() {
        assert_eq!(DaemonAction::Start, DaemonAction::Start);
        assert_eq!(DaemonAction::Stop, DaemonAction::Stop);
        assert_ne!(DaemonAction::Start, DaemonAction::Stop);
    }

    #[test]
    fn test_status_no_pid_file() {
        let config = Config::default();
        // Default PID file shouldn't exist — status should succeed but report not running
        let result = do_status(&config);
        // This succeeds (prints "not running"), doesn't error
        assert!(result.is_ok());
    }
}
