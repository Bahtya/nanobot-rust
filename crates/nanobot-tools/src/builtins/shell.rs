//! Shell command execution tool.

use crate::trait_def::{Tool, ToolError};
use async_trait::async_trait;
use nanobot_core::DEFAULT_TOOL_TIMEOUT_SECS;
use serde_json::{json, Value};
use std::time::Duration;
use tracing::debug;

/// Tool for executing shell commands.
pub struct ExecTool {
    timeout: Duration,
    sandboxed: bool,
}

impl ExecTool {
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_TOOL_TIMEOUT_SECS),
            sandboxed: false,
        }
    }
}

impl Default for ExecTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecTool {
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn sandboxed(mut self, sandboxed: bool) -> Self {
        self.sandboxed = sandboxed;
        self
    }
}

/// Commands that should be blocked for safety.
const BLOCKED_COMMANDS: &[&str] = &["rm -rf /", "mkfs", "dd if=", ":(){ :|:& };:", "> /dev/sda"];

fn is_command_safe(command: &str) -> bool {
    let lower = command.to_lowercase();
    !BLOCKED_COMMANDS
        .iter()
        .any(|blocked| lower.contains(blocked))
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        "exec"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return the output. Use for running system commands, scripts, and programs."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute",
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 120)",
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory for the command",
                },
            },
            "required": ["command"],
        })
    }

    fn toolset(&self) -> &str {
        "default"
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'command' parameter".to_string()))?;

        if !is_command_safe(command) {
            return Err(ToolError::PermissionDenied(
                "Command contains potentially destructive operations".to_string(),
            ));
        }

        let timeout_secs = args["timeout"].as_u64().unwrap_or(self.timeout.as_secs());
        let cwd = args["cwd"].as_str();

        debug!(
            "Executing command: {} (timeout: {}s)",
            command, timeout_secs
        );

        let mut cmd = if self.sandboxed {
            // In sandboxed mode, wrap with bubblewrap
            let mut cmd = tokio::process::Command::new("bwrap");
            cmd.arg("--ro-bind")
                .arg("/usr")
                .arg("/usr")
                .arg("--ro-bind")
                .arg("/lib")
                .arg("/lib")
                .arg("--ro-bind")
                .arg("/lib64")
                .arg("/lib64")
                .arg("--ro-bind")
                .arg("/bin")
                .arg("/bin")
                .arg("--ro-bind")
                .arg("/sbin")
                .arg("/sbin")
                .arg("--dev")
                .arg("/dev")
                .arg("--proc")
                .arg("/proc")
                .arg("--unshare-all")
                .arg("--die-with-parent")
                .arg("--")
                .arg("/bin/sh")
                .arg("-c")
                .arg(command);
            cmd
        } else {
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c").arg(command);
            cmd
        };

        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let output = tokio::time::timeout(Duration::from_secs(timeout_secs), cmd.output())
            .await
            .map_err(|_| ToolError::Timeout(format!("Command timed out after {}s", timeout_secs)))?
            .map_err(|e| ToolError::Execution(format!("Failed to execute command: {}", e)))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let mut result = String::new();
        if !stdout.is_empty() {
            result.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str("[stderr] ");
            result.push_str(&stderr);
        }
        if !output.status.success() {
            result.push_str(&format!(
                "\n[exit code: {}]",
                output.status.code().unwrap_or(-1)
            ));
        }

        // Truncate if too long
        if result.len() > nanobot_core::MAX_TOOL_OUTPUT_LENGTH {
            result.truncate(nanobot_core::MAX_TOOL_OUTPUT_LENGTH);
            result.push_str("\n... (output truncated)");
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trait_def::Tool;

    #[test]
    fn test_is_command_safe_normal() {
        assert!(is_command_safe("ls -la"));
        assert!(is_command_safe("echo hello"));
        assert!(is_command_safe("cat file.txt"));
        assert!(is_command_safe("python script.py"));
    }

    #[test]
    fn test_is_command_safe_blocked() {
        assert!(!is_command_safe("rm -rf /"));
        assert!(!is_command_safe("mkfs /dev/sda1"));
        assert!(!is_command_safe("dd if=/dev/zero of=/dev/sda"));
        assert!(!is_command_safe("RM -RF /"));
    }

    #[test]
    fn test_exec_tool_construction() {
        let tool = ExecTool::new();
        assert_eq!(tool.name(), "exec");
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn test_exec_tool_schema() {
        let tool = ExecTool::new();
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().unwrap();
        let required_names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(required_names.contains(&"command"));
    }

    #[tokio::test]
    async fn test_exec_tool_echo() {
        let tool = ExecTool::new();
        let result = tool.execute(json!({"command": "echo hello world"})).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("hello world"));
    }

    #[tokio::test]
    async fn test_exec_tool_missing_command() {
        let tool = ExecTool::new();
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing 'command'"));
    }

    #[tokio::test]
    async fn test_exec_tool_blocked_command() {
        let tool = ExecTool::new();
        let result = tool.execute(json!({"command": "rm -rf /"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("destructive"));
    }

    #[tokio::test]
    async fn test_exec_tool_exit_code() {
        let tool = ExecTool::new();
        let result = tool.execute(json!({"command": "exit 42"})).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("exit code: 42"));
    }

    #[tokio::test]
    async fn test_exec_tool_stderr() {
        let tool = ExecTool::new();
        let result = tool.execute(json!({"command": "echo error >&2"})).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("[stderr]"));
        assert!(output.contains("error"));
    }

    #[tokio::test]
    async fn test_exec_tool_with_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ExecTool::new();
        let result = tool
            .execute(json!({
                "command": "pwd",
                "cwd": dir.path().to_str().unwrap()
            }))
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains(dir.path().to_str().unwrap()));
    }

    #[test]
    fn test_exec_tool_default() {
        let tool = ExecTool::default();
        assert_eq!(tool.name(), "exec");
    }

    #[test]
    fn test_exec_tool_with_timeout() {
        let tool = ExecTool::new().with_timeout(std::time::Duration::from_secs(5));
        assert_eq!(tool.name(), "exec");
    }
}
