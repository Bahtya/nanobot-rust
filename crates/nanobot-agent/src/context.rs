//! Context builder — assembles the system prompt for the agent.
//!
//! Builds the system prompt from identity files, memory, skills, runtime metadata.
//! Mirrors the Python `agent/context.py` ContextBuilder.

use crate::notes::NotesManager;
use anyhow::Result;
use nanobot_bus::events::InboundMessage;
use nanobot_config::Config;
use nanobot_session::Session;
use nanobot_tools::ToolRegistry;

/// Builds the system prompt context for an agent invocation.
pub struct ContextBuilder<'a> {
    config: &'a Config,
}

impl<'a> ContextBuilder<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self { config }
    }

    /// Build the complete system prompt.
    pub fn build_system_prompt(
        &self,
        msg: &InboundMessage,
        session: &Session,
        tool_registry: &ToolRegistry,
        recalled_memory: Option<&str>,
    ) -> Result<String> {
        let mut parts = Vec::new();

        // Identity section
        parts.push(self.build_identity());

        // Runtime metadata
        parts.push(self.build_runtime_metadata(msg));

        // Recalled memories from the memory store (takes precedence)
        if let Some(memory_ctx) = recalled_memory {
            if !memory_ctx.is_empty() {
                parts.push(memory_ctx.to_string());
            }
        } else if !session.messages.is_empty() {
            // Fallback: generic memory hint for continuing conversations
            parts.push(self.build_memory_hint());
        }

        // Structured notes (prefer structured format with categories)
        if let Some(notes_ctx) = NotesManager::format_structured_context(session) {
            parts.push(notes_ctx);
        } else if let Some(notes_ctx) = session.format_notes_context() {
            parts.push(notes_ctx);
        }

        // Skills section
        let tools = tool_registry.tool_names();
        if !tools.is_empty() {
            parts.push(format!(
                "## Available Tools\n\nYou have access to the following tools: {}",
                tools.join(", ")
            ));
        }

        // Custom instructions
        if let Some(custom) = &self.config.custom_instructions {
            if !custom.is_empty() {
                parts.push(format!("## Additional Instructions\n\n{}", custom));
            }
        }

        Ok(parts.join("\n\n"))
    }

    fn build_identity(&self) -> String {
        let name = self.config.name.as_deref().unwrap_or("Nanobot");
        format!(
            "You are {}, an AI assistant powered by nanobot. \
             You help users accomplish tasks using the tools available to you. \
             Be concise, helpful, and accurate.",
            name
        )
    }

    fn build_runtime_metadata(&self, msg: &InboundMessage) -> String {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");
        format!(
            "## Runtime\n\n\
             - Current time: {}\n\
             - Platform: {}\n\
             - Chat ID: {}",
            now, msg.channel, msg.chat_id,
        )
    }

    fn build_memory_hint(&self) -> String {
        "## Memory\n\nThis is a continuing conversation. Use the message history to maintain context.".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanobot_bus::events::InboundMessage;
    use nanobot_config::Config;
    use nanobot_core::{MessageType, Platform};
    use nanobot_session::Session;
    use nanobot_tools::ToolRegistry;

    fn make_inbound() -> InboundMessage {
        InboundMessage {
            channel: Platform::Telegram,
            sender_id: "user1".to_string(),
            chat_id: "chat1".to_string(),
            content: "hello".to_string(),
            media: vec![],
            metadata: Default::default(),
            source: None,
            message_type: MessageType::Text,
            message_id: None,
            reply_to: None,
            timestamp: chrono::Local::now(),
        }
    }

    #[test]
    fn test_build_system_prompt_basic() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();

        // Should contain identity section
        assert!(prompt.contains("Nanobot"));
        // Should contain runtime section with platform
        assert!(prompt.contains("telegram"));
        assert!(prompt.contains("chat1"));
        // Empty session → no memory hint
        assert!(!prompt.contains("## Memory"));
        // No tools → no tools section
        assert!(!prompt.contains("## Available Tools"));
    }

    #[test]
    fn test_build_system_prompt_with_session_history() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let mut session = Session::new("test:key".to_string());
        session.add_user_message("previous message".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
        assert!(prompt.contains("## Memory"));
        assert!(prompt.contains("continuing conversation"));
    }

    #[test]
    fn test_build_system_prompt_with_tools() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());

        let tools = ToolRegistry::new();
        use async_trait::async_trait;
        use nanobot_tools::Tool;
        use nanobot_tools::ToolError;

        struct DummyTool;
        #[async_trait]
        impl Tool for DummyTool {
            fn name(&self) -> &str {
                "dummy_tool"
            }
            fn description(&self) -> &str {
                "A test tool"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _args: serde_json::Value) -> Result<String, ToolError> {
                Ok("ok".to_string())
            }
        }
        tools.register(DummyTool);

        let prompt = builder.build_system_prompt(&msg, &session, &tools, None).unwrap();
        assert!(prompt.contains("## Available Tools"));
        assert!(prompt.contains("dummy_tool"));
    }

    #[test]
    fn test_build_system_prompt_custom_name() {
        let mut config = Config::default();
        config.name = Some("CustomBot".to_string());
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder.build_system_prompt(&msg, &session, &tools, None).unwrap();
        assert!(prompt.contains("CustomBot"));
    }

    #[test]
    fn test_build_system_prompt_custom_instructions() {
        let mut config = Config::default();
        config.custom_instructions = Some("Always respond in French.".to_string());
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder.build_system_prompt(&msg, &session, &tools, None).unwrap();
        assert!(prompt.contains("## Additional Instructions"));
        assert!(prompt.contains("Always respond in French"));
    }

    #[test]
    fn test_build_identity_default() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config);
        let identity = builder.build_identity();
        assert!(identity.contains("Nanobot"));
        assert!(identity.contains("AI assistant"));
    }

    #[test]
    fn test_build_identity_custom_name() {
        let mut config = Config::default();
        config.name = Some("RoboAssistant".to_string());
        let builder = ContextBuilder::new(&config);
        let identity = builder.build_identity();
        assert!(identity.contains("RoboAssistant"));
    }

    #[test]
    fn test_build_runtime_metadata() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let runtime = builder.build_runtime_metadata(&msg);
        assert!(runtime.contains("## Runtime"));
        assert!(runtime.contains("telegram"));
        assert!(runtime.contains("chat1"));
        assert!(runtime.contains("Current time"));
    }

    #[test]
    fn test_build_system_prompt_all_sections() {
        let mut config = Config::default();
        config.name = Some("TestBot".to_string());
        config.custom_instructions = Some("Be helpful.".to_string());

        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let mut session = Session::new("test:key".to_string());
        session.add_user_message("history".to_string());

        let tools = ToolRegistry::new();
        use async_trait::async_trait;
        use nanobot_tools::Tool;
        use nanobot_tools::ToolError;

        struct AnotherTool;
        #[async_trait]
        impl Tool for AnotherTool {
            fn name(&self) -> &str {
                "my_tool"
            }
            fn description(&self) -> &str {
                "tool"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _args: serde_json::Value) -> Result<String, ToolError> {
                Ok("ok".to_string())
            }
        }
        tools.register(AnotherTool);

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
        assert!(prompt.contains("TestBot"));
        assert!(prompt.contains("## Runtime"));
        assert!(prompt.contains("## Memory"));
        assert!(prompt.contains("## Available Tools"));
        assert!(prompt.contains("my_tool"));
        assert!(prompt.contains("## Additional Instructions"));
    }

    #[test]
    fn test_build_system_prompt_with_recalled_memory() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let recalled = "## Recalled Memories\n\n- User prefers Rust\n- Project uses Tokio";
        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, Some(recalled))
            .unwrap();
        assert!(prompt.contains("## Recalled Memories"));
        assert!(prompt.contains("User prefers Rust"));
        // Should NOT contain the generic memory hint since recalled memory is present
        assert!(!prompt.contains("continuing conversation"));
    }

    #[test]
    fn test_build_system_prompt_empty_recalled_memory_ignored() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, Some(""))
            .unwrap();
        // Empty recalled memory should not add a section
        assert!(!prompt.contains("## Recalled"));
    }
}
