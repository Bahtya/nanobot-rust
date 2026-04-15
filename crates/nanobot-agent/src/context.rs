//! Context builder — assembles the system prompt for the agent.
//!
//! Builds the system prompt from identity files, memory, skills, runtime metadata.
//! Uses [`PromptAssembler`] from nanobot-learning to combine [`PromptSection`]
//! variants into the final prompt string.

use crate::notes::NotesManager;
use anyhow::Result;
use nanobot_bus::events::InboundMessage;
use nanobot_config::Config;
use nanobot_learning::prompt::{PromptAssembler, PromptSection};
use nanobot_session::Session;
use nanobot_tools::ToolRegistry;

/// Builds the system prompt context for an agent invocation.
pub struct ContextBuilder<'a> {
    config: &'a Config,
    /// Optional skill prompt sections to inject (matched externally).
    skill_sections: Option<String>,
    /// Assembler for combining prompt sections. Uses default when not set.
    prompt_assembler: Option<PromptAssembler>,
}

impl<'a> ContextBuilder<'a> {
    /// Create a new context builder for the given config.
    pub fn new(config: &'a Config) -> Self {
        Self {
            config,
            skill_sections: None,
            prompt_assembler: None,
        }
    }

    /// Attach matched skill prompt sections for injection into the system prompt.
    pub fn with_skills(mut self, sections: String) -> Self {
        self.skill_sections = Some(sections);
        self
    }

    /// Attach a [`PromptAssembler`] for assembling the system prompt.
    ///
    /// When set, the assembler controls how sections are joined and separated.
    /// When not set, a default assembler is used (double-newline separator).
    pub fn with_prompt_assembler(mut self, assembler: PromptAssembler) -> Self {
        self.prompt_assembler = Some(assembler);
        self
    }

    /// Build the complete system prompt.
    ///
    /// Collects prompt sections (identity, runtime, memory, notes, skills, tools,
    /// custom instructions) as [`PromptSection`] variants and assembles them using
    /// the configured [`PromptAssembler`].
    pub fn build_system_prompt(
        &self,
        msg: &InboundMessage,
        session: &Session,
        tool_registry: &ToolRegistry,
        recalled_memory: Option<&str>,
    ) -> Result<String> {
        let mut sections: Vec<PromptSection> = Vec::new();

        // Identity section
        sections.push(PromptSection::System {
            content: self.build_identity_content(),
        });

        // Runtime metadata
        sections.push(PromptSection::Custom {
            label: "Runtime".to_string(),
            content: self.build_runtime_content(msg),
        });

        // Recalled memories from the memory store (takes precedence)
        if let Some(memory_ctx) = recalled_memory {
            if !memory_ctx.is_empty() {
                sections.push(PromptSection::Memory {
                    content: memory_ctx.to_string(),
                });
            }
        } else if !session.messages.is_empty() {
            // Fallback: generic memory hint for continuing conversations
            sections.push(PromptSection::Memory {
                content: self.build_memory_hint_content(),
            });
        }

        // Structured notes (prefer structured format with categories)
        if let Some(notes_ctx) = NotesManager::format_structured_context(session) {
            sections.push(PromptSection::Custom {
                label: "Notes".to_string(),
                content: notes_ctx,
            });
        } else if let Some(notes_ctx) = session.format_notes_context() {
            sections.push(PromptSection::Custom {
                label: "Notes".to_string(),
                content: notes_ctx,
            });
        }

        // Skills section (from SkillRegistry matching)
        if let Some(ref skill_sections) = self.skill_sections {
            if !skill_sections.is_empty() {
                sections.push(PromptSection::Skills {
                    content: skill_sections.clone(),
                });
            }
        }

        // Tools section
        let tools = tool_registry.tool_names();
        if !tools.is_empty() {
            sections.push(PromptSection::Custom {
                label: "Available Tools".to_string(),
                content: format!(
                    "You have access to the following tools: {}",
                    tools.join(", ")
                ),
            });
        }

        // Custom instructions
        if let Some(custom) = &self.config.custom_instructions {
            if !custom.is_empty() {
                sections.push(PromptSection::Custom {
                    label: "Additional Instructions".to_string(),
                    content: custom.clone(),
                });
            }
        }

        let default_assembler = PromptAssembler::new();
        let assembler = self.prompt_assembler.as_ref().unwrap_or(&default_assembler);
        Ok(assembler.assemble(&sections))
    }

