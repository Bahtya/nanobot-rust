//! Subagent manager — background task execution.
//!
//! Mirrors the Python `agent/subagent.py` SubagentManager.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};
use uuid::Uuid;

/// Status of a subagent task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Completed(String),
    Failed(String),
}

/// A background subagent task.
#[derive(Debug)]
struct SubagentTask {
    id: String,
    name: String,
    #[allow(dead_code)]
    description: String,
    status: TaskStatus,
}

/// Manages background subagent tasks.
pub struct SubagentManager {
    tasks: Arc<RwLock<HashMap<String, SubagentTask>>>,
}

impl SubagentManager {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Spawn a new background task.
    pub async fn spawn(&self, name: &str, description: &str) -> String {
        let id = Uuid::new_v4().to_string();
        let task = SubagentTask {
            id: id.clone(),
            name: name.to_string(),
            description: description.to_string(),
            status: TaskStatus::Running,
        };

        self.tasks.write().await.insert(id.clone(), task);
        info!("Spawned subagent task: {} ({})", name, id);
        id
    }

    /// Complete a task with a result.
    pub async fn complete(&self, id: &str, result: String) {
        let mut tasks = self.tasks.write().await;
        if let Some(task) = tasks.get_mut(id) {
            task.status = TaskStatus::Completed(result);
            debug!("Completed subagent task: {}", id);
        }
    }

    /// Mark a task as failed.
    pub async fn fail(&self, id: &str, error: String) {
        let mut tasks = self.tasks.write().await;
        if let Some(task) = tasks.get_mut(id) {
            task.status = TaskStatus::Failed(error);
            debug!("Failed subagent task: {}", id);
        }
    }

    /// Get the status of a task.
    pub async fn get_status(&self, id: &str) -> Option<TaskStatus> {
        let tasks = self.tasks.read().await;
        tasks.get(id).map(|t| t.status.clone())
    }

    /// List all active tasks.
    pub async fn list_tasks(&self) -> Vec<(String, String, TaskStatus)> {
        let tasks = self.tasks.read().await;
        tasks
            .values()
            .map(|t| (t.id.clone(), t.name.clone(), t.status.clone()))
            .collect()
    }
}

impl Default for SubagentManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_subagent_manager_new() {
        let mgr = SubagentManager::new();
        let tasks = mgr.list_tasks().await;
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn test_subagent_manager_spawn() {
        let mgr = SubagentManager::new();
        let id = mgr.spawn("test_task", "a test").await;
        let status = mgr.get_status(&id).await.unwrap();
        assert_eq!(status, TaskStatus::Running);
    }

    #[tokio::test]
    async fn test_subagent_manager_complete() {
        let mgr = SubagentManager::new();
        let id = mgr.spawn("test_task", "a test").await;
        mgr.complete(&id, "done result".to_string()).await;
        let status = mgr.get_status(&id).await.unwrap();
        assert_eq!(status, TaskStatus::Completed("done result".to_string()));
    }

    #[tokio::test]
    async fn test_subagent_manager_fail() {
        let mgr = SubagentManager::new();
        let id = mgr.spawn("test_task", "a test").await;
        mgr.fail(&id, "error occurred".to_string()).await;
        let status = mgr.get_status(&id).await.unwrap();
        assert_eq!(status, TaskStatus::Failed("error occurred".to_string()));
    }

    #[tokio::test]
    async fn test_subagent_manager_list_tasks() {
        let mgr = SubagentManager::new();
        mgr.spawn("task1", "first").await;
        mgr.spawn("task2", "second").await;
        mgr.spawn("task3", "third").await;
        let tasks = mgr.list_tasks().await;
        assert_eq!(tasks.len(), 3);
        let names: Vec<&str> = tasks.iter().map(|(_, name, _)| name.as_str()).collect();
        assert!(names.contains(&"task1"));
        assert!(names.contains(&"task2"));
        assert!(names.contains(&"task3"));
    }

    #[tokio::test]
    async fn test_subagent_manager_default() {
        let mgr = SubagentManager::default();
        let tasks = mgr.list_tasks().await;
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn test_subagent_manager_get_status_nonexistent() {
        let mgr = SubagentManager::new();
        let status = mgr.get_status("nonexistent-id").await;
        assert!(status.is_none());
    }
}
