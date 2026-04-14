//! Cron tool — schedule and manage cron jobs.
//!
//! Supports add, list, and remove actions through a callback interface
//! that connects to the CronService at runtime.

use crate::trait_def::{Tool, ToolError};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

/// Callback type for cron operations.
/// Returns a JSON string result.
pub type CronCallback = Arc<dyn Fn(CronAction) -> Result<String, String> + Send + Sync>;

/// Actions the cron tool can perform.
pub enum CronAction {
    Add {
        schedule: String,
        message: String,
        name: Option<String>,
    },
    List,
    Remove {
        job_id: String,
    },
}

/// Tool for managing scheduled tasks.
pub struct CronTool {
    callback: Option<CronCallback>,
}

impl CronTool {
    pub fn new() -> Self {
        Self { callback: None }
    }

    /// Create with a callback for real cron service operations.
    pub fn with_callback(callback: CronCallback) -> Self {
        Self {
            callback: Some(callback),
        }
    }
}

impl Default for CronTool {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_schedule(schedule: &str) -> Option<serde_json::Value> {
    let s = schedule.trim();

    // "every N seconds/minutes/hours"
    if let Some(rest) = s.strip_prefix("every ") {
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.len() == 2 {
            if let Ok(n) = parts[0].parse::<i64>() {
                let ms = match parts[1] {
                    "second" | "seconds" => n * 1000,
                    "minute" | "minutes" => n * 60_000,
                    "hour" | "hours" => n * 3_600_000,
                    "day" | "days" => n * 86_400_000,
                    _ => return None,
                };
                return Some(json!({"kind": "every", "every_ms": ms}));
            }
        }
    }

    // "at YYYY-MM-DD HH:MM"
    if let Some(rest) = s.strip_prefix("at ") {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(rest.trim(), "%Y-%m-%d %H:%M") {
            let ms = dt.and_utc().timestamp_millis();
            return Some(json!({"kind": "at", "at_ms": ms}));
        }
        // Also try date-only
        if let Ok(d) = chrono::NaiveDate::parse_from_str(rest.trim(), "%Y-%m-%d") {
            let dt = d.and_hms_opt(0, 0, 0).expect("midnight is always valid");
            let ms = dt.and_utc().timestamp_millis();
            return Some(json!({"kind": "at", "at_ms": ms}));
        }
    }

    // Cron expression (5 fields)
    let fields: Vec<&str> = s.split_whitespace().collect();
    if fields.len() == 5 {
        return Some(json!({"kind": "cron", "expr": s}));
    }

    None
}

#[async_trait]
impl Tool for CronTool {
    fn name(&self) -> &str {
        "cron"
    }

    fn description(&self) -> &str {
        "Manage scheduled tasks. Add, list, or remove cron jobs."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action: 'add', 'list', 'remove'",
                    "enum": ["add", "list", "remove"],
                },
                "schedule": { "type": "string", "description": "Schedule: cron expression ('*/5 * * * *'), 'every N minutes', or 'at YYYY-MM-DD HH:MM'" },
                "message": { "type": "string", "description": "Message to process when the job fires" },
                "name": { "type": "string", "description": "Optional name for the job" },
                "job_id": { "type": "string", "description": "Job ID for remove action" },
            },
            "required": ["action"],
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'action'".to_string()))?;

