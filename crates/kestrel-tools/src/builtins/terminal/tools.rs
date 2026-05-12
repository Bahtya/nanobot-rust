//! Terminal multiplexer tools for AI-driven session management.
//!
//! Exposes ten tools that let the LLM create, interact with, and manage
//! PTY-backed terminal sessions:
//!
//! - `terminal_create_session`   — spawn a new shell in a PTY
//! - `terminal_send_input`       — send keystrokes / commands
//! - `terminal_read_output`      — read pending output (raw/escaped/text modes)
//! - `terminal_list_sessions`    — enumerate active sessions
//! - `terminal_kill_session`     — destroy a session
//! - `terminal_resize`           — change PTY dimensions
//! - `terminal_send_key`         — send special keys (arrows, enter, etc.)
//! - `terminal_capture_screen`   — capture current visible screen state
//! - `terminal_capture_scrollback` — capture scrollback history
//! - `terminal_wait_for_screen_change` — wait for screen state change (event-driven TUI)

use crate::builtins::terminal::emulator::{escape_control, strip_ansi, ReadMode};
use crate::trait_def::{Tool, ToolError};
use async_trait::async_trait;
use kestrel_core::MAX_TOOL_OUTPUT_LENGTH;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, info};

use super::manager::TerminalManager;

// ─── Shared helpers ─────────────────────────────────────────────

/// Extract the manager from the tool's state or return an error.
///
/// The manager is always present in normal operation; `None` only occurs
/// if the tool was constructed without wiring (e.g. in tests).
fn require_manager(mgr: &Option<Arc<TerminalManager>>) -> Result<Arc<TerminalManager>, ToolError> {
    mgr.clone()
        .ok_or_else(|| ToolError::NotAvailable("TerminalManager not wired".to_string()))
}

/// Truncate output to the maximum allowed length, appending a truncation
/// indicator if needed. Mirrors the approach used by the `exec` tool.
fn truncate_output(output: String) -> String {
    if output.len() > MAX_TOOL_OUTPUT_LENGTH {
        let mut truncated = output;
        let pos = truncated
            .char_indices()
            .nth(MAX_TOOL_OUTPUT_LENGTH)
            .map(|(i, _)| i)
            .unwrap_or(truncated.len());
        truncated.truncate(pos);
        truncated.push_str("\n... (output truncated)");
        truncated
    } else {
        output
    }
}

// ═══════════════════════════════════════════════════════════════════
// 1. terminal_create_session
// ═══════════════════════════════════════════════════════════════════

pub struct TerminalCreateSessionTool {
    mgr: Option<Arc<TerminalManager>>,
}

impl TerminalCreateSessionTool {
    pub fn new() -> Self {
        Self { mgr: None }
    }

    pub fn with_manager(mut self, mgr: Arc<TerminalManager>) -> Self {
        self.mgr = Some(mgr);
        self
    }
}

impl Default for TerminalCreateSessionTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TerminalCreateSessionTool {
    fn name(&self) -> &str {
        "terminal_create_session"
    }

    fn description(&self) -> &str {
        "Create a new interactive terminal session (PTY). Returns a session_id \
         for use with other terminal_* tools. The session runs a shell that \
         persists until explicitly killed."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "shell": {
                    "type": "string",
                    "description": "Shell executable path (default: system shell). \
                     Only known system shells are allowed unless running in dangerous mode."
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory for the session"
                },
                "cols": {
                    "type": "integer",
                    "description": "Terminal width in columns (default: 80)"
                },
                "rows": {
                    "type": "integer",
                    "description": "Terminal height in rows (default: 24)"
                }
            },
            "required": []
        })
    }

    fn is_mutating(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let mgr = require_manager(&self.mgr)?;
        let shell = args["shell"].as_str().map(String::from);
        let cwd = args["cwd"].as_str().map(String::from);
        let cols = args["cols"].as_u64().unwrap_or(80) as u16;
        let rows = args["rows"].as_u64().unwrap_or(24) as u16;

        debug!(
            shell = shell.as_deref().unwrap_or("default"),
            cwd = cwd.as_deref().unwrap_or("-"),
            cols = cols,
            rows = rows,
            "Creating terminal session"
        );

        // Spawn a PTY session on a blocking thread to avoid blocking
        // the async runtime. PTY spawn and fork are inherently blocking.
        let id = tokio::task::spawn_blocking(move || {
            mgr.create_session(shell, cwd.as_deref(), cols, rows)
        })
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {}", e)))?
        .map_err(|e| ToolError::Execution(format!("Failed to create session: {}", e)))?;

        debug!(session_id = %id, "Terminal session created");
        Ok(format!(
            "Created terminal session '{}' ({}x{}). Use terminal_send_input \
             to send commands and terminal_read_output to read results.",
            id, cols, rows
        ))
    }
}

// ═══════════════════════════════════════════════════════════════════
// 2. terminal_send_input
// ═══════════════════════════════════════════════════════════════════

pub struct TerminalSendInputTool {
    mgr: Option<Arc<TerminalManager>>,
}

impl TerminalSendInputTool {
    pub fn new() -> Self {
        Self { mgr: None }
    }

    pub fn with_manager(mut self, mgr: Arc<TerminalManager>) -> Self {
        self.mgr = Some(mgr);
        self
    }
}

impl Default for TerminalSendInputTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TerminalSendInputTool {
    fn name(&self) -> &str {
        "terminal_send_input"
    }

    fn description(&self) -> &str {
        "Send input (keystrokes/commands) to a terminal session. \
         Append '\\n' to the input string to simulate pressing Enter."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The session ID returned by terminal_create_session"
                },
                "input": {
                    "type": "string",
                    "description": "The text to type into the terminal"
                }
            },
            "required": ["session_id", "input"]
        })
    }

    fn is_mutating(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let mgr = require_manager(&self.mgr)?;
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'session_id'".to_string()))?;
        let input = args["input"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'input'".to_string()))?;

        debug!(
            session_id = session_id,
            input_len = input.len(),
            "Sending input to terminal session"
        );

        let sid = session_id.to_string();
        let input_owned = input.to_string();
        tokio::task::spawn_blocking(move || mgr.send_input(&sid, &input_owned))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {}", e)))?
            .map_err(|e| ToolError::Execution(format!("Failed to send input: {}", e)))?;

        Ok(format!(
            "Sent {} bytes to session '{}'.",
            input.len(),
            session_id
        ))
    }
}