    /// Build the identity content (agent name and description).
    fn build_identity_content(&self) -> String {
        let name = self.config.name.as_deref().unwrap_or("Nanobot");
        format!(
            "You are {}, an AI assistant powered by nanobot. \
             You help users accomplish tasks using the tools available to you. \
             Be concise, helpful, and accurate.",
            name
        )
    }

    /// Build the runtime metadata content (time, platform, chat ID).
    fn build_runtime_content(&self, msg: &InboundMessage) -> String {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z");
        format!(
            "- Current time: {}\n- Platform: {}\n- Chat ID: {}",
            now, msg.channel, msg.chat_id,
        )
    }

    /// Build the memory hint content for continuing conversations.
    fn build_memory_hint_content(&self) -> String {
        "This is a continuing conversation. Use the message history to maintain context."
            .to_string()
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
        // Empty session → no memory section
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
        let identity = builder.build_identity_content();
        assert!(identity.contains("Nanobot"));
        assert!(identity.contains("AI assistant"));
    }

    #[test]
    fn test_build_identity_custom_name() {
        let mut config = Config::default();
        config.name = Some("RoboAssistant".to_string());
        let builder = ContextBuilder::new(&config);
        let identity = builder.build_identity_content();
        assert!(identity.contains("RoboAssistant"));
    }

    #[test]
    fn test_build_runtime_content() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let runtime = builder.build_runtime_content(&msg);
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
    fn test_build_system_prompt_with_skills() {
        let config = Config::default();
        let skill_section = "\n### deploy-k8s\n**Steps:**\n1. Apply manifests\n**Pitfalls:**\n- Do not deploy on Fridays".to_string();
        let builder = ContextBuilder::new(&config).with_skills(skill_section);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder.build_system_prompt(&msg, &session, &tools, None).unwrap();
        assert!(prompt.contains("## Skills"));
        assert!(prompt.contains("deploy-k8s"));
        assert!(prompt.contains("Apply manifests"));
        assert!(prompt.contains("Do not deploy on Fridays"));
    }

    #[test]
    fn test_build_system_prompt_with_empty_skills() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config).with_skills(String::new());
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder.build_system_prompt(&msg, &session, &tools, None).unwrap();
        // Empty skill section should not appear
        assert!(!prompt.contains("## Skills"));
    }

    #[test]
    fn test_build_system_prompt_without_skills() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder.build_system_prompt(&msg, &session, &tools, None).unwrap();
        // No skill section injected
        assert!(!prompt.contains("## Skills"));
    }

    #[test]
    fn test_build_system_prompt_with_recalled_memory() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let recalled = "- User prefers Rust\n- Project uses Tokio";
        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, Some(recalled))
            .unwrap();
        assert!(prompt.contains("## Memory"));
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
        assert!(!prompt.contains("## Memory"));
    }

    // ── PromptAssembler integration tests ─────────────────────────

    #[test]
    fn test_context_builder_with_custom_assembler() {
        let config = Config::default();
        let assembler = PromptAssembler::with_separator("\n---\n");
        let builder = ContextBuilder::new(&config).with_prompt_assembler(assembler);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
        // Custom separator should be used between sections
        assert!(prompt.contains("\n---\n"));
    }

    #[test]
    fn test_context_builder_uses_section_headers() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let mut session = Session::new("test:key".to_string());
        session.add_user_message("history".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
        // PromptAssembler adds ## headers for each section
        assert!(prompt.contains("## System"));
        assert!(prompt.contains("## Runtime"));
        assert!(prompt.contains("## Memory"));
    }

    #[test]
    fn test_context_builder_prompt_section_order() {
        let mut config = Config::default();
        config.custom_instructions = Some("Be helpful.".to_string());
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
        // Sections should appear in order: System, Runtime, ..., Additional Instructions
        let system_pos = prompt.find("## System").unwrap();
        let runtime_pos = prompt.find("## Runtime").unwrap();
        let instructions_pos = prompt.find("## Additional Instructions").unwrap();
        assert!(system_pos < runtime_pos);
        assert!(runtime_pos < instructions_pos);
    }

    #[test]
    fn test_context_builder_assembler_default_when_not_set() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config);
        assert!(builder.prompt_assembler.is_none());
        // build_system_prompt should still work with default assembler
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
        // Default assembler uses double newline separator
        assert!(prompt.contains("\n\n"));
        assert!(prompt.contains("## System"));
    }
}
