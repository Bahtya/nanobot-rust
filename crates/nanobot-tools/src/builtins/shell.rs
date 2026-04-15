//! Shell command execution tool.

use crate::trait_def::{Tool, ToolError};
use async_trait::async_trait;
use nanobot_core::MAX_TOOL_OUTPUT_LENGTH;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::debug;

const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 30;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_MEMORY_KIB: u64 = 256 * 1024;

/// Tool for executing shell commands.
pub struct ExecTool {
    timeout: Duration,
    dangerous: bool,
    max_output_bytes: usize,
    max_memory_kib: u64,
}

impl ExecTool {
    /// Create a new exec tool with safe defaults.
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_EXEC_TIMEOUT_SECS),
            dangerous: false,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_memory_kib: DEFAULT_MAX_MEMORY_KIB,
        }
    }
}

impl Default for ExecTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ExecTool {
    /// Override the default execution timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the maximum combined stdout/stderr output captured for a command.
    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    /// Override the memory limit applied to sandboxed commands.
    pub fn with_max_memory_kib(mut self, max_memory_kib: u64) -> Self {
        self.max_memory_kib = max_memory_kib;
        self
    }

    /// Disable exec sandbox restrictions for trusted environments.
    pub fn dangerous(mut self, dangerous: bool) -> Self {
        self.dangerous = dangerous;
        self
    }
}

/// Shell builtins that are explicitly allowed unless combined with blocked patterns.
const ALLOWED_SHELL_BUILTINS: &[&str] = &[
    ".", ":", "alias", "bg", "cd", "echo", "env", "eval", "exec", "export", "false", "fg",
    "printf", "pwd", "read", "set", "test", "times", "true", "type", "ulimit", "umask", "unset",
];

/// Executables or shell entry points that should never run in sandboxed mode.
const BLOCKED_COMMANDS: &[&str] = &[
    "mkfs",
    "mkfs.ext2",
    "mkfs.ext3",
    "mkfs.ext4",
    "mkfs.xfs",
    "dd",
    "format",
    "shutdown",
    "reboot",
    "poweroff",
    "halt",
    "init",
    "telinit",
    "wipefs",
    "fdisk",
    "sfdisk",
    "parted",
];

/// Dangerous command patterns that should be rejected before execution.
const BLOCKED_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -fr /",
    "rm -rf --no-preserve-root /",
    "dd if=",
    ":(){ :|:& };:",
    "> /dev/sda",
    "> /dev/vda",
    "shutdown ",
    "reboot ",
    "poweroff ",
    "halt ",
    "init 0",
    "init 6",
];

fn extract_command_tokens(command: &str) -> Vec<String> {
    command
        .split(|c: char| c.is_whitespace() || matches!(c, '|' | ';' | '&' | '(' | ')' | '\n'))
        .filter_map(|token| {
            let trimmed = token.trim_matches(|c| c == '"' || c == '\'' || c == '`');
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_lowercase())
            }
        })
        .collect()
}

fn is_command_safe(command: &str) -> bool {
    let lower = command.to_lowercase();
    if BLOCKED_PATTERNS
        .iter()
        .any(|blocked| lower.contains(blocked))
    {
        return false;
    }

    let tokens = extract_command_tokens(command);
    for token in tokens {
        if ALLOWED_SHELL_BUILTINS.contains(&token.as_str()) {
            continue;
        }
        if BLOCKED_COMMANDS.contains(&token.as_str()) {
            return false;
        }
    }

    true
}

fn format_output(stdout: &[u8], stderr: &[u8], status: std::process::ExitStatus) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);

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
    if !status.success() {
        result.push_str(&format!("\n[exit code: {}]", status.code().unwrap_or(-1)));
    }

    if result.len() > MAX_TOOL_OUTPUT_LENGTH {
        result.truncate(MAX_TOOL_OUTPUT_LENGTH);
        result.push_str("\n... (output truncated)");
    }

    result
}

async fn pump_output<R>(
    mut reader: R,
    is_stderr: bool,
    tx: mpsc::UnboundedSender<(bool, Vec<u8>)>,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut buf = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        if tx.send((is_stderr, buf[..read].to_vec())).is_err() {
            break;
        }
    }
    Ok(())
}