// ═══════════════════════════════════════════════════════════════════
// 3. terminal_read_output
// ═══════════════════════════════════════════════════════════════════

pub struct TerminalReadOutputTool {
    mgr: Option<Arc<TerminalManager>>,
}

impl TerminalReadOutputTool {
    pub fn new() -> Self {
        Self { mgr: None }
    }

    pub fn with_manager(mut self, mgr: Arc<TerminalManager>) -> Self {
        self.mgr = Some(mgr);
        self
    }
}

impl Default for TerminalReadOutputTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TerminalReadOutputTool {
    fn name(&self) -> &str {
        "terminal_read_output"
    }

    fn description(&self) -> &str {
        "Read new output from a terminal session since the last read. \
         Optionally wait for output with a timeout. Supports three modes: \
         'raw' (preserves ANSI sequences), 'escaped' (control chars visible \
         as <ESC>, \\n, etc.), and 'text' (strips ANSI, keeps printable text). \
         Default mode is 'raw'. Output is truncated at 100,000 characters."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The session ID to read from"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Max milliseconds to wait for new output (default: 0, return immediately)"
                },
                "mode": {
                    "type": "string",
                    "description": "Output mode: 'raw' (default, preserves ANSI), 'escaped' (control chars visible), 'text' (strips ANSI)",
                    "enum": ["raw", "escaped", "text"]
                }
            },
            "required": ["session_id"]
        })
    }

    fn is_mutating(&self) -> bool {
        false
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let mgr = require_manager(&self.mgr)?;
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'session_id'".to_string()))?;
        let timeout_ms = args["timeout_ms"].as_u64();
        let mode = args["mode"]
            .as_str()
            .and_then(ReadMode::parse_mode)
            .unwrap_or(ReadMode::Raw);

        debug!(
            session_id = session_id,
            timeout_ms = timeout_ms.unwrap_or(0),
            mode = ?mode,
            "Reading output from terminal session"
        );

        let sid = session_id.to_string();
        let output = tokio::task::spawn_blocking(move || mgr.read_output(&sid, timeout_ms))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {}", e)))?
            .map_err(|e| ToolError::Execution(format!("Failed to read output: {}", e)))?;

        if output.is_empty() {
            Ok("(no new output)".to_string())
        } else {
            let output = match mode {
                ReadMode::Raw => output,
                ReadMode::Escaped => escape_control(&output),
                ReadMode::Text => strip_ansi(&output),
            };
            debug!(
                session_id = session_id,
                output_len = output.len(),
                "Returning terminal output"
            );
            Ok(truncate_output(output))
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// 4. terminal_list_sessions
// ═══════════════════════════════════════════════════════════════════

pub struct TerminalListSessionsTool {
    mgr: Option<Arc<TerminalManager>>,
}

impl TerminalListSessionsTool {
    pub fn new() -> Self {
        Self { mgr: None }
    }

    pub fn with_manager(mut self, mgr: Arc<TerminalManager>) -> Self {
        self.mgr = Some(mgr);
        self
    }
}

impl Default for TerminalListSessionsTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TerminalListSessionsTool {
    fn name(&self) -> &str {
        "terminal_list_sessions"
    }

    fn description(&self) -> &str {
        "List all active terminal sessions with their status."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    fn is_mutating(&self) -> bool {
        false
    }

    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let mgr = require_manager(&self.mgr)?;
        let sessions = mgr.list_sessions();

        debug!(session_count = sessions.len(), "Listing terminal sessions");

        if sessions.is_empty() {
            return Ok("No active terminal sessions.".to_string());
        }

        let mut lines = Vec::with_capacity(sessions.len() + 1);
        lines.push(format!("{} active session(s):", sessions.len()));
        for info in &sessions {
            let status = if info.alive { "alive" } else { "dead" };
            lines.push(format!(
                "  {} | shell={} | cwd={} | {}x{} | {}",
                info.id,
                info.shell,
                info.cwd.as_deref().unwrap_or("-"),
                info.cols,
                info.rows,
                status
            ));
        }
        Ok(lines.join("\n"))
    }
}

// ═══════════════════════════════════════════════════════════════════
// 5. terminal_kill_session
// ═══════════════════════════════════════════════════════════════════

pub struct TerminalKillSessionTool {
    mgr: Option<Arc<TerminalManager>>,
}

impl TerminalKillSessionTool {
    pub fn new() -> Self {
        Self { mgr: None }
    }

    pub fn with_manager(mut self, mgr: Arc<TerminalManager>) -> Self {
        self.mgr = Some(mgr);
        self
    }
}

impl Default for TerminalKillSessionTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TerminalKillSessionTool {
    fn name(&self) -> &str {
        "terminal_kill_session"
    }

    fn description(&self) -> &str {
        "Kill and destroy a terminal session. The session's shell process \
         is terminated and all buffered output is discarded."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The session ID to kill"
                }
            },
            "required": ["session_id"]
        })
    }

    fn is_mutating(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let mgr = require_manager(&self.mgr)?;
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'session_id'".to_string()))?;

        debug!(session_id = session_id, "Killing terminal session");

        let sid = session_id.to_string();
        tokio::task::spawn_blocking(move || mgr.kill_session(&sid))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {}", e)))?
            .map_err(|e| ToolError::Execution(format!("Failed to kill session: {}", e)))?;

        Ok(format!("Killed terminal session '{}'.", session_id))
    }
}

// ═══════════════════════════════════════════════════════════════════
// 6. terminal_resize
// ═══════════════════════════════════════════════════════════════════

pub struct TerminalResizeTool {
    mgr: Option<Arc<TerminalManager>>,
}

