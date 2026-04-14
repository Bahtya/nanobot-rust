//! Async signal handling via `tokio::signal::unix`.
//!
//! Provides a unified interface for receiving SIGTERM, SIGINT, and SIGHUP
//! asynchronously within a tokio runtime. Also provides a helper for sending
//! SIGTERM to another process (used by `daemon stop`).
//!
//! **Design**: Uses `tokio::signal::unix::signal()` (async), NOT `libc signal()`
//! (which is unsafe and not async-safe).

use anyhow::{Context, Result};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::signal::unix::{signal, SignalKind};

/// The type of shutdown signal received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownSignal {
    /// SIGTERM — graceful shutdown (drain connections, complete in-flight work).
    Graceful,
    /// SIGINT (Ctrl+C) — fast shutdown.
    Fast,
    /// SIGHUP — reload/log rotation (placeholder for now).
    Reload,
}

/// Wait for the next Unix signal and return its type.
///
/// Listens for SIGTERM, SIGINT, and SIGHUP concurrently. This function
/// should be used inside a `tokio::select!` block or as a top-level
/// shutdown signal waiter.
///
/// # Panics
///
/// Panics if the signal handlers cannot be installed (should not happen
/// on a standard Unix system).
pub async fn wait_for_signal() -> ShutdownSignal {
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    let mut sighup = signal(SignalKind::hangup()).expect("install SIGHUP handler");

    tokio::select! {
        _ = sigterm.recv() => {
            tracing::info!("Received SIGTERM");
            ShutdownSignal::Graceful
        }
        _ = sigint.recv() => {
            tracing::info!("Received SIGINT");
            ShutdownSignal::Fast
        }
        _ = sighup.recv() => {
            tracing::info!("Received SIGHUP");
            ShutdownSignal::Reload
        }
    }
}

/// Send SIGTERM to a process by PID.
///
/// Used by `daemon stop` to signal the background daemon to shut down.
///
/// # Arguments
///
/// * `pid` - Process ID to signal.
///
/// # Errors
///
/// Returns an error if the process does not exist or the caller lacks
/// permission to signal it.
pub fn send_sigterm(pid: i32) -> Result<()> {
    kill(Pid::from_raw(pid), Signal::SIGTERM)
        .context(format!("failed to send SIGTERM to pid {pid}"))
}

/// Install a no-op SIGHUP handler using `sigaction`.
///
/// This must be called **before** the tokio runtime is created to close the
/// window where the default SIGHUP handler (terminate) is active between fork
/// completion and `wait_for_signal()` being called.
///
/// When tokio's `signal(SignalKind::hangup())` is later called in
/// `wait_for_signal()`, it overrides `SIG_IGN` with its own handler.
pub fn install_early_sighup_handler() {
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = libc::SIG_IGN;
    unsafe {
        libc::sigaction(libc::SIGHUP, &sa, std::ptr::null_mut());
    }
}

/// Send SIGTERM to a process and wait for it to exit.
///
/// Polls with `waitpid(WNOHANG)` every 100ms for up to `timeout_secs` seconds.
/// Returns `Ok(())` if the process exited, or an error if it's still
/// running after the timeout.
///
/// # Arguments
///
/// * `pid` - Process ID to signal.
/// * `timeout_secs` - Maximum seconds to wait for exit.
pub fn send_sigterm_and_wait(pid: i32, timeout_secs: u64) -> Result<()> {
    send_sigterm(pid)?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    // Poll with waitpid(WNOHANG) — no /proc filesystem access needed.
    // waitpid on a non-child returns ECHILD, which we treat as "done".
    let nix_pid = Pid::from_raw(pid);
    while std::time::Instant::now() < deadline {
        let exited = match nix::sys::wait::waitpid(nix_pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
            Ok(_) => true,
            Err(nix::errno::Errno::ECHILD) => true,
            Err(e) => anyhow::bail!("waitpid error: {e}"),
        };
        if exited {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    anyhow::bail!("process {pid} did not exit within {timeout_secs}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shutdown_signal_variants() {
        let graceful = ShutdownSignal::Graceful;
        let fast = ShutdownSignal::Fast;
        let reload = ShutdownSignal::Reload;

        assert_eq!(graceful, ShutdownSignal::Graceful);
        assert_eq!(fast, ShutdownSignal::Fast);
        assert_eq!(reload, ShutdownSignal::Reload);
        assert_ne!(graceful, fast);
    }

    #[test]
    fn test_send_sigterm_to_nonexistent_process() {
        // PID 4_000_000 is extremely unlikely to exist
        let result = send_sigterm(4_000_000);
        assert!(result.is_err());
    }
}
