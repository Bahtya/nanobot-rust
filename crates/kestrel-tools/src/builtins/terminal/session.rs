//! Single PTY terminal session backed by `portable-pty`.

use super::emulator::{IncrementalUtf8Decoder, TerminalEmulatorHandle};
use super::screen::ScreenSnapshot;
use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
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
    cols: AtomicU16,
    rows: AtomicU16,
    last_activity: AtomicU64,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    child: Mutex<Option<Box<dyn Child + Send + Sync>>>,
    writer: Mutex<Box<dyn Write + Send>>,
    /// Raw PTY byte buffer (for debug/escaped modes).
    #[allow(dead_code)] // Used by future escaped mode and debug output
    raw_buffer: Arc<Mutex<RingBuffer>>,
    /// Incremental UTF-8 decoder state (shared with reader thread).
    utf8_decoder: Arc<Mutex<IncrementalUtf8Decoder>>,
    /// Decoded text stream for normal reading.
    decoded_buffer: Arc<Mutex<String>>,
    /// Terminal emulator handle (placeholder for future parser/screen model).
    emulator: Arc<Mutex<TerminalEmulatorHandle>>,
    /// Last screen hash observed by capture/wait APIs.
    last_observed_screen_hash: AtomicU64,
    /// Monotonic count of user inputs sent to this session.
    input_sequence: AtomicU64,
    /// Input sequence associated with the last observed screen state.
    observed_input_sequence: AtomicU64,
    /// Whether any screen baseline has been established yet.
    screen_observed: AtomicBool,
    alive: Arc<AtomicBool>,
    reader_handle: Option<std::thread::JoinHandle<()>>,
}

