//! Daemon subcommand — manage the kestrel background daemon.
//!
//! Provides `start`, `stop`, `restart`, and `status` actions for controlling
//! the daemonized kestrel process.

use anyhow::{bail, Result};
use kestrel_config::Config;

/// Handles returned by [`do_start`] that must live for the daemon's lifetime.
///
/// Dropping `DaemonHandles` flushes remaining log lines (via `log_guard`)
/// then releases the PID file lock.
pub struct DaemonHandles {
    /// PID file holding the flock — released on drop.
    pub pid_file: kestrel_daemon::pid_file::PidFile,
    /// Non-blocking log writer guard — flushes remaining logs on drop.
    pub log_guard: kestrel_daemon::logging::LogGuard,
}

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

/// Handle a daemon CLI subcommand.
///
/// Maps from the string-based subcommand name to the typed [`DaemonAction`]
/// and dispatches accordingly.
///
/// # Arguments
///
/// * `action` - Which daemon action to perform.
/// * `config` - The loaded configuration (for PID file paths, etc.).
///
/// # Returns
///
/// For `DaemonAction::Start`, returns `Ok(Some(DaemonHandles))` so the caller
/// can hold them for the process lifetime and drop on shutdown (flushing logs).
/// For other actions, returns `Ok(None)`.
pub fn handle_daemon_command(
    action: DaemonAction,
    config: Config,
) -> Result<Option<DaemonHandles>> {
    match action {
        DaemonAction::Start => do_start(&config).map(Some),
        DaemonAction::Stop => do_stop(&config).map(|()| None),
        DaemonAction::Restart => do_restart(&config).map(|()| None),
        DaemonAction::Status => do_status(&config).map(|()| None),
    }
}

/// Perform daemonization: double-fork, PID file, redirect stdio, file logging.
///
/// This must be called before the tokio runtime starts. After daemonize
/// returns, the caller is the grandchild process (the actual daemon).
/// The PID file is created AFTER daemonize to ensure it contains the
/// correct (grandchild) PID.
///
/// Returns [`DaemonHandles`] whose `log_guard` and `pid_file` must be held
/// for the daemon's lifetime and dropped on shutdown.
fn do_start(config: &Config) -> Result<DaemonHandles> {
    let pid_file_path = &config.daemon.pid_file;
    let log_dir = &config.daemon.log_dir;
    let working_dir = &config.daemon.working_directory;

    // Ensure log directory exists before daemonize (stderr redirect needs it)
    std::fs::create_dir_all(log_dir)?;

    let log_file_path = std::path::Path::new(log_dir).join("kestrel.err");
    let log_file_str = log_file_path.to_str().unwrap_or("/dev/null");

    // Daemonize FIRST: double-fork, setsid, chdir, redirect stdio.
    // After this returns, we are the grandchild (the actual daemon) with a NEW PID.
    kestrel_daemon::daemonize::daemonize(working_dir, Some(log_file_str))?;

    // NOW create PID file — after daemonize, so the PID is the grandchild's.
    // flock prevents double-start: if another instance holds the lock, we fail.
    // But first, clean up stale PID file from a previous crashed instance.
    if let Ok(Some(old_pid)) = kestrel_daemon::pid_file::PidFile::read_pid(pid_file_path) {
        if !kestrel_daemon::pid_file::is_process_running(old_pid) {
            let _ = std::fs::remove_file(pid_file_path);
            // No tracing subscriber yet — stderr is redirected to log file
            eprintln!("Cleaned stale PID file from crashed instance (pid={old_pid})");
        }
    }
    let pid_file = kestrel_daemon::pid_file::PidFile::create(pid_file_path)?;

    // Setup file logging in the daemon process
    let log_guard = kestrel_daemon::logging::setup_file_logging(log_dir, "info")?;
    tracing::info!("Daemon started (pid={})", std::process::id());

    Ok(DaemonHandles {
        pid_file,
        log_guard,
    })
}

/// Stop the running daemon by sending SIGTERM.
fn do_stop(config: &Config) -> Result<()> {
    let pid = match kestrel_daemon::pid_file::PidFile::read_pid(&config.daemon.pid_file)? {
        Some(pid) => pid,
        None => {
            bail!(
                "No PID file found at {} — is the daemon running?",
                config.daemon.pid_file
            )
        }
    };

    if !kestrel_daemon::pid_file::is_process_running(pid) {
        // Stale PID file — clean it up
        let _ = std::fs::remove_file(&config.daemon.pid_file);
        bail!("Process {pid} is not running. Removed stale PID file.");
    }

    println!("Stopping daemon (pid={pid})...");
    let timeout = config.daemon.grace_period_secs;
    kestrel_daemon::signal::send_sigterm_and_wait(pid, timeout)?;
    println!("Daemon stopped.");

    // Clean up PID file
    let _ = std::fs::remove_file(&config.daemon.pid_file);
    Ok(())
}

/// Restart: stop the existing daemon, then re-exec `daemon start`.
///
/// Cannot call `do_start` directly because the gateway launch logic lives
/// in `main.rs`. Instead, we re-exec ourselves with the `daemon start`
/// subcommand so the new process follows the full startup path.
fn do_restart(config: &Config) -> Result<()> {
    // Try to stop — ignore errors if not running
    if kestrel_daemon::pid_file::PidFile::read_pid(&config.daemon.pid_file)?.is_some() {
        let _ = do_stop(config);
    }

    // Re-exec: rebuild args replacing "restart" with "start"
    let exe = std::env::current_exe()?;
    let args: Vec<String> = std::env::args()
        .skip(1) // skip program name
        .map(|a| {
            if a == "restart" {
                "start".to_string()
            } else {
                a
            }
        })
        .collect();

    let child = std::process::Command::new(&exe).args(&args).spawn()?;
    println!("Restarted daemon (new process pid={})", child.id());
    Ok(())
}

/// Check the daemon's status.
fn do_status(config: &Config) -> Result<()> {
    match kestrel_daemon::pid_file::PidFile::read_pid(&config.daemon.pid_file)? {
        Some(pid) => {
            if kestrel_daemon::pid_file::is_process_running(pid) {
                println!("Daemon is running (pid={pid})");
            } else {
                // Auto-clean stale PID file
                let _ = std::fs::remove_file(&config.daemon.pid_file);
                println!("Daemon is NOT running (cleaned stale PID file: pid={pid})");
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
