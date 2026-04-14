//! Prompt assembly for injecting learned context into agent prompts.
//!
//! Defines [`PromptSection`] variants and [`PromptAssembler`] which combines
//! sections into a single system prompt string.

use serde::{Deserialize, Serialize};

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
}

impl PromptSection {
    /// Returns the text content of this section.
    pub fn content(&self) -> &str {
        match self {
            Self::Memory { content } => content,
            Self::Skills { content } => content,
            Self::System { content } => content,
            Self::Custom { content, .. } => content,
        }
    }

    /// Returns the header label used when rendering the section.
    pub fn header(&self) -> &str {
        match self {
            Self::Memory { .. } => "Memory",
            Self::Skills { .. } => "Skills",
            Self::System { .. } => "System",
            Self::Custom { label, .. } => label,
        }
    }
}

/// Assembles [`PromptSection`]s into a single prompt string.
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
}
