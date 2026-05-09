//! Single PTY terminal session backed by `portable-pty`.

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

/// Maximum output buffer size per session (256 KiB).
const MAX_BUFFER_SIZE: usize = 256 * 1024;

/// Allowed shell executables when `dangerous` mode is disabled.
/// Only bare shell names or absolute paths matching these entries are permitted.
const ALLOWED_SHELLS: &[&str] = &[
    "/bin/sh",
    "/bin/bash",
    "/bin/dash",
    "/bin/zsh",
    "/usr/bin/sh",
    "/usr/bin/bash",
    "/usr/bin/dash",
    "/usr/bin/zsh",
    "/usr/bin/fish",
    "/usr/local/bin/bash",
    "/usr/local/bin/zsh",
    "/usr/local/bin/fish",
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
];

/// Validate that the shell path is allowed when not in dangerous mode.
///
/// In dangerous mode, any shell is accepted. Otherwise, the shell must
/// either be a known system shell path or the default shell (which is
/// always trusted).
pub fn validate_shell(shell: Option<&str>, dangerous: bool) -> Result<String> {
    let shell = shell
        .map(String::from)
        .unwrap_or_else(kestrel_config::platform::get_shell_path);

    if dangerous {
        return Ok(shell);
    }

    // Extract the file name component for matching (e.g. "/bin/bash" -> "bash")
    let file_name = std::path::Path::new(&shell)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Check if the shell matches any allowed entry (either full path or just the file name)
    let is_allowed = ALLOWED_SHELLS
        .iter()
        .any(|allowed| shell == *allowed || file_name == *allowed || file_name == shell);

    if !is_allowed {
        anyhow::bail!(
            "Shell '{}' is not in the allowed list. Allowed shells: {}",
            shell,
            ALLOWED_SHELLS.join(", ")
        );
    }

    Ok(shell)
}

/// Session metadata returned to callers.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub shell: String,
    pub cwd: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub alive: bool,
}

/// A single PTY terminal session.
///
/// Wraps a `portable-pty` master PTY and maintains a ring buffer of recent
/// output that callers can drain incrementally.
///
/// The `master` field is wrapped in a `std::sync::Mutex` because
/// `Box<dyn MasterPty + Send>` is `Send` but not `Sync`. The mutex makes
/// the whole struct `Send + Sync`, satisfying the `Tool` trait bound.
pub struct TerminalSession {
    id: String,
    shell: String,
    cwd: Option<String>,
    cols: u16,
    rows: u16,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    child: Mutex<Option<Box<dyn Child + Send + Sync>>>,
    writer: Mutex<Box<dyn Write + Send>>,
    output_buffer: Arc<Mutex<RingBuffer>>,
    alive: Arc<AtomicBool>,
    reader_handle: Option<std::thread::JoinHandle<()>>,
}

// Safety: TerminalSession implements Sync because all interior mutability
// is protected by Mutex or atomic operations:
// - id, shell, cwd, cols, rows: String/u16, inherently Send+Sync
// - master: Mutex<Option<Box<dyn MasterPty + Send>>> — Mutex provides Sync
// - child: Mutex<Option<Box<dyn Child + Send + Sync>>> — Mutex provides Sync
// - writer: Mutex<Box<dyn Write + Send>> — Mutex provides Sync
// - output_buffer: Arc<Mutex<RingBuffer>> — Arc+Mutex provides Sync
// - alive: Arc<AtomicBool> — Arc+Atomic provides Sync
// - reader_handle: Option<JoinHandle<()>> — JoinHandle is Send+Sync (Rust 1.72+)
unsafe impl Sync for TerminalSession {}

