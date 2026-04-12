//! Spawn tool — create background subagent tasks.

use crate::trait_def::{Tool, ToolError};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

/// Callback type for spawning a subagent task.
///
/// The first argument is the task name, the second is the task description.
/// Returns a string result (typically a task ID or status message).
type SpawnCallback = dyn Fn(String, String) -> String + Send + Sync;

/// Tool for spawning background subagent tasks.
pub struct SpawnTool {
    spawn_callback: Option<Arc<SpawnCallback>>,
}

impl SpawnTool {
    pub fn new() -> Self {
        Self {
            spawn_callback: None,
        }
    }

    /// Provide a callback that will be invoked to spawn a subagent.
    ///
    /// The callback receives `(name, task_description)` and should return a
    /// status string (e.g. the spawned task ID).
    pub fn with_spawn_callback(mut self, cb: Arc<SpawnCallback>) -> Self {
        self.spawn_callback = Some(cb);
        self
    }
}

impl Default for SpawnTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SpawnTool {
    fn name(&self) -> &str {
        "spawn"
    }

    fn description(&self) -> &str {
        "Spawn a background agent task. The task runs independently and can report results later."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "Description of the task to perform" },
                "name": { "type": "string", "description": "Name for the subagent" },
            },
            "required": ["task"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let task = args["task"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'task'".to_string()))?;

        let name = args["name"].as_str().unwrap_or("unnamed").to_string();

        if let Some(ref cb) = self.spawn_callback {
            let result = cb(name, task.to_string());
            Ok(result)
        } else {
            Ok(format!("Spawned background task: {}", task))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spawn_tool_metadata() {
        let tool = SpawnTool::new();
        assert_eq!(tool.name(), "spawn");
        assert!(!tool.description().is_empty());
    }

    #[tokio::test]
    async fn test_spawn_tool_execute() {
        let tool = SpawnTool::new();
        let result = tool
            .execute(serde_json::json!({
                "task": "research topic X"
            }))
            .await
            .unwrap();
        assert!(result.contains("Spawned background task"));
        assert!(result.contains("research topic X"));
    }

    #[tokio::test]
    async fn test_spawn_tool_missing_task() {
        let tool = SpawnTool::new();
        let result = tool.execute(serde_json::json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Missing 'task'"));
    }

    #[test]
    fn test_spawn_tool_default() {
        let tool = SpawnTool::default();
        assert_eq!(tool.name(), "spawn");
    }

    #[tokio::test]
    async fn test_spawn_tool_with_callback() {
        let cb = Arc::new(|name: String, task: String| -> String {
            format!("Task '{}' spawned with id task-001: {}", name, task)
        });

        let tool = SpawnTool::new().with_spawn_callback(cb);

        let result = tool
            .execute(serde_json::json!({
                "task": "research topic X",
                "name": "researcher"
            }))
            .await
            .unwrap();

        assert!(result.contains("task-001"));
        assert!(result.contains("researcher"));
        assert!(result.contains("research topic X"));
    }

    #[tokio::test]
    async fn test_spawn_tool_with_callback_no_name() {
        let cb = Arc::new(|name: String, task: String| -> String {
            format!("name={}, task={}", name, task)
        });

        let tool = SpawnTool::new().with_spawn_callback(cb);

        let result = tool
            .execute(serde_json::json!({
                "task": "do something"
            }))
            .await
            .unwrap();

        assert_eq!(result, "name=unnamed, task=do something");
    }
}
