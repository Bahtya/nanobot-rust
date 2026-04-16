//! Context builder — assembles the system prompt for the agent.
//!
//! Builds the system prompt from identity files, memory, skills, runtime metadata.
//! Uses [`PromptAssembler`] from nanobot-learning to combine [`PromptSection`]
//! variants into the final prompt string.

use crate::notes::NotesManager;
use anyhow::Result;
use nanobot_bus::events::InboundMessage;
use nanobot_config::Config;
use nanobot_learning::prompt::{PromptAssembler, PromptSection, ToolInfo};
use nanobot_session::Session;
use nanobot_tools::ToolRegistry;

const DEFAULT_TOOL_GUIDANCE_TOKEN_BUDGET: usize = 2000;
const DEFAULT_SKILL_INDEX_MAX_ENTRIES: usize = 10;

/// Builds the system prompt context for an agent invocation.
pub struct ContextBuilder<'a> {
    config: &'a Config,
    /// Optional skill prompt sections to inject (matched externally).
    skill_sections: Option<String>,
    /// Optional skill index entries for listing available skills.
    skill_index_entries: Option<Vec<nanobot_learning::prompt::SkillIndexEntry>>,
    /// Optional prompt adjustment content learned from prior turns.
    prompt_adjustment: Option<String>,
    /// Assembler for combining prompt sections. Uses default when not set.
    prompt_assembler: Option<PromptAssembler>,
    /// Approximate token budget for rendered tool guidance.
    tool_guidance_token_budget: usize,
}

impl<'a> ContextBuilder<'a> {
    /// Create a new context builder for the given config.
    pub fn new(config: &'a Config) -> Self {
        Self {
            config,
            skill_sections: None,
            skill_index_entries: None,
            prompt_adjustment: None,
            prompt_assembler: None,
            tool_guidance_token_budget: DEFAULT_TOOL_GUIDANCE_TOKEN_BUDGET,
        }
    }

    /// Attach matched skill prompt sections for injection into the system prompt.
    pub fn with_skills(mut self, sections: String) -> Self {
        self.skill_sections = Some(sections);
        self
    }

    /// Attach skill index entries for listing available skills with metadata.
    ///
    /// Each entry provides the skill name, description, category, and triggers,
    /// allowing the agent to know what skills are available even when they are
    /// not directly matched to the current message.
    pub fn with_skill_index(
        mut self,
        entries: Vec<nanobot_learning::prompt::SkillIndexEntry>,
    ) -> Self {
        self.skill_index_entries = Some(entries);
        self
    }