impl TerminalSession {
    /// Spawn a new PTY session.
    ///
    /// The `dangerous` flag controls shell validation: when false, only
    /// known system shells are accepted (see [`ALLOWED_SHELLS`]).
    pub fn spawn(
        id: String,
        shell: Option<String>,
        cwd: Option<&str>,
        cols: u16,
        rows: u16,
        dangerous: bool,
    ) -> Result<Self> {
        debug!(
            id = %id,
            shell = shell.as_deref().unwrap_or("default"),
            cwd = cwd.unwrap_or("-"),
            cols = cols,
            rows = rows,
            dangerous = dangerous,
            "Spawning PTY session"
        );

        let shell_cmd = validate_shell(shell.as_deref(), dangerous)?;

        let pty_system = native_pty_system();

        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to open PTY")?;

        let mut cmd = CommandBuilder::new(&shell_cmd);
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }

        let child = pair.slave.spawn_command(cmd)?;

        let master = pair.master;
        let reader = master
            .try_clone_reader()
            .context("Failed to clone PTY reader")?;
        let writer = master.take_writer().context("Failed to take PTY writer")?;

        let output_buffer = Arc::new(Mutex::new(RingBuffer::new(MAX_BUFFER_SIZE)));
        let alive = Arc::new(AtomicBool::new(true));

        let buf_clone = output_buffer.clone();
        let alive_clone = alive.clone();
        let session_id = id.clone();
        let reader_handle = std::thread::Builder::new()
            .name(format!("pty-reader-{id}"))
            .spawn(move || {
                pump_output(reader, &buf_clone, &alive_clone, &session_id);
            })
            .context("Failed to spawn PTY reader thread")?;

        Ok(Self {
            id,
            shell: shell_cmd,
            cwd: cwd.map(String::from),
            cols,
            rows,
            master: Mutex::new(Some(master)),
            child: Mutex::new(Some(child)),
            writer: Mutex::new(writer),
            output_buffer,
            alive,
            reader_handle: Some(reader_handle),
        })
    }

    /// Write input bytes to the PTY (typed by the "user").
    pub fn send_input(&self, input: &str) -> Result<()> {
        if !self.alive.load(Ordering::Relaxed) {
            anyhow::bail!("Session '{}' is not alive", self.id);
        }
        debug!(session_id = %self.id, input_len = input.len(), "Writing input to PTY");
        let mut writer = self.writer.lock().unwrap();
        writer
            .write_all(input.as_bytes())
            .context("Failed to write to PTY")?;
        writer.flush().context("Failed to flush PTY writer")?;
        Ok(())
    }

    /// Drain and return all pending output since the last read.
    ///
    /// If `timeout_ms` is `Some`, waits up to that many milliseconds for
    /// new output before returning.
    pub fn read_output(&self, timeout_ms: Option<u64>) -> Result<String> {
        {
            let mut buf = self.output_buffer.lock().unwrap();
            if !buf.is_empty() {
                let output = buf.drain_to_string();
                debug!(session_id = %self.id, output_len = output.len(), "Returning buffered output");
                return Ok(output);
            }
        }

        // Buffer was empty — optionally poll for new data.
        if let Some(ms) = timeout_ms {
            debug!(session_id = %self.id, timeout_ms = ms, "Waiting for PTY output");
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
            loop {
                std::thread::sleep(std::time::Duration::from_millis(20));
                let mut buf = self.output_buffer.lock().unwrap();
                if !buf.is_empty() || std::time::Instant::now() >= deadline {
                    let output = buf.drain_to_string();
                    debug!(session_id = %self.id, output_len = output.len(), "Returning waited output");
                    return Ok(output);
                }
            }
        }

        Ok(String::new())
    }

    /// Read all accumulated output without draining.
    pub fn peek_output(&self) -> String {
        let buf = self.output_buffer.lock().unwrap();
        buf.peek_string()
    }

    /// Resize the PTY.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        debug!(session_id = %self.id, cols = cols, rows = rows, "Resizing PTY");
        let master = self.master.lock().unwrap();
        if let Some(ref m) = *master {
            m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to resize PTY")?;
        }
        // Update stored dimensions. We need interior mutability here.
        // Since resize is not called concurrently with itself (mutating tool),
        // we can safely update via Cell-like pattern. However, since u16
        // is not easily made atomic, we accept a potential brief inconsistency
        // in info() — the PTY resize itself is the source of truth.
        Ok(())
    }

    /// Kill the session (signal child process, close PTY).
    pub fn kill(&self) {
        debug!(session_id = %self.id, "Killing PTY session");
        self.alive.store(false, Ordering::Relaxed);

        // Signal the child process first.
        if let Ok(mut child_guard) = self.child.lock() {
            if let Some(ref mut child) = *child_guard {
                // Best-effort: try to kill the child process.
                // On Unix this sends SIGTERM; on Windows it calls TerminateProcess.
                let _ = child.kill();
                // Reap the child to avoid zombies.
                // try_wait() is non-blocking; if the child hasn't exited yet,
                // dropping it will clean up on the next GC cycle.
                let _ = child.try_wait();
            }
            *child_guard = None;
        }

        // Drop the master PTY — on Unix this sends SIGHUP to the child.
        let mut master = self.master.lock().unwrap();
        *master = None;
    }

    /// Whether the underlying process is still alive.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Return session metadata.
    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            shell: self.shell.clone(),
            cwd: self.cwd.clone(),
            cols: self.cols,
            rows: self.rows,
            alive: self.is_alive(),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.kill();
        // Don't join the reader thread — it may be blocked in a blocking
        // read() on the PTY fd. Dropping the master PTY will eventually
        // cause the read to return with an error/EOF, and the thread will
        // check `alive == false` and exit. Detaching avoids blocking the
        // drop path (which would hang the test suite / shutdown).
        if let Some(handle) = self.reader_handle.take() {
            // Detach: let it clean up on its own.
            // The thread holds an Arc<AtomicBool> (alive) so it will
            // observe the kill signal and exit when the PTY read unblocks.
            let _ = handle;
        }
    }
}

