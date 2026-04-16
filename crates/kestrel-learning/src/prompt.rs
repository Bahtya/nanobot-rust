//! Prompt assembly for injecting learned context into agent prompts.
//!
//! Defines [`PromptSection`] variants and [`PromptAssembler`] which combines
//! sections into a single system prompt string.

use serde::{Deserialize, Serialize};

const DEFAULT_SKILL_INDEX_MAX_ENTRIES: usize = 10;

/// A section of the assembled prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptSection {
    /// Memory context injected from the memory store.
    Memory { content: String },
    /// Skill descriptions and instructions.
    Skills { content: String },
    /// Base system prompt.
    System { content: String },
    /// Custom section with a label.
    Custom { label: String, content: String },
    /// Tool usage guidance — when to use which tool, with descriptions and parameters.
    ToolGuidance { content: String },
    /// Memory fence — structured recall triggers that hint the agent when to recall
    /// memories by category (e.g. "when discussing deployment, recall environment facts").
    MemoryFence { content: String },
    /// Skill index — list of available skills with descriptions, categories, and triggers.
    SkillIndex { content: String },
}

/// A tool metadata entry used to build tool guidance context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    /// Tool name (e.g. "exec", "read_file").
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for the tool's parameters (serialized as a string).
    pub parameters_schema: String,
}

/// A memory fence entry defining a category-based recall trigger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFenceEntry {
    /// Memory category to trigger recall for (e.g. "environment", "user_profile").
    pub category: String,
    /// Hint text describing when to recall this category.
    pub hint: String,
}

/// A skill index entry describing an available skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillIndexEntry {
    /// Skill name (kebab-case).
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Skill category (e.g. "devops", "security").
    pub category: String,
    /// Keyword triggers for the skill.
    pub triggers: Vec<String>,
}

impl PromptSection {
    /// Returns the text content of this section.
    pub fn content(&self) -> &str {
        match self {
            Self::Memory { content } => content,
            Self::Skills { content } => content,
            Self::System { content } => content,
            Self::Custom { content, .. } => content,
            Self::ToolGuidance { content } => content,
            Self::MemoryFence { content } => content,
            Self::SkillIndex { content } => content,
        }
    }

    /// Returns the header label used when rendering the section.
    pub fn header(&self) -> &str {
        match self {
            Self::Memory { .. } => "Memory",
            Self::Skills { .. } => "Skills",
            Self::System { .. } => "System",
            Self::Custom { label, .. } => label,
            Self::ToolGuidance { .. } => "Tool Guidance",
            Self::MemoryFence { .. } => "Memory Fence",
            Self::SkillIndex { .. } => "Skill Index",
        }
    }
}

/// Assembles [`PromptSection`]s into a single prompt string.
#[derive(Clone)]
pub struct PromptAssembler {
    /// Separator between sections.
    separator: String,
}

impl PromptAssembler {
    /// Creates a new assembler with the default separator (double newline).
    pub fn new() -> Self {
        Self {
            separator: "\n\n".into(),
        }
    }

    /// Creates a new assembler with a custom separator.
    pub fn with_separator(separator: impl Into<String>) -> Self {
        Self {
            separator: separator.into(),
        }
    }

    /// Assembles multiple sections into a single prompt string.
    ///
    /// Each section is rendered with a markdown header (`## Label`) and
    /// its content. Sections are joined with the configured separator.
    /// Empty sections are skipped.
    pub fn assemble(&self, sections: &[PromptSection]) -> String {
        let parts: Vec<String> = sections
            .iter()
            .filter(|s| !s.content().is_empty())
            .map(|s| format!("## {}\n{}", s.header(), s.content()))
            .collect();

        parts.join(&self.separator)
    }
}

impl Default for PromptAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptAssembler {
    /// Build tool guidance content from tool metadata entries.
    ///
    /// Produces a formatted string listing each tool with its name, description,
    /// and parameter schema, to help the LLM decide when and how to use each tool.
    pub fn build_tool_guidance(tools: &[ToolInfo]) -> String {
        Self::build_tool_guidance_with_budget(tools, usize::MAX)
    }

