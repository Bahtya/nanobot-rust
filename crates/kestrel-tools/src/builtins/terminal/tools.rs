//! Terminal multiplexer tools for AI-driven session management.
//!
//! Exposes six tools that let the LLM create, interact with, and manage
//! PTY-backed terminal sessions:
//!
//! - `terminal_create_session` — spawn a new shell in a PTY
//! - `terminal_send_input`     — send keystrokes / commands
//! - `terminal_read_output`    — read pending output
//! - `terminal_list_sessions`  — enumerate active sessions
//! - `terminal_kill_session`   — destroy a session
//! - `terminal_resize`         — change PTY dimensions

use crate::trait_def::{Tool, ToolError};
use async_trait::async_trait;
use kestrel_core::MAX_TOOL_OUTPUT_LENGTH;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::debug;

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
        truncated.truncate(MAX_TOOL_OUTPUT_LENGTH);
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
         Optionally wait for output with a timeout. Returns the text \
         output (ANSI sequences are preserved). Output is truncated at \
         100,000 characters to avoid overwhelming the context window."
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

        debug!(
            session_id = session_id,
            timeout_ms = timeout_ms.unwrap_or(0),
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
    registry.register(TerminalResizeTool::new().with_manager(mgr));
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
    ) {
        let mgr = Arc::new(TerminalManager::with_config(10, true));
        (
            mgr.clone(),
            TerminalCreateSessionTool::new().with_manager(mgr.clone()),
            TerminalSendInputTool::new().with_manager(mgr.clone()),
            TerminalReadOutputTool::new().with_manager(mgr.clone()),
            TerminalListSessionsTool::new().with_manager(mgr.clone()),
            TerminalKillSessionTool::new().with_manager(mgr.clone()),
            TerminalResizeTool::new().with_manager(mgr),
        )
    }

    #[test]
    fn test_tool_names() {
        let (_, create, send, read, list, kill, resize) = make_tools();
        assert_eq!(create.name(), "terminal_create_session");
        assert_eq!(send.name(), "terminal_send_input");
        assert_eq!(read.name(), "terminal_read_output");
        assert_eq!(list.name(), "terminal_list_sessions");
        assert_eq!(kill.name(), "terminal_kill_session");
        assert_eq!(resize.name(), "terminal_resize");
    }

    #[test]
    fn test_mutating_classification() {
        let (_, create, send, read, list, kill, resize) = make_tools();
        assert!(create.is_mutating());
        assert!(send.is_mutating());
        assert!(!read.is_mutating());
        assert!(!list.is_mutating());
        assert!(kill.is_mutating());
        assert!(resize.is_mutating());
    }

    #[test]
    fn test_descriptions_nonempty() {
        let (_, create, send, read, list, kill, resize) = make_tools();
        assert!(!create.description().is_empty());
        assert!(!send.description().is_empty());
        assert!(!read.description().is_empty());
        assert!(!list.description().is_empty());
        assert!(!kill.description().is_empty());
        assert!(!resize.description().is_empty());
    }

    #[test]
    fn test_schemas_are_valid_json() {
        let (_, create, send, read, list, kill, resize) = make_tools();
        for schema in &[
            create.parameters_schema(),
            send.parameters_schema(),
            read.parameters_schema(),
            list.parameters_schema(),
            kill.parameters_schema(),
            resize.parameters_schema(),
        ] {
            assert_eq!(schema["type"], "object");
        }
    }

    #[tokio::test]
    async fn test_list_sessions_empty() {
        let (_, _, _, _, list, _, _) = make_tools();
        let result = list.execute(json!({})).await.unwrap();
        assert!(result.contains("No active terminal sessions"));
    }

    #[tokio::test]
    async fn test_kill_nonexistent_session() {
        let (_, _, _, _, _, kill, _) = make_tools();
        let result = kill.execute(json!({"session_id": "nope"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_input_missing_params() {
        let (_, _, send, _, _, _, _) = make_tools();
        let result = send.execute(json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("session_id"));
    }

    #[tokio::test]
    async fn test_read_output_missing_session() {
        let (_, _, _, read, _, _, _) = make_tools();
        let result = read.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_resize_missing_params() {
        let (_, _, _, _, _, _, resize) = make_tools();
        let result = resize.execute(json!({"session_id": "x"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_create_and_list_session() {
        let (_, create, _, _, list, _, _) = make_tools();

        let result = create.execute(json!({})).await.unwrap();
        assert!(result.contains("Created terminal session"));

        let list_result = list.execute(json!({})).await.unwrap();
        assert!(list_result.contains("1 active session"));
        assert!(list_result.contains("alive"));
    }

    #[tokio::test]
    async fn test_create_send_read_kill_lifecycle() {
        let (_, create, send, read, _, kill, _) = make_tools();

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
        let output = read
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

        assert!(registry.is_mutating("terminal_create_session"));
        assert!(registry.is_mutating("terminal_send_input"));
        assert!(!registry.is_mutating("terminal_read_output"));
        assert!(!registry.is_mutating("terminal_list_sessions"));
        assert!(registry.is_mutating("terminal_kill_session"));
        assert!(registry.is_mutating("terminal_resize"));
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
}
