//! Unix daemonization via double-fork.
//!
//! Follows the classic daemonization pattern:
//! 1. First fork — parent exits, child continues
//! 2. `setsid()` — create new session
//! 3. Second fork — ensure process is not a session leader
//! 4. `chdir("/")` + `umask(0)` — detach from filesystem
//! 5. Redirect stdin/stdout/stderr to `/dev/null`
//!
//! **Critical**: Must run BEFORE the tokio runtime starts, because `fork()`
//! only duplicates the calling thread — all other threads (including tokio's)
//! are lost in the child.

use anyhow::{Context, Result};
use nix::sys::stat::{umask, Mode};
use nix::unistd::{chdir, close, dup2, fork, setsid, ForkResult};
use std::fs::File;
use std::os::unix::io::{AsRawFd, FromRawFd};

/// Daemonize the current process using the classic double-fork technique.
///
/// After this function returns, the caller is running as a background daemon
/// with no controlling terminal, `cwd` set to `working_dir`, and stdio
/// redirected to `/dev/null`.
///
/// # Arguments
///
/// * `working_dir` - Directory to change to after daemonizing (typically `/`).
/// * `log_file` - Optional path to redirect stderr to (for post-daemonize errors).
///
/// # Errors
///
/// Returns an error if any fork, `setsid`, or I/O operation fails.
///
/// # Safety
///
/// This function must be called BEFORE any threads are spawned (including
/// the tokio runtime). The child process after `fork()` only has the calling
/// thread — all other threads vanish.
pub fn daemonize(working_dir: &str, log_file: Option<&str>) -> Result<()> {
    // First fork
    match unsafe { fork() }.context("first fork")? {
        ForkResult::Parent { .. } => {
            // Parent exits — the caller sees the process end
            std::process::exit(0);
        }
        ForkResult::Child => {}
    }

    // Child: create new session
    setsid().context("setsid")?;

    // Second fork — ensures process is not session leader (can't acquire terminal)
    match unsafe { fork() }.context("second fork")? {
        ForkResult::Parent { .. } => {
            std::process::exit(0);
        }
        ForkResult::Child => {}
    }

    // Set working directory and umask
    chdir(working_dir).context("chdir to working_dir")?;
    umask(Mode::from_bits(0o022).unwrap());

    // Redirect stdio to /dev/null (or log file for stderr)
    redirect_stdio(log_file)?;

    tracing::info!("Daemonized successfully (pid={})", std::process::id());
    Ok(())
}

/// Redirect stdin, stdout, and stderr to `/dev/null` or a log file.
///
/// Stdin and stdout go to `/dev/null`. Stderr goes to `log_file` if provided,
/// otherwise also to `/dev/null`.
fn redirect_stdio(log_file: Option<&str>) -> Result<()> {
    let devnull = File::open("/dev/null").context("open /dev/null")?;
    let devnull_fd = devnull.as_raw_fd();

    // stdin → /dev/null
    dup2(devnull_fd, 0).context("dup2 stdin")?;
    // stdout → /dev/null
    dup2(devnull_fd, 1).context("dup2 stdout")?;

    // stderr → log file or /dev/null
    if let Some(path) = log_file {
        let err_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .context("open log file for stderr")?;
        dup2(err_file.as_raw_fd(), 2).context("dup2 stderr to log file")?;
    } else {
        dup2(devnull_fd, 2).context("dup2 stderr")?;
    }

    // Close the original /dev/null fd (fds 0,1,2 are now the active ones)
    close(devnull_fd).ok();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemonize_does_not_panic_on_struct_creation() {
        // We cannot test actual fork in unit tests — just verify the function
        // signature and types compile correctly.
        // The real test is integration: `nanobot-rs daemon start`.
        let _working_dir = "/";
        let _log_file: Option<&str> = None;
    }

    #[test]
    fn test_redirect_stdio_compiles() {
        // Verify the redirect_stdio function is accessible and has the right signature.
        // We don't call it here because it modifies global fds.
        let _: fn(Option<&str>) -> Result<()> = redirect_stdio;
    }
}