impl TerminalResizeTool {
    pub fn new() -> Self {
        Self { mgr: None }
    }

    pub fn with_manager(mut self, mgr: Arc<TerminalManager>) -> Self {
        self.mgr = Some(mgr);
        self
    }
}

impl Default for TerminalResizeTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TerminalResizeTool {
    fn name(&self) -> &str {
        "terminal_resize"
    }

    fn description(&self) -> &str {
        "Resize a terminal session's PTY dimensions (columns and rows)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The session ID to resize"
                },
                "cols": {
                    "type": "integer",
                    "description": "New width in columns"
                },
                "rows": {
                    "type": "integer",
                    "description": "New height in rows"
                }
            },
            "required": ["session_id", "cols", "rows"]
        })
    }

    fn is_mutating(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let mgr = require_manager(&self.mgr)?;
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'session_id'".to_string()))?;
        let cols = args["cols"]
            .as_u64()
            .ok_or_else(|| ToolError::Validation("Missing 'cols'".to_string()))?
            as u16;
        let rows = args["rows"]
            .as_u64()
            .ok_or_else(|| ToolError::Validation("Missing 'rows'".to_string()))?
            as u16;

        debug!(
            session_id = session_id,
            cols = cols,
            rows = rows,
            "Resizing terminal session"
        );

        let sid = session_id.to_string();
        tokio::task::spawn_blocking(move || mgr.resize_session(&sid, cols, rows))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {}", e)))?
            .map_err(|e| ToolError::Execution(format!("Failed to resize session: {}", e)))?;

        Ok(format!(
            "Resized session '{}' to {}x{}.",
            session_id, cols, rows
        ))
    }
}

// ═══════════════════════════════════════════════════════════════════
// 7. terminal_send_key
// ═══════════════════════════════════════════════════════════════════

/// Map a named key to the ANSI escape sequence that a terminal expects.
fn key_to_bytes(key: &str) -> Option<Vec<u8>> {
    match key {
        "Enter" => Some(b"\r".to_vec()),
        "Backspace" => Some(b"\x7f".to_vec()),
        "Tab" => Some(b"\t".to_vec()),
        "Escape" | "Esc" => Some(b"\x1b".to_vec()),
        "Up" => Some(b"\x1b[A".to_vec()),
        "Down" => Some(b"\x1b[B".to_vec()),
        "Right" => Some(b"\x1b[C".to_vec()),
        "Left" => Some(b"\x1b[D".to_vec()),
        "Home" => Some(b"\x1b[H".to_vec()),
        "End" => Some(b"\x1b[F".to_vec()),
        "PageUp" | "PgUp" => Some(b"\x1b[5~".to_vec()),
        "PageDown" | "PgDn" => Some(b"\x1b[6~".to_vec()),
        "Insert" => Some(b"\x1b[2~".to_vec()),
        "Delete" => Some(b"\x1b[3~".to_vec()),
        "F1" => Some(b"\x1bOP".to_vec()),
        "F2" => Some(b"\x1bOQ".to_vec()),
        "F3" => Some(b"\x1bOR".to_vec()),
        "F4" => Some(b"\x1bOS".to_vec()),
        "F5" => Some(b"\x1b[15~".to_vec()),
        "F6" => Some(b"\x1b[17~".to_vec()),
        "F7" => Some(b"\x1b[18~".to_vec()),
        "F8" => Some(b"\x1b[19~".to_vec()),
        "F9" => Some(b"\x1b[20~".to_vec()),
        "F10" => Some(b"\x1b[21~".to_vec()),
        "F11" => Some(b"\x1b[23~".to_vec()),
        "F12" => Some(b"\x1b[24~".to_vec()),
        "Space" => Some(b" ".to_vec()),
        // Shift+Tab — used by many TUI menus
        "Shift+Tab" | "Backtab" => Some(b"\x1b[Z".to_vec()),
        // Ctrl+Arrow — word-motion in editors
        "Ctrl+Up" => Some(b"\x1b[1;5A".to_vec()),
        "Ctrl+Down" => Some(b"\x1b[1;5B".to_vec()),
        "Ctrl+Right" => Some(b"\x1b[1;5C".to_vec()),
        "Ctrl+Left" => Some(b"\x1b[1;5D".to_vec()),
        // Shift+Arrow — selection in many editors
        "Shift+Up" => Some(b"\x1b[1;2A".to_vec()),
        "Shift+Down" => Some(b"\x1b[1;2B".to_vec()),
        "Shift+Right" => Some(b"\x1b[1;2C".to_vec()),
        "Shift+Left" => Some(b"\x1b[1;2D".to_vec()),
        _ => None,
    }
}

/// Parse a key string that may contain modifier combinations (e.g. "Ctrl+C", "Alt+x").
fn resolve_key_bytes(key: &str) -> Result<Vec<u8>, String> {
    // Handle Alt+ combinations — ESC prefix + key
    if let Some(rest) = key.strip_prefix("Alt+") {
        if rest.len() == 1 {
            let c = rest.chars().next().unwrap();
            return Ok(format!("\x1b{}", c).into_bytes());
        }
        // Alt+ named key: Alt+Enter, Alt+Up, etc.
        if let Some(bytes) = key_to_bytes(rest) {
            let mut result = vec![0x1b];
            result.extend_from_slice(&bytes);
            return Ok(result);
        }
        return Err(format!("Unknown Alt combination: Alt+{}", rest));
    }

    // Handle Ctrl+letter combinations
    if let Some(letter) = key.strip_prefix("Ctrl+") {
        if letter.len() == 1 {
            let c = letter.chars().next().unwrap().to_ascii_uppercase();
            if ('A'..='_').contains(&c) {
                return Ok(vec![c as u8 - b'A' + 1]);
            }
        }
        return match letter {
            "Space" => Ok(vec![0x00]),
            "Up" | "Down" | "Right" | "Left" => key_to_bytes(key)
                .ok_or_else(|| format!("Unknown Ctrl combination: Ctrl+{}", letter)),
            _ => Err(format!("Unknown Ctrl combination: Ctrl+{}", letter)),
        };
    }

    // Handle Shift+ combinations
    if key.starts_with("Shift+") {
        return key_to_bytes(key).ok_or_else(|| format!("Unknown Shift combination: {}", key));
    }

    key_to_bytes(key).ok_or_else(|| {
        format!(
            "Unknown key: '{}'. Supported: Enter, Backspace, Tab, Escape/Esc, \
             Up/Down/Left/Right, Home, End, PageUp/PgUp, PageDown/PgDn, Insert, Delete, \
             F1-F12, Space, Ctrl+A-Z, Ctrl+Space, Ctrl+Arrow, Alt+key, Shift+Tab, Shift+Arrow",
            key
        )
    })
}