    /// Build tool guidance content from tool metadata entries with a token budget.
    ///
    /// When the rendered guidance would exceed `max_tokens`, individual tool
    /// parameter schemas are truncated using a simple `len / 4` token estimate.
    pub fn build_tool_guidance_with_budget(tools: &[ToolInfo], max_tokens: usize) -> String {
        if tools.is_empty() {
            return String::new();
        }

        if max_tokens == usize::MAX {
            return Self::render_tool_guidance(tools, None);
        }

        let full = Self::render_tool_guidance(tools, None);
        if Self::estimate_tokens(&full) <= max_tokens {
            return full;
        }

        let mut schema_limits = vec![0usize; tools.len()];
        let base = Self::render_tool_guidance(tools, Some(&schema_limits));
        let base_tokens = Self::estimate_tokens(&base);
        if base_tokens >= max_tokens {
            return base;
        }

        for (index, tool) in tools.iter().enumerate() {
            if tool.parameters_schema.is_empty() {
                continue;
            }

            let mut low = 0usize;
            let mut high = tool.parameters_schema.len();
            while low < high {
                let mid = (low + high).div_ceil(2);
                schema_limits[index] = mid;
                let candidate = Self::render_tool_guidance(tools, Some(&schema_limits));
                if Self::estimate_tokens(&candidate) <= max_tokens {
                    low = mid;
                } else {
                    high = mid - 1;
                }
            }
            schema_limits[index] = low;
        }

        Self::render_tool_guidance(tools, Some(&schema_limits))
    }

    fn render_tool_guidance(tools: &[ToolInfo], schema_limits: Option<&[usize]>) -> String {
        let mut parts = Vec::new();
        parts.push("Available tools and when to use them:\n".to_string());

        for (index, tool) in tools.iter().enumerate() {
            parts.push(format!("### {}\n{}", tool.name, tool.description));
            if !tool.parameters_schema.is_empty() {
                let rendered_schema = match schema_limits.and_then(|limits| limits.get(index)) {
                    Some(limit) => Self::truncate_schema(&tool.parameters_schema, *limit),
                    None => tool.parameters_schema.clone(),
                };
                if !rendered_schema.is_empty() {
                    parts.push(format!("Parameters: {}", rendered_schema));
                }
            }
            parts.push(String::new());
        }

        parts.join("\n")
    }

    /// Build memory fence content from category-based recall triggers.
    ///
    /// Produces structured hints that guide the agent on when to recall specific
    /// memory categories. For example: "When discussing project setup, recall
    /// environment facts."
    pub fn build_memory_fence(fences: &[MemoryFenceEntry]) -> String {
        if fences.is_empty() {
            return String::new();
        }

        let mut parts = Vec::new();
        parts.push(
            "Memory recall triggers — consider recalling relevant memories when:\n".to_string(),
        );

        for fence in fences {
            parts.push(format!("- **{}**: {}", fence.category, fence.hint));
        }

        parts.join("\n")
    }

    /// Build skill index content from skill metadata entries.
    ///
    /// Produces a formatted list of all available skills with their descriptions,
    /// categories, and trigger keywords, so the agent knows what skills it can invoke.
    pub fn build_skill_index(skills: &[SkillIndexEntry], max_entries: usize) -> String {
        if skills.is_empty() {
            return String::new();
        }
        let max_entries = if max_entries == 0 {
            DEFAULT_SKILL_INDEX_MAX_ENTRIES
        } else {
            max_entries
        };

        let mut parts = Vec::new();
        parts.push("Available skills:\n".to_string());

        for skill in skills.iter().take(max_entries) {
            let triggers = skill.triggers.join(", ");
            parts.push(format!(
                "- **{}** [{}]: {} (triggers: {})",
                skill.name, skill.category, skill.description, triggers
            ));
        }

        parts.join("\n")
    }

