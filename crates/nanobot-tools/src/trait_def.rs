//! Tool trait definition.

use async_trait::async_trait;
use nanobot_core::FunctionDefinition;
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
    fn test_tool_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ToolError>();
    }
}