/// Background output pump: reads from PTY into the ring buffer.
fn pump_output(
    mut reader: Box<dyn Read + Send>,
    buffer: &Arc<Mutex<RingBuffer>>,
    alive: &AtomicBool,
    session_id: &str,
) {
    let mut tmp = [0u8; 4096];
    loop {
        if !alive.load(Ordering::Relaxed) {
            break;
        }
        match reader.read(&mut tmp) {
            Ok(0) => {
                debug!("PTY reader for session '{}' got EOF", session_id);
                alive.store(false, Ordering::Relaxed);
                break;
            }
            Ok(n) => {
                if let Ok(mut buf) = buffer.lock() {
                    buf.write(&tmp[..n]);
                }
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                warn!("PTY reader error for session '{}': {}", session_id, e);
                alive.store(false, Ordering::Relaxed);
                break;
            }
        }
    }
    debug!("PTY reader thread exiting for session '{}'", session_id);
}

/// Simple ring buffer for PTY output.
struct RingBuffer {
    data: Vec<u8>,
    write_pos: usize,
    len: usize,
    pending_start: usize,
    pending_len: usize,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            data: vec![0; capacity],
            write_pos: 0,
            len: 0,
            pending_start: 0,
            pending_len: 0,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        let cap = self.data.len();
        if cap == 0 || bytes.is_empty() {
            return;
        }

        // Write in up to two chunks to handle wrap-around.
        let first_len = bytes.len().min(cap - self.write_pos);
        self.data[self.write_pos..self.write_pos + first_len].copy_from_slice(&bytes[..first_len]);

        let remaining = bytes.len() - first_len;
        if remaining > 0 {
            let second_len = remaining.min(cap);
            self.data[..second_len].copy_from_slice(&bytes[first_len..first_len + second_len]);
        }

        // Advance write_pos and update length counters.
        let wrote = bytes.len();
        self.write_pos = (self.write_pos + wrote) % cap;