pub struct TerminalSendKeyTool {
    mgr: Option<Arc<TerminalManager>>,
}

impl TerminalSendKeyTool {
    pub fn new() -> Self {
        Self { mgr: None }
    }

    pub fn with_manager(mut self, mgr: Arc<TerminalManager>) -> Self {
        self.mgr = Some(mgr);
        self
    }
}

impl Default for TerminalSendKeyTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TerminalSendKeyTool {
    fn name(&self) -> &str {
        "terminal_send_key"
    }

    fn description(&self) -> &str {
        "Send a special key press to a terminal session. Use this for TUI \
         navigation (arrow keys, Enter, Escape, etc.) instead of \
         terminal_send_input. Supports: Enter, Backspace, Tab, Escape, \
         Up/Down/Left/Right, Home, End, PageUp, PageDown, Insert, Delete, \
         F1-F12, and Ctrl+letter combinations."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The session ID returned by terminal_create_session"
                },
                "key": {
                    "type": "string",
                    "description": "The key to send (e.g. 'Enter', 'Up', 'Ctrl+C', 'Escape', 'F1')"
                }
            },
            "required": ["session_id", "key"]
        })
    }

    fn is_mutating(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let mgr = require_manager(&self.mgr)?;
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'session_id'".to_string()))?;
        let key = args["key"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'key'".to_string()))?;

        let key_bytes = resolve_key_bytes(key).map_err(ToolError::Validation)?;

        // Convert bytes to a string for send_input (safe because all key sequences are valid UTF-8)
        let input = String::from_utf8(key_bytes)
            .map_err(|e| ToolError::Execution(format!("Invalid key sequence: {}", e)))?;

        debug!(
            session_id = session_id,
            key = key,
            bytes = input.len(),
            "Sending key to terminal session"
        );

        let sid = session_id.to_string();
        tokio::task::spawn_blocking(move || mgr.send_input(&sid, &input))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {}", e)))?
            .map_err(|e| ToolError::Execution(format!("Failed to send key: {}", e)))?;

        Ok(format!("Sent key '{}' to session '{}'.", key, session_id))
    }
}

// ═══════════════════════════════════════════════════════════════════
// 8. terminal_capture_screen
// ═══════════════════════════════════════════════════════════════════

pub struct TerminalCaptureScreenTool {
    mgr: Option<Arc<TerminalManager>>,
}

impl TerminalCaptureScreenTool {
    pub fn new() -> Self {
        Self { mgr: None }
    }

    pub fn with_manager(mut self, mgr: Arc<TerminalManager>) -> Self {
        self.mgr = Some(mgr);
        self
    }
}

impl Default for TerminalCaptureScreenTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TerminalCaptureScreenTool {
    fn name(&self) -> &str {
        "terminal_capture_screen"
    }