    /// Attach prompt adjustment guidance learned from previous turns.
    pub fn with_prompt_adjustment(mut self, adjustment: String) -> Self {
        self.prompt_adjustment = Some(adjustment);
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

    /// Set the approximate token budget for rendered tool guidance.
    ///
    /// Tool schemas are truncated per tool when the rendered guidance exceeds
    /// this budget, using a simple `len / 4` token estimate.
    pub fn with_tool_guidance_token_budget(mut self, budget: usize) -> Self {
        self.tool_guidance_token_budget = budget;
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

        // Learned prompt adjustments from prior turns.
        if let Some(ref adjustment) = self.prompt_adjustment {
            if !adjustment.is_empty() {
                sections.push(PromptSection::Custom {
                    label: "Learning Adjustment".to_string(),
                    content: adjustment.clone(),
                });
            }
        }

        // Tools section — enriched tool guidance with descriptions and parameters
        let tool_defs = tool_registry.get_definitions();
        if !tool_defs.is_empty() {
            let tool_infos: Vec<ToolInfo> = tool_defs
                .iter()
                .map(|def| {
                    let schema_str = def
                        .parameters
                        .as_ref()
                        .map(|p| serde_json::to_string(p).unwrap_or_default())
                        .unwrap_or_default();
                    ToolInfo {
                        name: def.name.clone(),
                        description: def.description.clone().unwrap_or_default(),
                        parameters_schema: schema_str,
                    }
                })
                .collect();
            let guidance = PromptAssembler::build_tool_guidance_with_budget(
                &tool_infos,
                self.tool_guidance_token_budget,
            );
            sections.push(PromptSection::ToolGuidance { content: guidance });
        }

        // Memory fence — structured recall triggers based on known categories
        let memory_fence_content =
            PromptAssembler::build_memory_fence(&Self::default_memory_fences());
        if !memory_fence_content.is_empty() {
            sections.push(PromptSection::MemoryFence {
                content: memory_fence_content,
            });
        }

        // Skill index — list of all available skills with metadata
        if let Some(ref entries) = self.skill_index_entries {
            if !entries.is_empty() {
                let index =
                    PromptAssembler::build_skill_index(entries, DEFAULT_SKILL_INDEX_MAX_ENTRIES);
                sections.push(PromptSection::SkillIndex { content: index });
            }
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

    /// Return the default memory fence entries for structured recall triggers.
    ///
    /// These fences guide the agent on when to consider recalling specific
    /// categories of memories from the store.
    fn default_memory_fences() -> Vec<nanobot_learning::prompt::MemoryFenceEntry> {
        vec![
            nanobot_learning::prompt::MemoryFenceEntry {
                category: "user_profile".to_string(),
                hint: "When personalizing responses or addressing the user".to_string(),
            },
            nanobot_learning::prompt::MemoryFenceEntry {
                category: "environment".to_string(),
                hint: "When discussing project setup, tools, or infrastructure".to_string(),
            },
            nanobot_learning::prompt::MemoryFenceEntry {
                category: "preference".to_string(),
                hint: "When choosing between approaches or making style decisions".to_string(),
            },
            nanobot_learning::prompt::MemoryFenceEntry {
                category: "error_lesson".to_string(),
                hint: "When encountering errors or debugging issues".to_string(),
            },
            nanobot_learning::prompt::MemoryFenceEntry {
                category: "project_convention".to_string(),
                hint: "When writing code, configuring tools, or making architecture decisions"
                    .to_string(),
            },
        ]
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
        // Empty session → no memory section (but Memory Fence is present)
        assert!(!prompt.contains("## Memory\n"));
        // No tools → no tool guidance section
        assert!(!prompt.contains("## Tool Guidance"));
        // Memory fence is always present (from default fences)
        assert!(prompt.contains("## Memory Fence"));
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

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
        assert!(prompt.contains("## Tool Guidance"));
        assert!(prompt.contains("### dummy_tool"));
        assert!(prompt.contains("A test tool"));
    }

    #[test]
    fn test_build_system_prompt_custom_name() {
        let config = Config {
            name: Some("CustomBot".to_string()),
            ..Config::default()
        };
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
        assert!(prompt.contains("CustomBot"));
    }

    #[test]
    fn test_build_system_prompt_custom_instructions() {
        let config = Config {
            custom_instructions: Some("Always respond in French.".to_string()),
            ..Config::default()
        };
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
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
        let config = Config {
            name: Some("RoboAssistant".to_string()),
            ..Config::default()
        };
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
        let config = Config {
            name: Some("TestBot".to_string()),
            custom_instructions: Some("Be helpful.".to_string()),
            ..Config::default()
        };

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
        assert!(prompt.contains("## Tool Guidance"));
        assert!(prompt.contains("### my_tool"));
        assert!(prompt.contains("## Memory Fence"));
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

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
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

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
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

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
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
        // Empty recalled memory should not add a Memory section (but Memory Fence is present)
        assert!(!prompt.contains("## Memory\n"));
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
        let config = Config {
            custom_instructions: Some("Be helpful.".to_string()),
            ..Config::default()
        };
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

    // ── Enriched context section tests ──────────────────────────────

    #[test]
    fn test_tool_guidance_with_description_and_params() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        use async_trait::async_trait;
        use nanobot_tools::Tool;
        use nanobot_tools::ToolError;

        struct RichTool;
        #[async_trait]
        impl Tool for RichTool {
            fn name(&self) -> &str {
                "search"
            }
            fn description(&self) -> &str {
                "Search the codebase for patterns"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regex pattern" },
                        "path": { "type": "string", "description": "Directory to search" }
                    },
                    "required": ["pattern"]
                })
            }
            async fn execute(&self, _args: serde_json::Value) -> Result<String, ToolError> {
                Ok("ok".to_string())
            }
        }
        tools.register(RichTool);

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
        assert!(prompt.contains("## Tool Guidance"));
        assert!(prompt.contains("### search"));
        assert!(prompt.contains("Search the codebase for patterns"));
        assert!(prompt.contains("Parameters:"));
    }