fn build_command(command: &str, dangerous: bool, max_memory_kib: u64) -> Command {
    let mut cmd = Command::new("sh");
    cmd.kill_on_drop(true);
    if dangerous {
        cmd.arg("-c").arg(command);
    } else {
        #[cfg(unix)]
        {
            cmd.arg("-c")
                .arg("ulimit -v \"$1\"; exec /bin/sh -c \"$2\"")
                .arg("sh")
                .arg(max_memory_kib.to_string())
                .arg(command);
        }
        #[cfg(not(unix))]
        {
            let _ = max_memory_kib;
            cmd.arg("-c").arg(command);
        }
    }
    cmd
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
                    "description": "Timeout in seconds (default: 30)",
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

    fn is_mutating(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'command' parameter".to_string()))?;

        if !self.dangerous && !is_command_safe(command) {
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

        let mut cmd = build_command(command, self.dangerous, self.max_memory_kib);

        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| ToolError::Execution(format!("Failed to execute command: {}", e)))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Execution("Failed to capture stdout".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Execution("Failed to capture stderr".to_string()))?;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let stdout_task = tokio::spawn(pump_output(stdout, false, tx.clone()));
        let stderr_task = tokio::spawn(pump_output(stderr, true, tx));

        let started_at = Instant::now();
        let timeout = Duration::from_secs(timeout_secs);
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        let mut total_output = 0_usize;
        let status = loop {
            while let Ok((is_stderr, chunk)) = rx.try_recv() {
                total_output += chunk.len();
                if total_output > self.max_output_bytes {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    stdout_task.abort();
                    stderr_task.abort();
                    return Err(ToolError::Execution(format!(
                        "Command exceeded max output size of {} bytes",
                        self.max_output_bytes
                    )));
                }

                if is_stderr {
                    stderr_buf.extend_from_slice(&chunk);
                } else {
                    stdout_buf.extend_from_slice(&chunk);
                }
            }

            if started_at.elapsed() >= timeout {
                let _ = child.kill().await;
                let _ = child.wait().await;
                stdout_task.abort();
                stderr_task.abort();
                return Err(ToolError::Timeout(format!(
                    "Command timed out after {}s",
                    timeout_secs
                )));
            }

            if let Some(status) = child
                .try_wait()
                .map_err(|e| ToolError::Execution(format!("Failed to wait for command: {}", e)))?
            {
                break status;
            }

            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        while let Some((is_stderr, chunk)) = rx.recv().await {
            total_output += chunk.len();
            if total_output > self.max_output_bytes {
                stdout_task.abort();
                stderr_task.abort();
                return Err(ToolError::Execution(format!(
                    "Command exceeded max output size of {} bytes",
                    self.max_output_bytes
                )));
            }

            if is_stderr {
                stderr_buf.extend_from_slice(&chunk);
            } else {
                stdout_buf.extend_from_slice(&chunk);
            }
        }

        let _ = stdout_task.await;
        let _ = stderr_task.await;

        Ok(format_output(&stdout_buf, &stderr_buf, status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trait_def::Tool;
    use std::time::Duration;

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
        assert!(!is_command_safe("shutdown -h now"));
        assert!(!is_command_safe("reboot"));
        assert!(!is_command_safe("RM -RF /"));
    }

    #[test]
    fn test_extract_command_tokens_handles_shell_separators() {
        let tokens = extract_command_tokens("echo ok && shutdown now | cat");
        assert!(tokens.contains(&"echo".to_string()));
        assert!(tokens.contains(&"shutdown".to_string()));
        assert!(tokens.contains(&"cat".to_string()));
    }

    #[test]
    fn test_exec_tool_construction() {
        let tool = ExecTool::new();
        assert_eq!(tool.name(), "exec");
        assert!(!tool.description().is_empty());
        assert!(!tool.dangerous);
        assert_eq!(tool.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
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
    async fn test_exec_tool_dangerous_mode_bypasses_blocklist() {
        let tool = ExecTool::new().dangerous(true);
        let result = tool.execute(json!({"command": "echo 'rm -rf /'"})).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("rm -rf /"));
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
        let tool = ExecTool::new().with_timeout(Duration::from_secs(5));
        assert_eq!(tool.name(), "exec");
    }

    #[tokio::test]
    async fn test_exec_tool_timeout_enforced() {
        let tool = ExecTool::new().with_timeout(Duration::from_millis(200));
        let result = tool.execute(json!({"command": "sleep 1"})).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ToolError::Timeout(_)));
    }

    #[tokio::test]
    async fn test_exec_tool_output_limit_enforced() {
        let tool = ExecTool::new().with_max_output_bytes(1024);
        let result = tool
            .execute(json!({"command": "yes x | head -c 4096"}))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("max output size"));
    }

    #[cfg(unix)]
    #[test]
    fn test_build_command_applies_memory_limit_when_sandboxed() {
        let cmd = build_command("echo hi", false, 4096);
        let debug = format!("{cmd:?}");
        assert!(debug.contains("ulimit -v"));
        assert!(debug.contains("4096"));
    }

    #[test]
    fn test_build_command_skips_memory_limit_in_dangerous_mode() {
        let cmd = build_command("echo hi", true, 4096);
        let debug = format!("{cmd:?}");
        assert!(!debug.contains("ulimit -v"));
    }
}