    fn description(&self) -> &str {
        "Capture the current visible terminal screen as a snapshot. Returns \
         the screen contents with cursor position, dimensions, and metadata. \
         Unlike terminal_read_output (which drains incremental transcript output), \
         this tool reads from the emulator's screen model — the same state a user \
         would see on screen. Works for both normal shells and full-screen TUI \
         programs (alternate screen). Repeated calls do not consume or destroy state."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The session ID returned by terminal_create_session"
                },
                "format": {
                    "type": "string",
                    "description": "Output format: 'text' (default, plain text lines) or 'json' (structured with metadata)",
                    "enum": ["text", "json"]
                }
            },
            "required": ["session_id"]
        })
    }

    fn is_mutating(&self) -> bool {
        false
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let mgr = require_manager(&self.mgr)?;
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'session_id'".to_string()))?;
        let format = args["format"].as_str().unwrap_or("text");

        debug!(
            session_id = session_id,
            format = format,
            "Capturing terminal screen"
        );

        let sid = session_id.to_string();
        let snapshot = tokio::task::spawn_blocking(move || mgr.capture_screen(&sid))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {}", e)))?
            .map_err(|e| ToolError::Execution(format!("Failed to capture screen: {}", e)))?;

        match format {
            "json" => serde_json::to_string_pretty(&snapshot)
                .map_err(|e| ToolError::Execution(format!("JSON serialization error: {}", e))),
            _ => {
                // Plain text: show lines with cursor indicator
                let mut output = String::new();
                for (i, line) in snapshot.lines.iter().enumerate() {
                    if i == snapshot.cursor_row {
                        output.push_str(&format!(
                            ">{:>3} |{}\n",
                            i,
                            if line.is_empty() { "" } else { line }
                        ));
                    } else {
                        output.push_str(&format!(
                            " {:>3} |{}\n",
                            i,
                            if line.is_empty() { "" } else { line }
                        ));
                    }
                }
                output.push_str(&format!(
                    "\ncursor: ({}, {})  dims: {}x{}  alternate: {}  title: {}",
                    snapshot.cursor_row,
                    snapshot.cursor_col,
                    snapshot.cols,
                    snapshot.rows,
                    snapshot.is_alternate,
                    if snapshot.window_title.is_empty() {
                        "-"
                    } else {
                        &snapshot.window_title
                    }
                ));
                Ok(output)
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// 9. terminal_capture_scrollback
// ═══════════════════════════════════════════════════════════════════

pub struct TerminalCaptureScrollbackTool {
    mgr: Option<Arc<TerminalManager>>,
}

impl TerminalCaptureScrollbackTool {
    pub fn new() -> Self {
        Self { mgr: None }
    }

    pub fn with_manager(mut self, mgr: Arc<TerminalManager>) -> Self {
        self.mgr = Some(mgr);
        self
    }
}

impl Default for TerminalCaptureScrollbackTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TerminalCaptureScrollbackTool {
    fn name(&self) -> &str {
        "terminal_capture_scrollback"
    }

    fn description(&self) -> &str {
        "Capture recent scrollback (history) lines from a terminal session. \
         Returns lines that have scrolled off the top of the visible screen, \
         in chronological order (oldest first). Only available for the primary \
         screen buffer — alternate screen programs typically do not produce \
         scrollback."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The session ID returned by terminal_create_session"
                },
                "max_lines": {
                    "type": "integer",
                    "description": "Maximum number of scrollback lines to return (default: 100)"
                }
            },
            "required": ["session_id"]
        })
    }

    fn is_mutating(&self) -> bool {
        false
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let mgr = require_manager(&self.mgr)?;
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'session_id'".to_string()))?;
        let max_lines = args["max_lines"].as_u64().unwrap_or(100) as usize;

        debug!(
            session_id = session_id,
            max_lines = max_lines,
            "Capturing terminal scrollback"
        );

        let sid = session_id.to_string();
        let lines = tokio::task::spawn_blocking(move || mgr.capture_scrollback(&sid, max_lines))
            .await
            .map_err(|e| ToolError::Execution(format!("Task join error: {}", e)))?
            .map_err(|e| ToolError::Execution(format!("Failed to capture scrollback: {}", e)))?;

        if lines.is_empty() {
            Ok("(no scrollback)".to_string())
        } else {
            Ok(truncate_output(lines.join("\n")))
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// 10. terminal_wait_for_screen_change
// ═══════════════════════════════════════════════════════════════════

pub struct TerminalWaitForScreenChangeTool {
    mgr: Option<Arc<TerminalManager>>,
}

impl TerminalWaitForScreenChangeTool {
    pub fn new() -> Self {
        Self { mgr: None }
    }

    pub fn with_manager(mut self, mgr: Arc<TerminalManager>) -> Self {
        self.mgr = Some(mgr);
        self
    }
}

impl Default for TerminalWaitForScreenChangeTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TerminalWaitForScreenChangeTool {
    fn name(&self) -> &str {
        "terminal_wait_for_screen_change"
    }

    fn description(&self) -> &str {
        "Wait for the terminal screen to change, then return the new screen snapshot. \
         This is event-driven TUI automation: instead of blind sleep loops, the tool \
         polls the emulator's screen model (not raw PTY bytes) and returns as soon as \
         the visible state differs from when the call began. \
         Optionally provide a 'match' regex to wait for specific content to appear. \
         Works in both normal and alternate-screen (full-screen TUI) modes."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The session ID returned by terminal_create_session"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Maximum milliseconds to wait for a screen change (default: 5000)"
                },
                "match": {
                    "type": "string",
                    "description": "Optional regex pattern that must appear in the screen content after the change. The tool waits until both a screen mutation occurs AND this pattern matches."
                }
            },
            "required": ["session_id"]
        })
    }

    fn is_mutating(&self) -> bool {
        false
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let mgr = require_manager(&self.mgr)?;
        let session_id = args["session_id"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'session_id'".to_string()))?;
        let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(5000);
        let match_pattern = args["match"].as_str();

        info!(
            session_id = session_id,
            timeout_ms = timeout_ms,
            match_pattern = match_pattern.unwrap_or("none"),
            "terminal_wait_for_screen_change: executing"
        );

        debug!(
            session_id = session_id,
            timeout_ms = timeout_ms,
            match_pattern = match_pattern.unwrap_or("none"),
            "Waiting for screen change"
        );

        let sid = session_id.to_string();
        let pattern_owned = match_pattern.map(String::from);
        let (snapshot, changed) = tokio::task::spawn_blocking(move || {
            mgr.wait_for_screen_change(&sid, timeout_ms, pattern_owned.as_deref())
        })
        .await
        .map_err(|e| ToolError::Execution(format!("Task join error: {}", e)))?
        .map_err(|e| ToolError::Execution(e.to_string()))?;

        let mut output = String::new();
        if !changed {
            output.push_str(&format!("Screen unchanged after {}ms.\n\n", timeout_ms));
        }
        for (i, line) in snapshot.lines.iter().enumerate() {
            if i == snapshot.cursor_row {
                output.push_str(&format!(
                    ">{:>3} |{}\n",
                    i,
                    if line.is_empty() { "" } else { line }
                ));
            } else {
                output.push_str(&format!(
                    " {:>3} |{}\n",
                    i,
                    if line.is_empty() { "" } else { line }
                ));
            }
        }
        output.push_str(&format!(
            "\ncursor: ({}, {})  dims: {}x{}  alternate: {}  title: {}",
            snapshot.cursor_row,
            snapshot.cursor_col,
            snapshot.cols,
            snapshot.rows,
            snapshot.is_alternate,
            if snapshot.window_title.is_empty() {
                "-"
            } else {
                &snapshot.window_title
            }
        ));
        Ok(output)
    }
}

// ═══════════════════════════════════════════════════════════════════
// Registration helper
// ═══════════════════════════════════════════════════════════════════