    #[test]
    fn test_memory_fence_includes_categories() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
        assert!(prompt.contains("## Memory Fence"));
        assert!(prompt.contains("**user_profile**:"));
        assert!(prompt.contains("**environment**:"));
        assert!(prompt.contains("**preference**:"));
        assert!(prompt.contains("**error_lesson**:"));
        assert!(prompt.contains("**project_convention**:"));
    }

    #[test]
    fn test_skill_index_with_entries() {
        let config = Config::default();
        let entries = vec![
            nanobot_learning::prompt::SkillIndexEntry {
                name: "deploy-k8s".to_string(),
                description: "Deploy to Kubernetes".to_string(),
                category: "devops".to_string(),
                triggers: vec!["deploy".to_string(), "k8s".to_string()],
            },
            nanobot_learning::prompt::SkillIndexEntry {
                name: "run-tests".to_string(),
                description: "Run the test suite".to_string(),
                category: "testing".to_string(),
                triggers: vec!["test".to_string()],
            },
        ];
        let builder = ContextBuilder::new(&config).with_skill_index(entries);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
        assert!(prompt.contains("## Skill Index"));
        assert!(prompt.contains("**deploy-k8s** [devops]: Deploy to Kubernetes"));
        assert!(prompt.contains("**run-tests** [testing]: Run the test suite"));
    }

    #[test]
    fn test_skill_index_empty_not_shown() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config).with_skill_index(vec![]);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
        assert!(!prompt.contains("## Skill Index"));
    }

    #[test]
    fn test_skill_index_not_set_not_shown() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
        assert!(!prompt.contains("## Skill Index"));
    }

    #[test]
    fn test_enriched_context_section_order() {
        let config = Config {
            custom_instructions: Some("Be helpful.".to_string()),
            ..Config::default()
        };

        use async_trait::async_trait;
        use nanobot_tools::Tool;
        use nanobot_tools::ToolError;

        struct OrderTool;
        #[async_trait]
        impl Tool for OrderTool {
            fn name(&self) -> &str {
                "order_tool"
            }
            fn description(&self) -> &str {
                "test"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _args: serde_json::Value) -> Result<String, ToolError> {
                Ok("ok".to_string())
            }
        }

        let entries = vec![nanobot_learning::prompt::SkillIndexEntry {
            name: "test-skill".to_string(),
            description: "A test".to_string(),
            category: "testing".to_string(),
            triggers: vec!["test".to_string()],
        }];

        let tools = ToolRegistry::new();
        tools.register(OrderTool);

        let builder = ContextBuilder::new(&config).with_skill_index(entries);
        let msg = make_inbound();
        let mut session = Session::new("test:key".to_string());
        session.add_user_message("history".to_string());

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();

        // Verify section ordering: System → Runtime → Memory → Notes → Skills → Tool Guidance → Memory Fence → Skill Index → Additional Instructions
        let system_pos = prompt.find("## System").unwrap();
        let runtime_pos = prompt.find("## Runtime").unwrap();
        let memory_pos = prompt.find("## Memory").unwrap();
        let tool_guidance_pos = prompt.find("## Tool Guidance").unwrap();
        let fence_pos = prompt.find("## Memory Fence").unwrap();
        let skill_index_pos = prompt.find("## Skill Index").unwrap();
        let instructions_pos = prompt.find("## Additional Instructions").unwrap();

        assert!(system_pos < runtime_pos);
        assert!(runtime_pos < memory_pos);
        assert!(memory_pos < tool_guidance_pos);
        assert!(tool_guidance_pos < fence_pos);
        assert!(fence_pos < skill_index_pos);
        assert!(skill_index_pos < instructions_pos);
    }

    #[test]
    fn test_tool_guidance_multiple_tools() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        use async_trait::async_trait;
        use nanobot_tools::Tool;
        use nanobot_tools::ToolError;

        struct ToolA;
        #[async_trait]
        impl Tool for ToolA {
            fn name(&self) -> &str {
                "tool_a"
            }
            fn description(&self) -> &str {
                "First tool"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _args: serde_json::Value) -> Result<String, ToolError> {
                Ok("a".to_string())
            }
        }

        struct ToolB;
        #[async_trait]
        impl Tool for ToolB {
            fn name(&self) -> &str {
                "tool_b"
            }
            fn description(&self) -> &str {
                "Second tool"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _args: serde_json::Value) -> Result<String, ToolError> {
                Ok("b".to_string())
            }
        }

        tools.register(ToolA);
        tools.register(ToolB);

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
        assert!(prompt.contains("### tool_a"));
        assert!(prompt.contains("First tool"));
        assert!(prompt.contains("### tool_b"));
        assert!(prompt.contains("Second tool"));
    }

    #[test]
    fn test_tool_guidance_budget_truncates_schema() {
        let config = Config::default();
        let builder = ContextBuilder::new(&config).with_tool_guidance_token_budget(50);
        let msg = make_inbound();
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        use async_trait::async_trait;
        use nanobot_tools::Tool;
        use nanobot_tools::ToolError;

        struct VerboseTool;
        #[async_trait]
        impl Tool for VerboseTool {
            fn name(&self) -> &str {
                "verbose_tool"
            }
            fn description(&self) -> &str {
                "Tool with a very large schema"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "payload": {
                            "type": "string",
                            "description": "x".repeat(600)
                        }
                    }
                })
            }
            async fn execute(&self, _args: serde_json::Value) -> Result<String, ToolError> {
                Ok("ok".to_string())
            }
        }

        tools.register(VerboseTool);

        let prompt = builder
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();
        assert!(prompt.contains("## Tool Guidance"));
        assert!(prompt.contains("### verbose_tool"));
        assert!(prompt.contains("Parameters:"));
        assert!(prompt.contains("..."));
    }
}
