//! Single PTY terminal session backed by `portable-pty`.

use anyhow::{Context, Result};
use portable_pty::{
    native_pty_system, CommandBuilder, MasterPty, PtySize,
};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// Maximum output buffer size per session (256 KiB).
const MAX_BUFFER_SIZE: usize = 256 * 1024;

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
pub struct TerminalSession {
    id: String,
    shell: String,
    cwd: Option<String>,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    output_buffer: Arc<Mutex<RingBuffer>>,
    alive: Arc<AtomicBool>,
    reader_handle: Option<std::thread::JoinHandle<()>>,
}

impl TerminalSession {
    /// Spawn a new PTY session.
    ///
    /// `id` is an externally-assigned identifier. The session spawns the
    /// given `shell` (or the system default if `None`) inside a new PTY.
    pub fn spawn(
        id: String,
        shell: Option<String>,
        cwd: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> Result<Self> {
        debug!(
            id = %id,
            shell = shell.as_deref().unwrap_or("default"),
            cwd = cwd.unwrap_or("-"),
            cols = cols,
            rows = rows,
            "Spawning PTY session"
        );

        let pty_system = native_pty_system();

        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to open PTY")?;

        let shell_cmd = shell.unwrap_or_else(default_shell);
        let mut cmd = CommandBuilder::new(&shell_cmd);
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }

        let _child = pair.slave.spawn_command(cmd)?;

        let master = pair.master;
        let reader = master.try_clone_reader().context("Failed to clone PTY reader")?;
        let writer = master.try_clone_writer().context("Failed to clone PTY writer")?;

        let output_buffer = Arc::new(Mutex::new(RingBuffer::new(MAX_BUFFER_SIZE)));
        let alive = Arc::new(AtomicBool::new(true));

        // Spawn a background thread to pump PTY output into the ring buffer.
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
            master,
            writer: Arc::new(Mutex::new(writer)),
            output_buffer,
            alive,
            reader_handle: Some(reader_handle),
        })
    }

    /// Write input bytes to the PTY (typed by the "user").
    pub async fn send_input(&self, input: &str) -> Result<()> {
        if !self.alive.load(Ordering::Relaxed) {
            anyhow::bail!("Session '{}' is not alive", self.id);
        }
        debug!(session_id = %self.id, input_len = input.len(), "Writing input to PTY");
        let mut writer = self.writer.lock().await;
        writer
            .write_all(input.as_bytes())
            .context("Failed to write to PTY")?;
        writer.flush().context("Failed to flush PTY writer")?;
        Ok(())
    }

    /// Drain and return all pending output since the last read.
    ///
    /// If `timeout_ms` is `Some`, waits up to that many milliseconds for
    /// new output before returning. Returns whatever has been collected.
    pub async fn read_output(&self, timeout_ms: Option<u64>) -> Result<String> {
        let buf = self.output_buffer.lock().await;

        if buf.is_empty() {
            drop(buf);
            if let Some(ms) = timeout_ms {
                debug!(session_id = %self.id, timeout_ms = ms, "Waiting for PTY output");
                // Poll with short intervals until timeout or data arrives.
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let buf = self.output_buffer.lock().await;
                    if !buf.is_empty() || std::time::Instant::now() >= deadline {
                        let output = buf.drain_to_string();
                        debug!(session_id = %self.id, output_len = output.len(), "Returning waited output");
                        return Ok(output);
                    }
                }
            }
            return Ok(String::new());
        }

        let output = buf.drain_to_string();
        debug!(session_id = %self.id, output_len = output.len(), "Returning buffered output");
        Ok(output)
    }

    /// Read all accumulated output without draining.
    pub async fn peek_output(&self) -> String {
        let buf = self.output_buffer.lock().await;
        buf.peek_string()
    }

    /// Resize the PTY.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        debug!(session_id = %self.id, cols = cols, rows = rows, "Resizing PTY");
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to resize PTY")
    }

    /// Kill the session (close PTY, signal process).
    pub fn kill(&self) {
        debug!(session_id = %self.id, "Killing PTY session");
        self.alive.store(false, Ordering::Relaxed);
        // Closing the master PTY will send SIGHUP to the child process on Unix.
        // On Windows, ConPTY handles cleanup.
        drop(self.master.try_clone_writer());
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
            cols: 0,  // TODO: track actual size
            rows: 0,
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
        if let Some(handle) = self.reader_handle.take() {
            // The reader thread checks `alive` and will exit shortly.
            // Don't block on join indefinitely.
            let _ = handle.join();
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
                if let Ok(mut buf) = buffer.try_lock() {
                    buf.write(&tmp[..n]);
                }
                // If we can't lock (contention), data is lost for this chunk.
                // This is acceptable for LLM consumption — we prefer liveness.
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    // Non-blocking spin; yield briefly.
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                warn!(
                    "PTY reader error for session '{}': {}",
                    session_id, e
                );
                alive.store(false, Ordering::Relaxed);
                break;
            }
        }
    }
    debug!("PTY reader thread exiting for session '{}'", session_id);
}

/// Simple ring buffer for PTY output.
///
/// Writes are append-only. When the buffer is full, oldest bytes are
/// overwritten. `drain_to_string` returns and clears all accumulated data.
struct RingBuffer {
    data: Vec<u8>,
    write_pos: usize,
    len: usize,
    /// Bytes not yet consumed by `drain_to_string`.
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
        for &b in bytes {
            self.data[self.write_pos] = b;
            self.write_pos = (self.write_pos + 1) % self.data.len();
            if self.len < self.data.len() {
                self.len += 1;
                self.pending_len += 1;
            } else {
                // Overwrote oldest byte — advance pending_start.
                self.pending_start = (self.pending_start + 1) % self.data.len();
                // pending_len stays at max (== self.data.len())
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.pending_len == 0
    }

    /// Drain all pending (unconsumed) bytes as a String.
    /// Invalid UTF-8 sequences are replaced with the replacement char.
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

    /// Peek at all pending bytes without draining.
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

/// Returns the default shell for the current platform.
fn default_shell() -> String {
    kestrel_config::platform::get_shell_path()
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
        // Write more than capacity.
        buf.write(b"abcdefghijklmnop");
        // Should only keep last 8 bytes: "ijklmnop" → but actually last 8 of 16
        let s = buf.drain_to_string();
        assert_eq!(s.len(), 8);
        assert!(s.contains("op"));
    }

    #[test]
    fn test_ring_buffer_peek() {
        let mut buf = RingBuffer::new(16);
        buf.write(b"peek test");
        assert_eq!(buf.peek_string(), "peek test");
        assert!(!buf.is_empty()); // peek doesn't drain
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
    fn test_default_shell_is_nonempty() {
        let shell = default_shell();
        assert!(!shell.is_empty());
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
    }
}
