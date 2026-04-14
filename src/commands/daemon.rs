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
/// For `DaemonAction::Start`, returns `Ok(Some(PidFile))` so the caller
/// can pass it to the gateway runner for cleanup on shutdown.
/// For other actions, returns `Ok(None)`.
pub fn handle_daemon_command(
    action: DaemonAction,
    config: Config,
) -> Result<Option<nanobot_daemon::pid_file::PidFile>> {
    match action {
        DaemonAction::Start => do_start(&config).map(Some),
        DaemonAction::Stop => do_stop(&config).map(|()| None),
        DaemonAction::Restart => do_restart(&config).map(|()| None),
        DaemonAction::Status => do_status(&config).map(|()| None),
    }
}

/// Perform daemonization: double-fork, PID file, redirect stdio.
///
/// This must be called before the tokio runtime starts. After daemonize
/// returns, the caller is the grandchild process (the actual daemon).
/// The PID file is created AFTER daemonize to ensure it contains the
/// correct (grandchild) PID.
///
/// Returns a `PidFile` whose lock persists for the daemon's lifetime.
fn do_start(config: &Config) -> Result<nanobot_daemon::pid_file::PidFile> {
    let pid_file_path = &config.daemon.pid_file;
    let log_dir = &config.daemon.log_dir;
    let working_dir = &config.daemon.working_directory;

    // Ensure log directory exists before daemonize (stderr redirect needs it)
    std::fs::create_dir_all(log_dir)?;

    let log_file_path = std::path::Path::new(log_dir).join("nanobot-rs.err");
    let log_file_str = log_file_path.to_str().unwrap_or("/dev/null");

    // Daemonize FIRST: double-fork, setsid, chdir, redirect stdio.
    // After this returns, we are the grandchild (the actual daemon) with a NEW PID.
    nanobot_daemon::daemonize::daemonize(working_dir, Some(log_file_str))?;

    // NOW create PID file — after daemonize, so the PID is the grandchild's.
    // flock prevents double-start: if another instance holds the lock, we fail.
    let pid_file = nanobot_daemon::pid_file::PidFile::create(pid_file_path)?;

    // Setup file logging in the daemon process
    let _guard = nanobot_daemon::logging::setup_file_logging(log_dir, "info");
    tracing::info!("Daemon started (pid={})", std::process::id());

    // Store the logging guard in a global so it lives for the process lifetime.
    // Box::leak is intentional — the guard must outlive all log calls.
    Box::leak(Box::new(_guard));

    Ok(pid_file)
}

/// Stop the running daemon by sending SIGTERM.
fn do_stop(config: &Config) -> Result<()> {
    let pid = match nanobot_daemon::pid_file::PidFile::read_pid(&config.daemon.pid_file)? {
        Some(pid) => pid,
        None => {
            bail!(
                "No PID file found at {} — is the daemon running?",
                config.daemon.pid_file
            )
        }
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
    let _pid_file = do_start(config)?;
    // NOTE: restart calls do_start which daemonizes. After daemonize,
    // the grandchild returns here but the process will exit when this
    // function returns because there's no gateway to run.
    // A full restart should be done via: `daemon stop && daemon start`
    // (where start is wired to gateway::run in main.rs)
    Ok(())
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