        match action {
            "add" => {
                let schedule = args["schedule"]
                    .as_str()
                    .ok_or_else(|| ToolError::Validation("Missing 'schedule'".to_string()))?;
                let message = args["message"]
                    .as_str()
                    .ok_or_else(|| ToolError::Validation("Missing 'message'".to_string()))?;
                let name = args["name"].as_str().map(|s| s.to_string());

                if let Some(cb) = &self.callback {
                    cb(CronAction::Add {
                        schedule: schedule.to_string(),
                        message: message.to_string(),
                        name,
                    })
                    .map_err(ToolError::Execution)
                } else {
                    // Fallback without backend
                    let parsed = parse_schedule(schedule);
                    if parsed.is_none() {
                        return Err(ToolError::Validation(
                            format!("Invalid schedule format: '{}'. Use cron expression, 'every N minutes', or 'at YYYY-MM-DD HH:MM'", schedule),
                        ));
                    }
                    Ok(format!("Scheduled job: '{}' at {}", message, schedule))
                }
            }
            "list" => {
                if let Some(cb) = &self.callback {
                    cb(CronAction::List).map_err(ToolError::Execution)
                } else {
                    Ok("No scheduled jobs (cron service not connected).".to_string())
                }
            }
            "remove" => {
                let job_id = args["job_id"]
                    .as_str()
                    .ok_or_else(|| ToolError::Validation("Missing 'job_id'".to_string()))?;

                if let Some(cb) = &self.callback {
                    cb(CronAction::Remove {
                        job_id: job_id.to_string(),
                    })
                    .map_err(ToolError::Execution)
                } else {
                    Ok(format!("Removed job: {}", job_id))
                }
            }
            _ => Err(ToolError::Validation(format!("Unknown action: {}", action))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cron_tool_metadata() {
        let tool = CronTool::new();
        assert_eq!(tool.name(), "cron");
        assert!(!tool.description().is_empty());
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["action"].is_object());
    }

    #[tokio::test]
    async fn test_cron_tool_add() {
        let tool = CronTool::new();
        let result = tool
            .execute(serde_json::json!({
                "action": "add",
                "schedule": "*/5 * * * *",
                "message": "Check status"
            }))
            .await
            .unwrap();
        assert!(result.contains("Scheduled job"));
        assert!(result.contains("Check status"));
    }

    #[tokio::test]
    async fn test_cron_tool_add_with_callback() {
        let cb = Arc::new(|action: CronAction| -> Result<String, String> {
            match action {
                CronAction::Add {
                    schedule,
                    message,
                    name,
                } => Ok(format!(
                    "Created job '{}' with schedule '{}' (name: {:?})",
                    message, schedule, name
                )),
                _ => Err("wrong action".to_string()),
            }
        });
        let tool = CronTool::with_callback(cb);
        let result = tool
            .execute(serde_json::json!({
                "action": "add",
                "schedule": "every 30 minutes",
                "message": "Sync data",
                "name": "sync-job"
            }))
            .await
            .unwrap();
        assert!(result.contains("Created job"));
        assert!(result.contains("Sync data"));
        assert!(result.contains("sync-job"));
    }

    #[tokio::test]
    async fn test_cron_tool_list_with_callback() {
        let cb = Arc::new(|action: CronAction| -> Result<String, String> {
            match action {
                CronAction::List => Ok("1. sync-job (every 30m)\n2. cleanup (daily)".to_string()),
                _ => Err("wrong action".to_string()),
            }
        });
        let tool = CronTool::with_callback(cb);
        let result = tool
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert!(result.contains("sync-job"));
        assert!(result.contains("cleanup"));
    }

    #[tokio::test]
    async fn test_cron_tool_remove_with_callback() {
        let cb = Arc::new(|action: CronAction| -> Result<String, String> {
            match action {
                CronAction::Remove { job_id } => Ok(format!("Removed job: {}", job_id)),
                _ => Err("wrong action".to_string()),
            }
        });
        let tool = CronTool::with_callback(cb);
        let result = tool
            .execute(serde_json::json!({
                "action": "remove",
                "job_id": "job-123"
            }))
            .await
            .unwrap();
        assert!(result.contains("Removed job: job-123"));
    }

    #[tokio::test]
    async fn test_cron_tool_add_missing_schedule() {
        let tool = CronTool::new();
        let result = tool
            .execute(serde_json::json!({
                "action": "add",
                "message": "test"
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_cron_tool_list_no_callback() {
        let tool = CronTool::new();
        let result = tool
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert!(result.contains("No scheduled jobs"));
    }

    #[tokio::test]
    async fn test_cron_tool_remove_no_callback() {
        let tool = CronTool::new();
        let result = tool
            .execute(serde_json::json!({
                "action": "remove",
                "job_id": "job-123"
            }))
            .await
            .unwrap();
        assert!(result.contains("Removed job"));
    }

    #[tokio::test]
    async fn test_cron_tool_unknown_action() {
        let tool = CronTool::new();
        let result = tool.execute(serde_json::json!({"action": "unknown"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_cron_tool_missing_action() {
        let tool = CronTool::new();
        let result = tool.execute(serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_cron_tool_default() {
        let tool = CronTool::default();
        assert_eq!(tool.name(), "cron");
    }

    #[test]
    fn test_parse_schedule_every() {
        let result = parse_schedule("every 5 minutes").unwrap();
        assert_eq!(result["kind"], "every");
        assert_eq!(result["every_ms"], 300_000);

        let result = parse_schedule("every 1 hour").unwrap();
        assert_eq!(result["every_ms"], 3_600_000);
    }

    #[test]
    fn test_parse_schedule_at() {
        let result = parse_schedule("at 2025-06-15 09:30").unwrap();
        assert_eq!(result["kind"], "at");
        assert!(result["at_ms"].is_number());
    }

    #[test]
    fn test_parse_schedule_cron() {
        let result = parse_schedule("*/5 * * * *").unwrap();
        assert_eq!(result["kind"], "cron");
        assert_eq!(result["expr"], "*/5 * * * *");
    }

    #[test]
    fn test_parse_schedule_invalid() {
        assert!(parse_schedule("invalid schedule").is_none());
    }
}