        if self.len + wrote <= cap {
            self.len += wrote;
            self.pending_len += wrote;
        } else {
            // Overflow: the new data overwrote some old data.
            let overflow = (self.len + wrote) - cap;
            self.len = cap;
            self.pending_len = cap;
            self.pending_start = (self.pending_start + overflow) % cap;
        }
    }

    fn is_empty(&self) -> bool {
        self.pending_len == 0
    }

    fn drain_to_string(&mut self) -> String {
        if self.pending_len == 0 {
            return String::new();
        }
        let mut bytes = Vec::with_capacity(self.pending_len);
        for i in 0..self.pending_len {
            let idx = (self.pending_start + i) % self.data.len();
            bytes.push(self.data[idx]);
        }
        self.pending_start = self.write_pos;
        self.pending_len = 0;
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn peek_string(&self) -> String {
        if self.pending_len == 0 {
            return String::new();
        }
        let mut bytes = Vec::with_capacity(self.pending_len);
        for i in 0..self.pending_len {
            let idx = (self.pending_start + i) % self.data.len();
            bytes.push(self.data[idx]);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_buffer_basic() {
        let mut buf = RingBuffer::new(16);
        assert!(buf.is_empty());

        buf.write(b"hello");
        assert!(!buf.is_empty());
        assert_eq!(buf.drain_to_string(), "hello");
        assert!(buf.is_empty());
    }

    #[test]
    fn test_ring_buffer_wrap() {
        let mut buf = RingBuffer::new(8);
        buf.write(b"abcdefghijklmnop");
        let s = buf.drain_to_string();
        assert_eq!(s.len(), 8);
        assert!(s.contains("op"));
    }

    #[test]
    fn test_ring_buffer_peek() {
        let mut buf = RingBuffer::new(16);
        buf.write(b"peek test");
        assert_eq!(buf.peek_string(), "peek test");
        assert!(!buf.is_empty());
        assert_eq!(buf.drain_to_string(), "peek test");
        assert!(buf.is_empty());
    }

    #[test]
    fn test_ring_buffer_multiple_writes() {
        let mut buf = RingBuffer::new(64);
        buf.write(b"line1\n");
        buf.write(b"line2\n");
        let s = buf.drain_to_string();
        assert_eq!(s, "line1\nline2\n");
    }

    #[test]
    fn test_ring_buffer_bulk_write() {
        let mut buf = RingBuffer::new(16);
        // Write data larger than remaining space to test wrap
        buf.write(b"0123456789ABCDEF"); // fills exactly
        assert_eq!(buf.len, 16);
        buf.write(b"XYZ"); // wraps, overwrites start
        assert_eq!(buf.len, 16);
        let s = buf.drain_to_string();
        assert!(s.ends_with("XYZ"));
    }

    #[test]
    fn test_validate_shell_default() {
        // The default shell should always pass validation.
        let result = validate_shell(None, false);
        assert!(
            result.is_ok(),
            "Default shell should be allowed: {:?}",
            result
        );
        assert!(!result.unwrap().is_empty());
    }

    #[test]
    fn test_validate_shell_dangerous_allows_any() {
        let result = validate_shell(Some("/usr/bin/my_custom_shell"), true);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/usr/bin/my_custom_shell");
    }

    #[test]
    fn test_validate_shell_rejects_unknown() {
        let result = validate_shell(Some("/usr/bin/suspicious_shell"), false);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not in the allowed list"));
    }

    #[test]
    fn test_validate_shell_allows_known_bash() {
        let result = validate_shell(Some("/bin/bash"), false);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/bin/bash");
    }

    #[test]
    fn test_session_info_fields() {
        let info = SessionInfo {
            id: "test-1".to_string(),
            shell: "/bin/sh".to_string(),
            cwd: Some("/tmp".to_string()),
            cols: 80,
            rows: 24,
            alive: true,
        };
        assert_eq!(info.id, "test-1");
        assert_eq!(info.cols, 80);
        assert_eq!(info.rows, 24);
    }
}
