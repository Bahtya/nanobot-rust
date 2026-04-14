//! PID file management with `flock`-based atomic locking.
//!
//! Uses `flock(LOCK_EX | LOCK_NB)` for atomic exclusive locking, which
//! prevents race conditions compared to "check if file exists" approaches.
//!
//! The `PidFile` struct holds the locked file handle. When dropped, the lock
//! is released automatically. Call `clean()` to also unlink the file.

use anyhow::{bail, Context, Result};
use nix::fcntl::{flock, FlockArg};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

/// A PID file with an exclusive `flock` held for the lifetime of this struct.
///
/// The file is created (or opened) and locked atomically. If another process
/// holds the lock, creation fails immediately.
///
/// # Drop behavior
///
/// When dropped, the `flock` is released. The PID file is **not** deleted
/// automatically — call [`PidFile::clean()`] to remove it on graceful shutdown.
pub struct PidFile {
    /// Path to the PID file on disk.
    path: String,
    /// The locked file handle. Kept open to hold the flock.
    _file: File,
}

impl PidFile {
    /// Create and lock a PID file atomically.
    ///
    /// Writes the current process ID to the file and acquires an exclusive,
    /// non-blocking `flock`. If another process already holds the lock, this
    /// returns an error.
    ///
    /// # Arguments
    ///
    /// * `path` - Filesystem path for the PID file (e.g. `/run/nanobot-rs.pid`).
    ///
    /// # Errors
    ///
    /// - If the parent directory does not exist.
    /// - If another process holds the `flock` (double-start prevention).
    /// - If I/O operations fail.
    pub fn create(path: &str) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(parent).context("create PID file parent directory")?;
        }

        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(path)
            .context("open PID file")?;

        // Acquire exclusive, non-blocking lock
        flock(&file, FlockArg::LockExclusiveNonblock).context(
            "failed to lock PID file — another instance may be running",
        )?;

        // Write current PID
        let pid = std::process::id();
        write!(&file, "{pid}").context("write PID to file")?;
        file.sync_all().context("sync PID file")?;

        tracing::info!("PID file created and locked: {path} (pid={pid})");

        Ok(Self {
            path: path.to_string(),
            _file: file,
        })
    }

    /// Read the PID from a file without acquiring a lock.
    ///
    /// Returns `Ok(None)` if the file does not exist.
    ///
    /// # Arguments
    ///
    /// * `path` - Filesystem path for the PID file.
    pub fn read_pid(path: &str) -> Result<Option<i32>> {
        if !Path::new(path).exists() {
            return Ok(None);
        }

        let mut file = File::open(path).context("open PID file for reading")?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .context("read PID file")?;

        let pid: i32 = contents
            .trim()
            .parse()
            .context("parse PID from file")?;

        Ok(Some(pid))
    }

    /// Remove the PID file and release the lock.
    ///
    /// Consumes `self`. This is the clean-shutdown path: unlink the file
    /// and drop the file handle (which releases the flock).
    pub fn clean(self) -> Result<()> {
        let path = self.path.clone();
        drop(self); // Release the flock first

        if Path::new(&path).exists() {
            fs::remove_file(&path).context("remove PID file")?;
            tracing::info!("PID file removed: {path}");
        }

        Ok(())
    }

    /// Returns the path to the PID file.
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// Check whether a process with the given PID is currently running.
///
/// Uses `/proc/{pid}` on Linux. Returns `false` if the `/proc` entry
/// doesn't exist.
pub fn is_process_running(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_pid_file_create_and_read() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.pid");
        let path_str = path.to_str().unwrap();

        let pf = PidFile::create(path_str).unwrap();
        assert_eq!(pf.path(), path_str);

        // Read PID from file
        let pid = PidFile::read_pid(path_str).unwrap();
        assert!(pid.is_some());
        assert_eq!(pid.unwrap() as u32, std::process::id());

        pf.clean().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn test_pid_file_read_nonexistent() {
        let pid = PidFile::read_pid("/tmp/nonexistent_nanobot_test.pid").unwrap();
        assert!(pid.is_none());
    }

    #[test]
    fn test_pid_file_double_lock_fails() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("double.pid");
        let path_str = path.to_str().unwrap();

        let _first = PidFile::create(path_str).unwrap();
        // Second lock attempt should fail (LOCK_NB)
        let result = PidFile::create(path_str);
        assert!(result.is_err());
    }

    #[test]
    fn test_pid_file_clean_removes_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("cleanup.pid");
        let path_str = path.to_str().unwrap();

        let pf = PidFile::create(path_str).unwrap();
        assert!(path.exists());

        pf.clean().unwrap();
        assert!(!path.exists());

        // After clean, a new lock can be acquired
        let pf2 = PidFile::create(path_str).unwrap();
        pf2.clean().unwrap();
    }

    #[test]
    fn test_is_process_running() {
        // Our own PID should be running
        assert!(is_process_running(std::process::id() as i32));
        // PID -1 definitely doesn't exist
        assert!(!is_process_running(-1));
        // Very high PID is extremely unlikely
        assert!(!is_process_running(4_000_000));
    }

    #[test]
    fn test_pid_file_creates_parent_directory() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a").join("b").join("test.pid");
        let path_str = nested.to_str().unwrap();

        let pf = PidFile::create(path_str).unwrap();
        assert!(nested.exists());
        pf.clean().unwrap();
    }
}