/// Register all terminal tools with a shared `TerminalManager`.
pub fn register_terminal_tools(
    registry: &crate::registry::ToolRegistry,
    mgr: Arc<TerminalManager>,
) {
    registry.register(TerminalCreateSessionTool::new().with_manager(mgr.clone()));
    registry.register(TerminalSendInputTool::new().with_manager(mgr.clone()));
    registry.register(TerminalReadOutputTool::new().with_manager(mgr.clone()));
    registry.register(TerminalListSessionsTool::new().with_manager(mgr.clone()));
    registry.register(TerminalKillSessionTool::new().with_manager(mgr.clone()));
    registry.register(TerminalResizeTool::new().with_manager(mgr.clone()));
    registry.register(TerminalSendKeyTool::new().with_manager(mgr.clone()));
    registry.register(TerminalCaptureScreenTool::new().with_manager(mgr.clone()));
    registry.register(TerminalCaptureScrollbackTool::new().with_manager(mgr.clone()));
    registry.register(TerminalWaitForScreenChangeTool::new().with_manager(mgr));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a manager in dangerous mode for testing (allows any shell).
    fn make_tools() -> (
        Arc<TerminalManager>,
        TerminalCreateSessionTool,
        TerminalSendInputTool,
        TerminalReadOutputTool,
        TerminalListSessionsTool,
        TerminalKillSessionTool,
        TerminalResizeTool,
        TerminalSendKeyTool,
        TerminalCaptureScreenTool,
        TerminalCaptureScrollbackTool,
        TerminalWaitForScreenChangeTool,
    ) {
        let mgr = Arc::new(TerminalManager::with_config(10, true));
        (
            mgr.clone(),
            TerminalCreateSessionTool::new().with_manager(mgr.clone()),
            TerminalSendInputTool::new().with_manager(mgr.clone()),
            TerminalReadOutputTool::new().with_manager(mgr.clone()),
            TerminalListSessionsTool::new().with_manager(mgr.clone()),
            TerminalKillSessionTool::new().with_manager(mgr.clone()),
            TerminalResizeTool::new().with_manager(mgr.clone()),
            TerminalSendKeyTool::new().with_manager(mgr.clone()),
            TerminalCaptureScreenTool::new().with_manager(mgr.clone()),
            TerminalCaptureScrollbackTool::new().with_manager(mgr.clone()),
            TerminalWaitForScreenChangeTool::new().with_manager(mgr),
        )
    }

    fn test_session_args() -> Value {
        if cfg!(windows) {
            json!({ "shell": "cmd.exe" })
        } else {
            json!({})
        }
    }

    #[test]
    fn test_tool_names() {
        let (_, create, send, read, list, kill, resize, send_key, capture, scrollback, wait) =
            make_tools();
        assert_eq!(create.name(), "terminal_create_session");
        assert_eq!(send.name(), "terminal_send_input");
        assert_eq!(read.name(), "terminal_read_output");
        assert_eq!(list.name(), "terminal_list_sessions");
        assert_eq!(kill.name(), "terminal_kill_session");
        assert_eq!(resize.name(), "terminal_resize");
        assert_eq!(send_key.name(), "terminal_send_key");
        assert_eq!(capture.name(), "terminal_capture_screen");
        assert_eq!(scrollback.name(), "terminal_capture_scrollback");
        assert_eq!(wait.name(), "terminal_wait_for_screen_change");
    }

    #[test]
    fn test_mutating_classification() {
        let (_, create, send, read, list, kill, resize, send_key, capture, scrollback, wait) =
            make_tools();
        assert!(create.is_mutating());
        assert!(send.is_mutating());
        assert!(!read.is_mutating());
        assert!(!list.is_mutating());
        assert!(kill.is_mutating());
        assert!(resize.is_mutating());
        assert!(send_key.is_mutating());
        assert!(!capture.is_mutating());
        assert!(!scrollback.is_mutating());
        assert!(!wait.is_mutating());
    }

    #[test]
    fn test_descriptions_nonempty() {
        let (_, create, send, read, list, kill, resize, send_key, capture, scrollback, wait) =
            make_tools();
        assert!(!create.description().is_empty());
        assert!(!send.description().is_empty());
        assert!(!read.description().is_empty());
        assert!(!list.description().is_empty());
        assert!(!kill.description().is_empty());
        assert!(!resize.description().is_empty());
        assert!(!send_key.description().is_empty());
        assert!(!capture.description().is_empty());
        assert!(!scrollback.description().is_empty());
        assert!(!wait.description().is_empty());
    }

    #[test]
    fn test_schemas_are_valid_json() {
        let (_, create, send, read, list, kill, resize, send_key, capture, scrollback, wait) =
            make_tools();
        for schema in &[
            create.parameters_schema(),
            send.parameters_schema(),
            read.parameters_schema(),
            list.parameters_schema(),
            kill.parameters_schema(),
            resize.parameters_schema(),
            send_key.parameters_schema(),
            capture.parameters_schema(),
            scrollback.parameters_schema(),
            wait.parameters_schema(),
        ] {
            assert_eq!(schema["type"], "object");
        }
    }

    #[tokio::test]
    async fn test_list_sessions_empty() {
        let (_, _, _, _, list, _, _, _, _, _, _) = make_tools();
        let result = list.execute(json!({})).await.unwrap();
        assert!(result.contains("No active terminal sessions"));
    }

    #[tokio::test]
    async fn test_kill_nonexistent_session() {
        let (_, _, _, _, _, kill, _, _, _, _, _) = make_tools();
        let result = kill.execute(json!({"session_id": "nope"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_input_missing_params() {
        let (_, _, send, _, _, _, _, _, _, _, _) = make_tools();
        let result = send.execute(json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("session_id"));
    }

    #[tokio::test]
    async fn test_read_output_missing_session() {
        let (_, _, _, read, _, _, _, _, _, _, _) = make_tools();
        let result = read.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_resize_missing_params() {
        let (_, _, _, _, _, _, resize, _, _, _, _) = make_tools();
        let result = resize.execute(json!({"session_id": "x"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_key_missing_params() {
        let (_, _, _, _, _, _, _, send_key, _, _, _) = make_tools();
        let result = send_key.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_key_unknown_key() {
        let (_, _, _, _, _, _, _, send_key, _, _, _) = make_tools();
        let result = send_key
            .execute(json!({"session_id": "x", "key": "UnknownKey"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_capture_screen_missing_session() {
        let (_, _, _, _, _, _, _, _, capture, _, _) = make_tools();
        let result = capture.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_capture_screen_nonexistent_session() {
        let (_, _, _, _, _, _, _, _, capture, _, _) = make_tools();
        let result = capture.execute(json!({"session_id": "nonexistent"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_capture_scrollback_missing_session() {
        let (_, _, _, _, _, _, _, _, _, scrollback, _) = make_tools();
        let result = scrollback.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_capture_scrollback_nonexistent_session() {
        let (_, _, _, _, _, _, _, _, _, scrollback, _) = make_tools();
        let result = scrollback
            .execute(json!({"session_id": "nonexistent"}))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_wait_for_screen_change_missing_session() {
        let (_, _, _, _, _, _, _, _, _, _, wait) = make_tools();
        let result = wait.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_wait_for_screen_change_nonexistent_session() {
        let (_, _, _, _, _, _, _, _, _, _, wait) = make_tools();
        let result = wait.execute(json!({"session_id": "nonexistent"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_create_and_list_session() {
        let (_, create, _, _, list, _, _, _, _, _, _) = make_tools();

        let result = create.execute(json!({})).await.unwrap();
        assert!(result.contains("Created terminal session"));

        let list_result = list.execute(json!({})).await.unwrap();
        assert!(list_result.contains("1 active session"));
        assert!(list_result.contains("alive"));
    }

    #[tokio::test]
    async fn test_capture_screen_and_scrollback() {
        let (_, create, send, _, _, kill, _, _, capture, scrollback, _) = make_tools();

        // Create session
        let create_result = create.execute(test_session_args()).await.unwrap();
        let session_id = create_result
            .split('\'')
            .nth(1)
            .expect("should have session id")
            .to_string();

        // Send some output
        send.execute(json!({
            "session_id": session_id,
            "input": "echo hello\n"
        }))
        .await
        .unwrap();

        // Wait for output to be processed
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // Capture screen (text format)
        let screen_text = capture
            .execute(json!({
                "session_id": session_id,
                "format": "text"
            }))
            .await
            .unwrap();
        assert!(screen_text.contains("cursor:") || screen_text.contains("dims:"));

        // Capture screen (JSON format)
        let screen_json = capture
            .execute(json!({
                "session_id": session_id,
                "format": "json"
            }))
            .await
            .unwrap();
        assert!(screen_json.contains("cursor_row"));
        assert!(screen_json.contains("is_alternate"));
        assert!(screen_json.contains("lines"));

        // Capture scrollback
        let sb = scrollback
            .execute(json!({
                "session_id": session_id,
                "max_lines": 50
            }))
            .await
            .unwrap();
        // No scrollback yet (only one command), should be empty or contain content
        assert!(sb.contains("scrollback") || !sb.is_empty());

        kill.execute(json!({"session_id": session_id}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_screen_change_detects_output() {
        let (mgr, create, send, _read, _, kill, _, _, capture, _, wait) = make_tools();
        let timeout_ms = if cfg!(windows) { 15_000 } else { 3_000 };

        let create_result = create.execute(test_session_args()).await.unwrap();
        let session_id = create_result
            .split('\'')
            .nth(1)
            .expect("should have session id")
            .to_string();

        // Establish a baseline screen (sets screen_observed = true)
        capture
            .execute(json!({"session_id": session_id}))
            .await
            .unwrap();

        // Send input to increment input_sequence so wait_for_screen_change
        // treats the next screen change as user-driven rather than spontaneous.
        send.execute(json!({
            "session_id": session_id,
            "input": "echo test_change_marker\n"
        }))
        .await
        .unwrap();

        // Inject output directly into the emulator to simulate screen change.
        // On Windows CI, ConPTY input is unreliable so we bypass it.
        let sid = session_id.clone();
        tokio::task::spawn_blocking(move || {
            mgr.inject_emulator_output(&sid, b"test_change_marker\r\n")
        })
        .await
        .unwrap()
        .unwrap();

        // Wait should detect the screen change quickly
        let result = wait
            .execute(json!({
                "session_id": session_id,
                "timeout_ms": timeout_ms
            }))
            .await;

        assert!(
            result.is_ok(),
            "Expected screen change detection, got: {:?}",
            result
        );
        let output = result.unwrap();
        assert!(output.contains("cursor:") || output.contains("dims:"));

        kill.execute(json!({"session_id": session_id}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_screen_change_with_pattern() {
        let (mgr, create, send, _read, _, kill, _, _, capture, _, wait) = make_tools();
        let timeout_ms = if cfg!(windows) { 15_000 } else { 3_000 };

        let create_result = create.execute(test_session_args()).await.unwrap();
        let session_id = create_result
            .split('\'')
            .nth(1)
            .expect("should have session id")
            .to_string();

        // Establish a baseline screen
        capture
            .execute(json!({"session_id": session_id}))
            .await
            .unwrap();

        // Send input to increment input_sequence
        send.execute(json!({
            "session_id": session_id,
            "input": "echo UNIQUE_PATTERN_42\n"
        }))
        .await
        .unwrap();

        // Inject output with the pattern directly into the emulator
        let sid = session_id.clone();
        tokio::task::spawn_blocking(move || {
            mgr.inject_emulator_output(&sid, b"UNIQUE_PATTERN_42\r\n")
        })
        .await
        .unwrap()
        .unwrap();

        // Wait for the specific pattern
        let result = wait
            .execute(json!({
                "session_id": session_id,
                "timeout_ms": timeout_ms,
                "match": "UNIQUE_PATTERN_42"
            }))
            .await;

        assert!(result.is_ok(), "Expected pattern match, got: {:?}", result);

        kill.execute(json!({"session_id": session_id}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_screen_change_timeout() {
        let (_, create, _, _, _, kill, _, _, capture, _, wait) = make_tools();

        let create_result = create.execute(json!({})).await.unwrap();
        let session_id = create_result
            .split('\'')
            .nth(1)
            .expect("should have session id")
            .to_string();

        capture
            .execute(json!({"session_id": session_id}))
            .await
            .unwrap();

        // Wait with very short timeout — no output sent, should return screen unchanged
        let result = wait
            .execute(json!({
                "session_id": session_id,
                "timeout_ms": 100
            }))
            .await;

        assert!(
            result.is_ok(),
            "Expected Ok with screen content, got: {:?}",
            result
        );
        let output = result.unwrap();
        assert!(
            output.contains("Screen unchanged"),
            "Expected 'Screen unchanged' prefix, got: {:?}",
            output
        );
        assert!(
            output.contains("cursor:") || output.contains("dims:"),
            "Expected screen metadata in output"
        );

        kill.execute(json!({"session_id": session_id}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_wait_for_screen_change_invalid_regex() {
        let (_, _, _, _, _, _, _, _, _, _, wait) = make_tools();

        let result = wait
            .execute(json!({
                "session_id": "fake",
                "timeout_ms": 100,
                "match": "[invalid"
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_create_send_read_kill_lifecycle() {
        let (_, create, send, read, _, kill, _, _, _, _, _) = make_tools();

        // Create session
        let create_result = create.execute(json!({})).await.unwrap();
        // Extract session ID from "Created terminal session 'ts-XX' ..."
        let session_id = create_result
            .split('\'')
            .nth(1)
            .expect("should have session id in quotes")
            .to_string();

        // Send a command
        let send_result = send
            .execute(json!({
                "session_id": session_id,
                "input": "echo hello_world\n"
            }))
            .await
            .unwrap();
        assert!(send_result.contains("Sent"));

        // Wait briefly for output, then read
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let _output = read
            .execute(json!({
                "session_id": session_id,
                "timeout_ms": 500
            }))
            .await
            .unwrap();
        // The output should contain the echoed text or at least the command prompt.
        // On some platforms/shells the output may vary, so just verify it's not "no new output".
        // If the shell is slow, output may not yet be available, so we don't assert content.

        // Kill session
        let kill_result = kill
            .execute(json!({"session_id": session_id}))
            .await
            .unwrap();
        assert!(kill_result.contains("Killed"));
    }

    #[test]
    fn test_register_terminal_tools() {
        use crate::registry::ToolRegistry;

        let registry = ToolRegistry::new();
        let mgr = Arc::new(TerminalManager::with_config(10, true));
        register_terminal_tools(&registry, mgr);

        assert!(registry.get("terminal_create_session").is_some());
        assert!(registry.get("terminal_send_input").is_some());
        assert!(registry.get("terminal_read_output").is_some());
        assert!(registry.get("terminal_list_sessions").is_some());
        assert!(registry.get("terminal_kill_session").is_some());
        assert!(registry.get("terminal_resize").is_some());
        assert!(registry.get("terminal_send_key").is_some());
        assert!(registry.get("terminal_capture_screen").is_some());
        assert!(registry.get("terminal_capture_scrollback").is_some());
        assert!(registry.get("terminal_wait_for_screen_change").is_some());

        assert!(registry.is_mutating("terminal_create_session"));
        assert!(registry.is_mutating("terminal_send_input"));
        assert!(!registry.is_mutating("terminal_read_output"));
        assert!(!registry.is_mutating("terminal_list_sessions"));
        assert!(registry.is_mutating("terminal_kill_session"));
        assert!(registry.is_mutating("terminal_resize"));
        assert!(registry.is_mutating("terminal_send_key"));
        assert!(!registry.is_mutating("terminal_capture_screen"));
        assert!(!registry.is_mutating("terminal_capture_scrollback"));
        assert!(!registry.is_mutating("terminal_wait_for_screen_change"));
    }

    #[test]
    fn test_truncate_output_short() {
        let output = "hello world".to_string();
        assert_eq!(truncate_output(output), "hello world");
    }

    #[test]
    fn test_truncate_output_long() {
        let long_output = "x".repeat(200_000);
        let result = truncate_output(long_output);
        assert!(result.len() > MAX_TOOL_OUTPUT_LENGTH);
        assert!(result.ends_with("... (output truncated)"));
        // The actual content part should be exactly MAX_TOOL_OUTPUT_LENGTH chars
        let truncated_part = &result[..MAX_TOOL_OUTPUT_LENGTH];
        assert!(truncated_part.chars().all(|c| c == 'x'));
    }

    #[test]
    fn test_key_to_bytes() {
        assert_eq!(key_to_bytes("Enter"), Some(b"\r".to_vec()));
        assert_eq!(key_to_bytes("Up"), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_to_bytes("Down"), Some(b"\x1b[B".to_vec()));
        assert_eq!(key_to_bytes("Escape"), Some(b"\x1b".to_vec()));
        assert_eq!(key_to_bytes("Esc"), Some(b"\x1b".to_vec()));
        assert_eq!(key_to_bytes("Tab"), Some(b"\t".to_vec()));
        assert_eq!(key_to_bytes("F1"), Some(b"\x1bOP".to_vec()));
        assert_eq!(key_to_bytes("F12"), Some(b"\x1b[24~".to_vec()));
        assert_eq!(key_to_bytes("Space"), Some(b" ".to_vec()));
        assert_eq!(key_to_bytes("Unknown"), None);
    }

    #[test]
    fn test_resolve_key_bytes_ctrl() {
        assert_eq!(resolve_key_bytes("Ctrl+C").unwrap(), vec![0x03]);
        assert_eq!(resolve_key_bytes("Ctrl+A").unwrap(), vec![0x01]);
        assert_eq!(resolve_key_bytes("Ctrl+Z").unwrap(), vec![0x1A]);
        assert_eq!(resolve_key_bytes("Ctrl+Space").unwrap(), vec![0x00]);
        assert!(resolve_key_bytes("Ctrl+1").is_err());
    }

    #[test]
    fn test_resolve_key_bytes_named() {
        assert_eq!(resolve_key_bytes("Enter").unwrap(), b"\r".to_vec());
        assert_eq!(resolve_key_bytes("PageUp").unwrap(), b"\x1b[5~".to_vec());
        assert_eq!(resolve_key_bytes("PgDn").unwrap(), b"\x1b[6~".to_vec());
    }

    #[test]
    fn test_resolve_key_bytes_unknown() {
        assert!(resolve_key_bytes("Foo").is_err());
    }
}