    fn estimate_tokens(content: &str) -> usize {
        content.len().div_ceil(4)
    }

    fn truncate_schema(schema: &str, max_len: usize) -> String {
        if max_len == 0 {
            return String::new();
        }
        if schema.len() <= max_len {
            return schema.to_string();
        }

        let ellipsis = "...";
        let mut end = max_len.saturating_sub(ellipsis.len());
        while end > 0 && !schema.is_char_boundary(end) {
            end -= 1;
        }

        if end == 0 {
            ellipsis.to_string()
        } else {
            format!("{}{}", &schema[..end], ellipsis)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_basic() {
        let assembler = PromptAssembler::new();
        let sections = vec![
            PromptSection::System {
                content: "You are a helpful assistant.".into(),
            },
            PromptSection::Memory {
                content: "User prefers concise answers.".into(),
            },
        ];

        let result = assembler.assemble(&sections);
        assert!(result.contains("## System"));
        assert!(result.contains("You are a helpful assistant."));
        assert!(result.contains("## Memory"));
        assert!(result.contains("User prefers concise answers."));
    }

    #[test]
    fn assemble_skips_empty() {
        let assembler = PromptAssembler::new();
        let sections = vec![
            PromptSection::System {
                content: "Base prompt.".into(),
            },
            PromptSection::Memory {
                content: String::new(),
            },
            PromptSection::Skills {
                content: "Skill A: deploy.".into(),
            },
        ];

        let result = assembler.assemble(&sections);
        assert!(result.contains("## System"));
        assert!(!result.contains("## Memory"));
        assert!(result.contains("## Skills"));
    }

    #[test]
    fn assemble_custom_section() {
        let assembler = PromptAssembler::new();
        let sections = vec![PromptSection::Custom {
            label: "Project Context".into(),
            content: "Rust project with 15 crates.".into(),
        }];

        let result = assembler.assemble(&sections);
        assert!(result.contains("## Project Context"));
        assert!(result.contains("Rust project with 15 crates."));
    }

    #[test]
    fn assemble_empty_input() {
        let assembler = PromptAssembler::new();
        let result = assembler.assemble(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn custom_separator() {
        let assembler = PromptAssembler::with_separator("\n---\n");
        let sections = vec![
            PromptSection::System {
                content: "A".into(),
            },
            PromptSection::Skills {
                content: "B".into(),
            },
        ];

        let result = assembler.assemble(&sections);
        assert!(result.contains("\n---\n"));
    }

    #[test]
    fn section_header_and_content() {
        let section = PromptSection::Custom {
            label: "Test".into(),
            content: "hello".into(),
        };
        assert_eq!(section.header(), "Test");
        assert_eq!(section.content(), "hello");
    }

    #[test]
    fn prompt_section_serde() {
        let section = PromptSection::Memory {
            content: "test".into(),
        };
        let json = serde_json::to_string(&section).expect("serialize");
        let decoded: PromptSection = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(section, decoded);
    }

    // ── New section variant tests ──────────────────────────────────

    #[test]
    fn tool_guidance_header() {
        let section = PromptSection::ToolGuidance {
            content: "use exec for shell".into(),
        };
        assert_eq!(section.header(), "Tool Guidance");
        assert_eq!(section.content(), "use exec for shell");
    }

    #[test]
    fn memory_fence_header() {
        let section = PromptSection::MemoryFence {
            content: "recall env facts".into(),
        };
        assert_eq!(section.header(), "Memory Fence");
        assert_eq!(section.content(), "recall env facts");
    }

    #[test]
    fn skill_index_header() {
        let section = PromptSection::SkillIndex {
            content: "deploy-k8s skill".into(),
        };
        assert_eq!(section.header(), "Skill Index");
        assert_eq!(section.content(), "deploy-k8s skill");
    }

    #[test]
    fn tool_guidance_serde_roundtrip() {
        let section = PromptSection::ToolGuidance {
            content: "guidance".into(),
        };
        let json = serde_json::to_string(&section).unwrap();
        let back: PromptSection = serde_json::from_str(&json).unwrap();
        assert_eq!(section, back);
    }

    #[test]
    fn memory_fence_serde_roundtrip() {
        let section = PromptSection::MemoryFence {
            content: "fence".into(),
        };
        let json = serde_json::to_string(&section).unwrap();
        let back: PromptSection = serde_json::from_str(&json).unwrap();
        assert_eq!(section, back);
    }

    #[test]
    fn skill_index_serde_roundtrip() {
        let section = PromptSection::SkillIndex {
            content: "index".into(),
        };
        let json = serde_json::to_string(&section).unwrap();
        let back: PromptSection = serde_json::from_str(&json).unwrap();
        assert_eq!(section, back);
    }

    #[test]
    fn assemble_with_tool_guidance() {
        let assembler = PromptAssembler::new();
        let sections = vec![
            PromptSection::System {
                content: "system".into(),
            },
            PromptSection::ToolGuidance {
                content: "Use exec for commands.".into(),
            },
        ];
        let result = assembler.assemble(&sections);
        assert!(result.contains("## System"));
        assert!(result.contains("## Tool Guidance"));
        assert!(result.contains("Use exec for commands."));
    }

    #[test]
    fn assemble_with_memory_fence() {
        let assembler = PromptAssembler::new();
        let sections = vec![
            PromptSection::System {
                content: "system".into(),
            },
            PromptSection::MemoryFence {
                content: "recall preferences".into(),
            },
        ];
        let result = assembler.assemble(&sections);
        assert!(result.contains("## Memory Fence"));
        assert!(result.contains("recall preferences"));
    }

    #[test]
    fn assemble_with_skill_index() {
        let assembler = PromptAssembler::new();
        let sections = vec![
            PromptSection::System {
                content: "system".into(),
            },
            PromptSection::SkillIndex {
                content: "deploy-k8s".into(),
            },
        ];
        let result = assembler.assemble(&sections);
        assert!(result.contains("## Skill Index"));
        assert!(result.contains("deploy-k8s"));
    }

    #[test]
    fn assemble_skips_empty_new_variants() {
        let assembler = PromptAssembler::new();
        let sections = vec![
            PromptSection::System {
                content: "base".into(),
            },
            PromptSection::ToolGuidance {
                content: String::new(),
            },
            PromptSection::MemoryFence {
                content: String::new(),
            },
            PromptSection::SkillIndex {
                content: String::new(),
            },
        ];
        let result = assembler.assemble(&sections);
        assert!(result.contains("## System"));
        assert!(!result.contains("## Tool Guidance"));
        assert!(!result.contains("## Memory Fence"));
        assert!(!result.contains("## Skill Index"));
    }

    // ── Builder method tests ───────────────────────────────────────

    #[test]
    fn build_tool_guidance_basic() {
        let tools = vec![
            ToolInfo {
                name: "exec".into(),
                description: "Execute shell commands".into(),
                parameters_schema:
                    r#"{"type":"object","properties":{"command":{"type":"string"}}}"#.into(),
            },
            ToolInfo {
                name: "read_file".into(),
                description: "Read file contents".into(),
                parameters_schema: String::new(),
            },
        ];
        let result = PromptAssembler::build_tool_guidance(&tools);
        assert!(result.contains("### exec"));
        assert!(result.contains("Execute shell commands"));
        assert!(result.contains("Parameters:"));
        assert!(result.contains("### read_file"));
        assert!(result.contains("Read file contents"));
    }

    #[test]
    fn build_tool_guidance_empty() {
        let result = PromptAssembler::build_tool_guidance(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn build_tool_guidance_without_schema() {
        let tools = vec![ToolInfo {
            name: "exec".into(),
            description: "Run a command".into(),
            parameters_schema: String::new(),
        }];
        let result = PromptAssembler::build_tool_guidance(&tools);
        assert!(result.contains("### exec"));
        assert!(result.contains("Run a command"));
        // No parameters schema → no "Parameters:" line
        assert!(!result.contains("Parameters:"));
    }

    #[test]
    fn build_memory_fence_basic() {
        let fences = vec![
            MemoryFenceEntry {
                category: "environment".into(),
                hint: "When discussing project setup or tools".into(),
            },
            MemoryFenceEntry {
                category: "user_profile".into(),
                hint: "When personalizing responses".into(),
            },
        ];
        let result = PromptAssembler::build_memory_fence(&fences);
        assert!(result.contains("Memory recall triggers"));
        assert!(result.contains("**environment**: When discussing project setup or tools"));
        assert!(result.contains("**user_profile**: When personalizing responses"));
    }

    #[test]
    fn build_memory_fence_empty() {
        let result = PromptAssembler::build_memory_fence(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn build_skill_index_basic() {
        let skills = vec![
            SkillIndexEntry {
                name: "deploy-k8s".into(),
                description: "Deploy to Kubernetes".into(),
                category: "devops".into(),
                triggers: vec!["deploy".into(), "k8s".into()],
            },
            SkillIndexEntry {
                name: "run-tests".into(),
                description: "Run test suite".into(),
                category: "testing".into(),
                triggers: vec!["test".into()],
            },
        ];
        let result = PromptAssembler::build_skill_index(&skills, DEFAULT_SKILL_INDEX_MAX_ENTRIES);
        assert!(result.contains("Available skills"));
        assert!(result
            .contains("**deploy-k8s** [devops]: Deploy to Kubernetes (triggers: deploy, k8s)"));
        assert!(result.contains("**run-tests** [testing]: Run test suite (triggers: test)"));
    }

    #[test]
    fn build_skill_index_empty() {
        let result = PromptAssembler::build_skill_index(&[], DEFAULT_SKILL_INDEX_MAX_ENTRIES);
        assert!(result.is_empty());
    }

    #[test]
    fn build_skill_index_limits_entries() {
        let skills = vec![
            SkillIndexEntry {
                name: "one".into(),
                description: "First".into(),
                category: "test".into(),
                triggers: vec!["one".into()],
            },
            SkillIndexEntry {
                name: "two".into(),
                description: "Second".into(),
                category: "test".into(),
                triggers: vec!["two".into()],
            },
        ];

        let result = PromptAssembler::build_skill_index(&skills, 1);
        assert!(result.contains("**one**"));
        assert!(!result.contains("**two**"));
    }

    #[test]
    fn build_tool_guidance_truncates_schema_to_budget() {
        let tools = vec![ToolInfo {
            name: "exec".into(),
            description: "Run commands".into(),
            parameters_schema: format!(
                "{{\"type\":\"object\",\"properties\":{{\"command\":{{\"type\":\"string\"}},\"padding\":\"{}\"}}}}",
                "x".repeat(400)
            ),
        }];

        let result = PromptAssembler::build_tool_guidance_with_budget(&tools, 40);
        assert!(result.contains("### exec"));
        assert!(result.contains("Parameters:"));
        assert!(result.contains("..."));
        assert!(PromptAssembler::estimate_tokens(&result) <= 40);
    }

    #[test]
    fn tool_info_serde_roundtrip() {
        let info = ToolInfo {
            name: "exec".into(),
            description: "Execute commands".into(),
            parameters_schema: "{}".into(),
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: ToolInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, back);
    }

    #[test]
    fn memory_fence_entry_serde_roundtrip() {
        let entry = MemoryFenceEntry {
            category: "fact".into(),
            hint: "Recall when asked about facts".into(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: MemoryFenceEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn skill_index_entry_serde_roundtrip() {
        let entry = SkillIndexEntry {
            name: "test-skill".into(),
            description: "A test".into(),
            category: "testing".into(),
            triggers: vec!["test".into(), "unit".into()],
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: SkillIndexEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }
}
