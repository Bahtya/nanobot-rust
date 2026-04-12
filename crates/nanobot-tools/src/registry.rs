//! Tool registry — dynamic tool registration and dispatch.

use crate::trait_def::{Tool, ToolError};
use nanobot_core::FunctionDefinition;
use parking_lot::RwLock;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;

/// Central registry for agent tools.
#[derive(Clone)]
pub struct ToolRegistry {
    tools: Arc<RwLock<HashMap<String, Arc<dyn Tool>>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn register(&self, tool: impl Tool + 'static) {
        let name = tool.name().to_string();
        debug!("Registering tool: {}", name);
        self.tools.write().insert(name, Arc::new(tool));
    }

    pub fn unregister(&self, name: &str) {
        self.tools.write().remove(name);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.read().get(name).cloned()
    }

    pub async fn execute(&self, name: &str, args: Value) -> Result<String, ToolError> {
        let tool = self
            .get(name)
            .ok_or_else(|| ToolError::NotAvailable(format!("Tool not found: {}", name)))?;

        if !tool.is_available() {
            return Err(ToolError::NotAvailable(format!(
                "Tool '{}' is not available",
                name
            )));
        }

        tool.execute(args).await
    }

    pub fn get_definitions(&self) -> Vec<FunctionDefinition> {
        let tools = self.tools.read();
        tools
            .values()
            .filter(|t| t.is_available())
            .map(|t| t.to_function_definition())
            .collect()
    }

    pub fn get_definitions_for_toolset(&self, toolset: &str) -> Vec<FunctionDefinition> {
        let tools = self.tools.read();
        tools
            .values()
            .filter(|t| t.toolset() == toolset && t.is_available())
            .map(|t| t.to_function_definition())
            .collect()
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.tools.read().keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.tools.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.read().is_empty()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct MockTool {
        tool_name: &'static str,
        available: bool,
    }

    impl MockTool {
        fn new(name: &'static str) -> Self {
            Self {
                tool_name: name,
                available: true,
            }
        }
    }

    #[async_trait]
    impl Tool for MockTool {
        fn name(&self) -> &str {
            self.tool_name
        }
        fn description(&self) -> &str {
            "mock tool"
        }
        fn parameters_schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn is_available(&self) -> bool {
            self.available
        }
        async fn execute(&self, _args: Value) -> Result<String, ToolError> {
            Ok("mock result".to_string())
        }
    }

    #[test]
    fn test_registry_new() {
        let registry = ToolRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn test_registry_register_and_get() {
        let registry = ToolRegistry::new();
        registry.register(MockTool::new("test_tool"));

        let tool = registry.get("test_tool");
        assert!(tool.is_some());
        assert_eq!(tool.unwrap().name(), "test_tool");

        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_registry_unregister() {
        let registry = ToolRegistry::new();
        registry.register(MockTool::new("tool_a"));
        assert!(registry.get("tool_a").is_some());

        registry.unregister("tool_a");
        assert!(registry.get("tool_a").is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn test_registry_get_definitions() {
        let registry = ToolRegistry::new();
        registry.register(MockTool::new("tool_a"));
        registry.register(MockTool::new("tool_b"));

        let defs = registry.get_definitions();
        assert_eq!(defs.len(), 2);

        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"tool_a"));
        assert!(names.contains(&"tool_b"));
    }

    #[test]
    fn test_registry_tool_names() {
        let registry = ToolRegistry::new();
        registry.register(MockTool::new("alpha"));
        registry.register(MockTool::new("beta"));

        let mut names = registry.tool_names();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn test_registry_default() {
        let registry = ToolRegistry::default();
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn test_registry_execute_tool() {
        let registry = ToolRegistry::new();
        registry.register(MockTool::new("my_tool"));

        let result = registry.execute("my_tool", serde_json::json!({})).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "mock result");
    }

    #[tokio::test]
    async fn test_registry_execute_tool_not_found() {
        let registry = ToolRegistry::new();
        let result = registry.execute("nonexistent", serde_json::json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ToolError::NotAvailable(_)));
        assert!(err.to_string().contains("Tool not found"));
    }

    #[tokio::test]
    async fn test_registry_execute_unavailable_tool() {
        struct UnavailableTool;
        #[async_trait]
        impl Tool for UnavailableTool {
            fn name(&self) -> &str {
                "unavail"
            }
            fn description(&self) -> &str {
                "unavailable"
            }
            fn parameters_schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            fn is_available(&self) -> bool {
                false
            }
            async fn execute(&self, _args: Value) -> Result<String, ToolError> {
                Ok("should not reach".to_string())
            }
        }

        let registry = ToolRegistry::new();
        registry.register(UnavailableTool);

        let result = registry.execute("unavail", serde_json::json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not available"));
    }

    #[test]
    fn test_registry_get_definitions_for_toolset() {
        struct ToolsetTool {
            name: &'static str,
            toolset: &'static str,
        }
        #[async_trait]
        impl Tool for ToolsetTool {
            fn name(&self) -> &str {
                self.name
            }
            fn description(&self) -> &str {
                "test"
            }
            fn parameters_schema(&self) -> Value {
                serde_json::json!({"type": "object"})
            }
            fn toolset(&self) -> &str {
                self.toolset
            }
            async fn execute(&self, _args: Value) -> Result<String, ToolError> {
                Ok("ok".into())
            }
        }

        let registry = ToolRegistry::new();
        registry.register(ToolsetTool {
            name: "a",
            toolset: "web",
        });
        registry.register(ToolsetTool {
            name: "b",
            toolset: "default",
        });
        registry.register(ToolsetTool {
            name: "c",
            toolset: "web",
        });

        let web_defs = registry.get_definitions_for_toolset("web");
        assert_eq!(web_defs.len(), 2);
        let names: Vec<&str> = web_defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"c"));
        assert!(!names.contains(&"b"));

        let default_defs = registry.get_definitions_for_toolset("default");
        assert_eq!(default_defs.len(), 1);
        assert_eq!(default_defs[0].name, "b");

        let empty_defs = registry.get_definitions_for_toolset("nonexistent");
        assert!(empty_defs.is_empty());
    }

    #[test]
    fn test_registry_len() {
        let registry = ToolRegistry::new();
        assert_eq!(registry.len(), 0);
        registry.register(MockTool::new("a"));
        assert_eq!(registry.len(), 1);
        registry.register(MockTool::new("b"));
        assert_eq!(registry.len(), 2);
        registry.unregister("a");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn test_registry_unregister_nonexistent() {
        let registry = ToolRegistry::new();
        registry.register(MockTool::new("x"));
        // Unregistering nonexistent should not panic
        registry.unregister("nonexistent");
        assert_eq!(registry.len(), 1);
    }
}