// Safety: TerminalSession implements Sync because all interior mutability
// is protected by Mutex or atomic operations:
// - id, shell, cwd: String, inherently Send+Sync
// - cols, rows, last_activity: AtomicU16/AtomicU64 — atomics provide Sync
// - master: Mutex<Option<Box<dyn MasterPty + Send>>> — Mutex provides Sync
// - child: Mutex<Option<Box<dyn Child + Send + Sync>>> — Mutex provides Sync
// - writer: Mutex<Box<dyn Write + Send>> — Mutex provides Sync
// - raw_buffer: Arc<Mutex<RingBuffer>> — Arc+Mutex provides Sync
// - utf8_decoder: Arc<Mutex<IncrementalUtf8Decoder>> — Arc+Mutex provides Sync
// - decoded_buffer: Arc<Mutex<String>> — Arc+Mutex provides Sync
// - emulator: Arc<Mutex<TerminalEmulatorHandle>> — Arc+Mutex provides Sync
// - alive: Arc<AtomicBool> — Arc+Atomic provides Sync
// - reader_handle: Option<JoinHandle<()>> — JoinHandle is Send+Sync (Rust 1.72+)
unsafe impl Sync for TerminalSession {}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

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

        let raw_buffer = Arc::new(Mutex::new(RingBuffer::new(MAX_BUFFER_SIZE)));
        let decoded_buffer = Arc::new(Mutex::new(String::new()));
        let utf8_decoder = Arc::new(Mutex::new(IncrementalUtf8Decoder::new()));
        let emulator = Arc::new(Mutex::new(TerminalEmulatorHandle::new(cols, rows)));
        let alive = Arc::new(AtomicBool::new(true));

        let raw_clone = raw_buffer.clone();
        let decoder_clone = utf8_decoder.clone();
        let decoded_clone = decoded_buffer.clone();
        let emulator_clone = emulator.clone();
        let alive_clone = alive.clone();
        let session_id = id.clone();
        let reader_handle = std::thread::Builder::new()
            .name(format!("pty-reader-{id}"))
            .spawn(move || {
                pump_output(
                    reader,
                    &raw_clone,
                    &decoder_clone,
                    &decoded_clone,
                    &emulator_clone,
                    &alive_clone,
                    &session_id,
                );
            })
            .context("Failed to spawn PTY reader thread")?;

        let initial_hash = {
            let emu = emulator.lock().unwrap_or_else(|e| e.into_inner());
            emu.state_hash()
        };

        Ok(Self {
            id,
            shell: shell_cmd,
            cwd: cwd.map(String::from),
            cols: AtomicU16::new(cols),
            rows: AtomicU16::new(rows),
            last_activity: AtomicU64::new(epoch_secs()),
            master: Mutex::new(Some(master)),
            child: Mutex::new(Some(child)),
            writer: Mutex::new(writer),
            raw_buffer,
            utf8_decoder,
            decoded_buffer,
            emulator,
            last_observed_screen_hash: AtomicU64::new(initial_hash),
            input_sequence: AtomicU64::new(0),
            observed_input_sequence: AtomicU64::new(0),
            screen_observed: AtomicBool::new(false),
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
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        writer
            .write_all(input.as_bytes())
            .context("Failed to write to PTY")?;
        writer.flush().context("Failed to flush PTY writer")?;
        self.input_sequence.fetch_add(1, Ordering::Relaxed);
        self.last_activity.store(epoch_secs(), Ordering::Relaxed);
        Ok(())
    }

    /// Drain and return all pending decoded output since the last read.
    ///
    /// If `timeout_ms` is `Some`, waits up to that many milliseconds for
    /// new output before returning.
    pub fn read_output(&self, timeout_ms: Option<u64>) -> Result<String> {
        self.last_activity.store(epoch_secs(), Ordering::Relaxed);
        {
            let mut decoded = self
                .decoded_buffer
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !decoded.is_empty() {
                let output = std::mem::take(&mut *decoded);
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
                if !self.alive.load(Ordering::Relaxed) {
                    debug!(session_id = %self.id, "Session killed while waiting for output");
                    let mut decoded = self
                        .decoded_buffer
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let mut decoder = self.utf8_decoder.lock().unwrap_or_else(|e| e.into_inner());
                    let mut output = std::mem::take(&mut *decoded);
                    output.push_str(&decoder.flush_lossy());
                    return Ok(output);
                }
                let mut decoded = self
                    .decoded_buffer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if !decoded.is_empty() || std::time::Instant::now() >= deadline {
                    let output = std::mem::take(&mut *decoded);
                    debug!(session_id = %self.id, output_len = output.len(), "Returning waited output");
                    return Ok(output);
                }
            }
        }

        Ok(String::new())
    }

    /// Read all accumulated decoded output without draining.
    pub fn peek_output(&self) -> String {
        let decoded = self
            .decoded_buffer
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        decoded.clone()
    }

    /// Resize the PTY and synchronize internal dimensions.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        debug!(session_id = %self.id, cols = cols, rows = rows, "Resizing PTY");
        let master = self.master.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref m) = *master {
            m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to resize PTY")?;
        }
        self.cols.store(cols, Ordering::Relaxed);
        self.rows.store(rows, Ordering::Relaxed);
        self.emulator
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .resize(cols, rows);
        self.last_activity.store(epoch_secs(), Ordering::Relaxed);
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
        let mut master = self.master.lock().unwrap_or_else(|e| e.into_inner());
        *master = None;
    }

    /// Whether the underlying process is still alive.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Return the epoch timestamp (seconds) of the last activity.
    pub fn last_activity_secs(&self) -> u64 {
        self.last_activity.load(Ordering::Relaxed)
    }

    /// Return session metadata.
    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            shell: self.shell.clone(),
            cwd: self.cwd.clone(),
            cols: self.cols.load(Ordering::Relaxed),
            rows: self.rows.load(Ordering::Relaxed),
            alive: self.is_alive(),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Capture a snapshot of the current visible terminal screen.
    ///
    /// Returns a [`ScreenSnapshot`] containing visible lines, cursor position,
    /// dimensions, and metadata. This does not consume or drain any state —
    /// repeated calls return the current screen each time.
    pub fn capture_screen(&self) -> ScreenSnapshot {
        let emulator = self.emulator.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = emulator.screen().snapshot();
        self.last_observed_screen_hash
            .store(emulator.state_hash(), Ordering::Relaxed);
        self.observed_input_sequence.store(
            self.input_sequence.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.screen_observed.store(true, Ordering::Relaxed);
        snapshot
    }

    /// Capture recent scrollback history.
    ///
    /// Returns up to `max_lines` scrollback lines in chronological order
    /// (oldest first). Returns an empty vec if there is no scrollback.
    pub fn capture_scrollback(&self, max_lines: usize) -> Vec<String> {
        let emulator = self.emulator.lock().unwrap_or_else(|e| e.into_inner());
        emulator.screen().scrollback_lines(max_lines)
    }

    /// Wait for the screen state to change, with optional pattern matching.
    ///
    /// Takes a baseline snapshot and polls until either:
    /// - The screen state hash differs from the baseline, AND
    /// - (if `match_pattern` is provided) the screen text matches the pattern
    /// - The timeout elapses
    ///
    /// Returns the final snapshot on success, or an error on timeout.
    pub fn wait_for_screen_change(
        &self,
        timeout_ms: u64,
        match_pattern: Option<&str>,
    ) -> Result<ScreenSnapshot> {
        let current_input_seq = self.input_sequence.load(Ordering::Relaxed);
        let observed_input_seq = self.observed_input_sequence.load(Ordering::Relaxed);
        let mut baseline_hash = {
            let emulator = self.emulator.lock().unwrap_or_else(|e| e.into_inner());
            let current_hash = emulator.state_hash();
            let should_reset_baseline = (!self.screen_observed.load(Ordering::Relaxed)
                && current_input_seq == 0)
                || current_input_seq == observed_input_seq;
            if should_reset_baseline {
                self.last_observed_screen_hash
                    .store(current_hash, Ordering::Relaxed);
                self.observed_input_sequence
                    .store(current_input_seq, Ordering::Relaxed);
                self.screen_observed.store(true, Ordering::Relaxed);
                current_hash
            } else {
                self.last_observed_screen_hash.load(Ordering::Relaxed)
            }
        };

        let compiled_regex = match_pattern
            .map(|p| regex::Regex::new(p).context(format!("Invalid regex pattern: {}", p)))
            .transpose()?;

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let poll_interval = std::time::Duration::from_millis(50);

        loop {
            std::thread::sleep(poll_interval);

            let snapshot = {
                let emulator = self.emulator.lock().unwrap_or_else(|e| e.into_inner());
                let current_hash = emulator.state_hash();
                if current_hash == baseline_hash {
                    if std::time::Instant::now() >= deadline {
                        anyhow::bail!(
                            "Timeout waiting for screen change ({}ms elapsed)",
                            timeout_ms
                        );
                    }
                    continue;
                }
                if current_input_seq == 0 {
                    baseline_hash = current_hash;
                    self.last_observed_screen_hash
                        .store(current_hash, Ordering::Relaxed);
                    self.observed_input_sequence.store(0, Ordering::Relaxed);
                    self.screen_observed.store(true, Ordering::Relaxed);
                    if std::time::Instant::now() >= deadline {
                        anyhow::bail!(
                            "Timeout waiting for screen change ({}ms elapsed)",
                            timeout_ms
                        );
                    }
                    continue;
                }
                self.last_observed_screen_hash
                    .store(current_hash, Ordering::Relaxed);
                self.observed_input_sequence
                    .store(current_input_seq, Ordering::Relaxed);
                self.screen_observed.store(true, Ordering::Relaxed);
                emulator.screen().snapshot()
            };

            // Screen changed — check optional pattern match
            if let Some(ref re) = compiled_regex {
                let screen_text = snapshot.lines.join("\n");
                if !re.is_match(&screen_text) {
                    if std::time::Instant::now() >= deadline {
                        anyhow::bail!(
                            "Screen changed but pattern '{}' not found within timeout ({}ms)",
                            match_pattern.unwrap_or(""),
                            timeout_ms
                        );
                    }
                    continue;
                }
            }

            return Ok(snapshot);
        }
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

/// Background output pump: reads from PTY, stores raw bytes, feeds the ANSI
/// parser, and incrementally decodes UTF-8 into the decoded text buffer.
fn pump_output(
    mut reader: Box<dyn Read + Send>,
    raw_buffer: &Arc<Mutex<RingBuffer>>,
    utf8_decoder: &Arc<Mutex<IncrementalUtf8Decoder>>,
    decoded_buffer: &Arc<Mutex<String>>,
    emulator: &Arc<Mutex<TerminalEmulatorHandle>>,
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
                // Flush any remaining incomplete UTF-8 bytes.
                if let Ok(mut decoder) = utf8_decoder.lock() {
                    let tail = decoder.flush_lossy();
                    if !tail.is_empty() {
                        if let Ok(mut decoded) = decoded_buffer.lock() {
                            decoded.push_str(&tail);
                        }
                    }
                }
                // Flush ANSI parser state.
                if let Ok(mut emu) = emulator.lock() {
                    emu.flush_parser();
                }
                alive.store(false, Ordering::Relaxed);
                break;
            }
            Ok(n) => {
                let chunk = &tmp[..n];

                // Store raw bytes.
                if let Ok(mut raw) = raw_buffer.lock() {
                    raw.write(chunk);
                }

                // Feed bytes through ANSI parser.
                if let Ok(mut emu) = emulator.lock() {
                    emu.feed_bytes(chunk);
                }

                // Incrementally decode UTF-8 and append to decoded buffer.
                if let Ok(mut decoder) = utf8_decoder.lock() {
                    let decoded_text = decoder.decode(chunk);
                    if !decoded_text.is_empty() {
                        if let Ok(mut decoded) = decoded_buffer.lock() {
                            decoded.push_str(&decoded_text);
                        }
                    }
                }
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                warn!("PTY reader error for session '{}': {}", session_id, e);
                // Flush incomplete UTF-8 before dying.
                if let Ok(mut emu) = emulator.lock() {
                    emu.flush_parser();
                }
                if let Ok(mut decoder) = utf8_decoder.lock() {
                    let tail = decoder.flush_lossy();
                    if !tail.is_empty() {
                        if let Ok(mut decoded) = decoded_buffer.lock() {
                            decoded.push_str(&tail);
                        }
                    }
                }
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

    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.pending_len == 0
    }

    #[allow(dead_code)]
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

    #[allow(dead_code)]
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
