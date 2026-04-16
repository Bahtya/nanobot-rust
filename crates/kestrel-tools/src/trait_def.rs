//! Tool trait definition and sub-agent spawner interface.

use async_trait::async_trait;
use kestrel_core::FunctionDefinition;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Error type for tool execution.
#[derive(Error, Debug)]
pub enum ToolError {
    #[error("Parameter validation error: {0}")]
    Validation(String),

    #[error("Execution error: {0}")]
    Execution(String),

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Not available: {0}")]
    NotAvailable(String),
}

/// Core trait for agent tools.
///
/// Each tool implements this trait to provide a name, schema, and execution handler.
/// Mirrors the Python `tools/base.py` Tool class.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (used in function calls).
    fn name(&self) -> &str;

    /// Human-readable description for the LLM.
    fn description(&self) -> &str;

    /// JSON Schema for the tool's parameters.
    fn parameters_schema(&self) -> Value;

    /// Which toolset this tool belongs to (for filtering).
    fn toolset(&self) -> &str {
        "default"
    }

    /// Whether this tool is currently available.
    fn is_available(&self) -> bool {
        true
    }

    /// Whether this tool mutates external state (files, shell, etc.).
    ///
    /// Mutating tools are serialized so only one runs at a time, preventing
    /// race conditions when the LLM issues multiple state-changing calls
    /// concurrently. Read-only tools (search, read, grep) return `false`
    /// and run in parallel.
    fn is_mutating(&self) -> bool {
        false
    }

    /// Required environment variables for this tool.
    fn required_env_vars(&self) -> Vec<&str> {
        vec![]
    }

    /// Execute the tool with the given arguments.
    async fn execute(&self, args: Value) -> Result<String, ToolError>;

    /// Get the OpenAI function definition format.
    fn to_function_definition(&self) -> FunctionDefinition {
        FunctionDefinition {
            name: self.name().to_string(),
            description: Some(self.description().to_string()),
            parameters: Some(self.parameters_schema()),
        }
    }
}

// ─── Sub-agent spawner trait ────────────────────────────────────

/// Status of a spawned sub-agent task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpawnStatus {
    /// Task is currently executing.
    Running,
    /// Task completed successfully with the given output.
    Completed(String),
    /// Task failed with the given error message.
    Failed(String),
}

/// Trait for spawning and managing sub-agent tasks.
///
/// Implementors provide the concrete mechanism for creating sub-agents,
/// tracking their status, and cancelling them. `SpawnTool` delegates to
/// whichever `SubAgentSpawner` is injected at wiring time.
#[async_trait]
pub trait SubAgentSpawner: Send + Sync {
    /// Spawn a new sub-agent task with the given name, prompt, and optional
    /// context. Returns a unique task ID on success.
    async fn spawn(
        &self,
        name: &str,
        prompt: &str,
        context: Option<String>,
    ) -> anyhow::Result<String>;

    /// Query the current status of a spawned task.
    /// Returns `None` if the task ID is unknown.
    async fn status(&self, task_id: &str) -> Option<SpawnStatus>;

    /// Request cancellation of a running task.
    /// Returns `true` if the task was found and signalled for cancellation.
    async fn cancel(&self, task_id: &str) -> bool;

    /// List all tracked tasks as `(id, name, status)` tuples.
    async fn list(&self) -> Vec<(String, String, SpawnStatus)>;

    /// Spawn a sub-agent task with an explicit per-task timeout.
    ///
    /// If `timeout_secs` is `Some(secs)`, the sub-agent will be killed
    /// after that many seconds. `None` falls back to the manager default.
    ///
    /// The default implementation delegates to [`spawn`](Self::spawn),
    /// ignoring the timeout parameter. Implementors that support timeouts
    /// should override this method.
    async fn spawn_with_timeout(
        &self,
        name: &str,
        prompt: &str,
        context: Option<String>,
        timeout_secs: Option<u64>,
    ) -> anyhow::Result<String> {
        let _ = timeout_secs;
        self.spawn(name, prompt, context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_error_display() {
        let validation = ToolError::Validation("bad param".to_string());
        assert!(validation.to_string().contains("bad param"));

        let execution = ToolError::Execution("something failed".to_string());
        assert!(execution.to_string().contains("something failed"));

        let timeout = ToolError::Timeout("30s".to_string());
        assert!(timeout.to_string().contains("30s"));

        let permission = ToolError::PermissionDenied("no access".to_string());
        assert!(permission.to_string().contains("no access"));

        let not_available = ToolError::NotAvailable("missing key".to_string());
        assert!(not_available.to_string().contains("missing key"));
    }

    #[test]
    fn test_tool_is_mutating_default_is_false() {
        struct DefaultTool;
        #[async_trait]
        impl Tool for DefaultTool {
            fn name(&self) -> &str {
                "default"
            }
            fn description(&self) -> &str {
                "default tool"
            }
            fn parameters_schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _args: Value) -> Result<String, ToolError> {
                Ok("ok".to_string())
            }
        }
        let tool = DefaultTool;
        assert!(!tool.is_mutating());
    }

    #[test]
    fn test_tool_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ToolError>();
    }

    #[test]
    fn test_spawn_status_serde_roundtrip() {
        let statuses = vec![
            SpawnStatus::Running,
            SpawnStatus::Completed("done".to_string()),
            SpawnStatus::Failed("oops".to_string()),
        ];
        for s in &statuses {
            let json = serde_json::to_string(s).unwrap();
            let back: SpawnStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, s);
        }
    }

    #[test]
    fn test_spawn_status_equality() {
        assert_eq!(SpawnStatus::Running, SpawnStatus::Running);
        assert_ne!(SpawnStatus::Running, SpawnStatus::Completed("".into()));
        assert_eq!(
            SpawnStatus::Completed("ok".into()),
            SpawnStatus::Completed("ok".into())
        );
    }

    /// Verify the default `spawn_with_timeout` delegates to `spawn`.
    #[tokio::test]
    async fn test_spawn_with_timeout_default_delegates() {
        struct StubSpawner;
        #[async_trait]
        impl SubAgentSpawner for StubSpawner {
            async fn spawn(
                &self,
                name: &str,
                prompt: &str,
                _context: Option<String>,
            ) -> anyhow::Result<String> {
                Ok(format!("{}:{}", name, prompt))
            }
            async fn status(&self, _task_id: &str) -> Option<SpawnStatus> {
                None
            }
            async fn cancel(&self, _task_id: &str) -> bool {
                false
            }
            async fn list(&self) -> Vec<(String, String, SpawnStatus)> {
                vec![]
            }
        }

        let spawner = StubSpawner;
        let id = spawner
            .spawn_with_timeout("test", "hello", None, Some(30))
            .await
            .unwrap();
        assert_eq!(id, "test:hello");
    }
}
