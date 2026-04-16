//! Spawn tool — create background subagent tasks via the SubAgentSpawner trait.

use crate::trait_def::{SubAgentSpawner, Tool, ToolError};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

/// Tool for spawning background sub-agent tasks.
///
/// When wired with a [`SubAgentSpawner`], the tool delegates to it for actual
/// task creation and tracking. Without a spawner, it returns a best-effort
/// acknowledgement string.
pub struct SpawnTool {
    spawner: Option<Arc<dyn SubAgentSpawner>>,
}

impl SpawnTool {
    /// Create a new SpawnTool without a backing spawner (returns acknowledgement strings).
    pub fn new() -> Self {
        Self { spawner: None }
    }

    /// Wire this tool to a concrete [`SubAgentSpawner`] implementation.
    ///
    /// When set, `execute` delegates to the spawner and returns the real task ID.
    pub fn with_spawner(mut self, spawner: Arc<dyn SubAgentSpawner>) -> Self {
        self.spawner = Some(spawner);
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
                "context": { "type": "string", "description": "Optional extra context to inject before the task prompt" },
            },
            "required": ["task"],
        })
    }

    fn is_mutating(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let task = args["task"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'task'".to_string()))?;

        let name = args["name"].as_str().unwrap_or("unnamed").to_string();
        let context = args["context"].as_str().map(String::from);

        if let Some(ref spawner) = self.spawner {
            match spawner.spawn(&name, task, context).await {
                Ok(task_id) => Ok(format!(
                    "Spawned sub-agent '{}' (id: {}). Use status('{}') to check progress.",
                    name, task_id, task_id
                )),
                Err(e) => Err(ToolError::Execution(format!("Failed to spawn task: {}", e))),
            }
        } else {
            Ok(format!(
                "Spawned background task '{}' (no spawner wired): {}",
                name, task
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trait_def::SpawnStatus;

    #[test]
    fn test_spawn_tool_metadata() {
        let tool = SpawnTool::new();
        assert_eq!(tool.name(), "spawn");
        assert!(!tool.description().is_empty());
    }

    #[tokio::test]
    async fn test_spawn_tool_execute_no_spawner() {
        let tool = SpawnTool::new();
        let result = tool
            .execute(serde_json::json!({
                "task": "research topic X"
            }))
            .await
            .unwrap();
        assert!(result.contains("no spawner wired"));
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

    /// Minimal mock spawner for testing the wiring.
    struct MockSpawner {
        status_response: SpawnStatus,
    }

    impl MockSpawner {
        fn new() -> Self {
            Self {
                status_response: SpawnStatus::Running,
            }
        }
    }

    #[async_trait]
    impl SubAgentSpawner for MockSpawner {
        async fn spawn(
            &self,
            name: &str,
            prompt: &str,
            context: Option<String>,
        ) -> anyhow::Result<String> {
            let id = format!("mock-{}-{}", name, prompt.len());
            let _ = context; // accepted but ignored in mock
            Ok(id)
        }

        async fn status(&self, _task_id: &str) -> Option<SpawnStatus> {
            Some(self.status_response.clone())
        }

        async fn cancel(&self, _task_id: &str) -> bool {
            true
        }

        async fn list(&self) -> Vec<(String, String, SpawnStatus)> {
            vec![]
        }
    }

    #[tokio::test]
    async fn test_spawn_tool_with_spawner() {
        let spawner = Arc::new(MockSpawner::new());
        let tool = SpawnTool::new().with_spawner(spawner);

        let result = tool
            .execute(serde_json::json!({
                "task": "research topic X",
                "name": "researcher",
                "context": "Focus on recent papers"
            }))
            .await
            .unwrap();

        assert!(result.contains("mock-researcher"));
        assert!(result.contains("Spawned sub-agent"));
    }

    #[tokio::test]
    async fn test_spawn_tool_with_spawner_no_name() {
        let spawner = Arc::new(MockSpawner::new());
        let tool = SpawnTool::new().with_spawner(spawner);

        let result = tool
            .execute(serde_json::json!({
                "task": "do something"
            }))
            .await
            .unwrap();

        assert!(result.contains("unnamed"));
    }

    #[tokio::test]
    async fn test_spawn_tool_spawner_error() {
        struct FailingSpawner;
        #[async_trait]
        impl SubAgentSpawner for FailingSpawner {
            async fn spawn(
                &self,
                _name: &str,
                _prompt: &str,
                _context: Option<String>,
            ) -> anyhow::Result<String> {
                Err(anyhow::anyhow!("no capacity"))
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

        let tool = SpawnTool::new().with_spawner(Arc::new(FailingSpawner));
        let result = tool.execute(serde_json::json!({ "task": "fail me" })).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no capacity"));
    }

    #[tokio::test]
    async fn test_spawn_tool_with_context_param() {
        let spawner = Arc::new(MockSpawner::new());
        let tool = SpawnTool::new().with_spawner(spawner);

        let result = tool
            .execute(serde_json::json!({
                "task": "summarize",
                "context": "Previous conversation about Rust"
            }))
            .await
            .unwrap();

        assert!(result.contains("Spawned sub-agent"));
    }
}
