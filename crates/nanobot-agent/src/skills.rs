//! Skills loader — loads markdown-based skill definitions.
//!
//! Mirrors the Python `agent/skills.py` SkillsLoader with frontmatter parsing.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// A loaded skill definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Skill name.
    pub name: String,

    /// Human-readable description.
    pub description: String,

    /// The skill instructions (markdown body).
    pub instructions: String,

    /// Required binaries.
    #[serde(default)]
    pub requires_bin: Vec<String>,

    /// Required environment variables.
    #[serde(default)]
    pub requires_env: Vec<String>,

    /// Tags for categorization.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Loads skills from markdown files with YAML frontmatter.
pub struct SkillsLoader {
    skills_dir: PathBuf,
}

impl SkillsLoader {
    pub fn new(skills_dir: PathBuf) -> Self {
        Self { skills_dir }
    }

    /// Load all available skills.
    pub fn load_all(&self) -> Result<Vec<Skill>> {
        if !self.skills_dir.exists() {
            debug!(
                "Skills directory does not exist: {}",
                self.skills_dir.display()
            );
            return Ok(Vec::new());
        }

        let mut skills = Vec::new();
        let entries = std::fs::read_dir(&self.skills_dir)
            .with_context(|| format!("Failed to read skills dir: {}", self.skills_dir.display()))?;

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let skill_file = path.join("SKILL.md");
            if !skill_file.exists() {
                continue;
            }

            match self.load_skill(&skill_file) {
                Ok(skill) => {
                    debug!("Loaded skill: {}", skill.name);
                    skills.push(skill);
                }
                Err(e) => {
                    warn!("Failed to load skill from {}: {}", skill_file.display(), e);
                }
            }
        }

        Ok(skills)
    }

    /// Load a single skill from a SKILL.md file.
    fn load_skill(&self, path: &Path) -> Result<Skill> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read skill file: {}", path.display()))?;

        let (frontmatter, body) = parse_frontmatter(&content)?;

        let name = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let description = frontmatter
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let requires_bin = yaml_string_array(&frontmatter, "requires_bin");
        let requires_env = yaml_string_array(&frontmatter, "requires_env");
        let tags = yaml_string_array(&frontmatter, "tags");

        Ok(Skill {
            name,
            description,
            instructions: body.trim().to_string(),
            requires_bin,
            requires_env,
            tags,
        })
    }

    /// Get the skills section for the system prompt.
    pub fn skills_prompt(skills: &[Skill]) -> String {
        if skills.is_empty() {
            return String::new();
        }

        let mut parts = vec!["## Available Skills\n".to_string()];
        for skill in skills {
            parts.push(format!("\n### {}\n{}", skill.name, skill.instructions));
        }
        parts.join("\n")
    }
}

/// Extract a string array from a YAML value's field.
fn yaml_string_array(value: &serde_yaml::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse YAML frontmatter from a markdown file.
fn parse_frontmatter(content: &str) -> Result<(serde_yaml::Value, String)> {
    let trimmed = content.trim_start();

    if !trimmed.starts_with("---") {
        return Ok((serde_yaml::Value::Null, content.to_string()));
    }

    let after_first = &trimmed[3..];
    let end = after_first.find("---").context("Unclosed frontmatter")?;

    let frontmatter_str = &after_first[..end];
    let body = after_first[end + 3..].to_string();

    let frontmatter: serde_yaml::Value = serde_yaml::from_str(frontmatter_str)
        .with_context(|| "Failed to parse frontmatter YAML")?;

    Ok((frontmatter, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter_valid() {
        let input = "---\nname: test\n---\nbody text";
        let (fm, body) = parse_frontmatter(input).unwrap();
        assert_eq!(fm.get("name").unwrap().as_str(), Some("test"));
        assert_eq!(body.trim(), "body text");
    }

    #[test]
    fn test_parse_frontmatter_no_frontmatter() {
        let input = "Just some regular text\nwithout frontmatter";
        let (fm, body) = parse_frontmatter(input).unwrap();
        assert!(fm.is_null());
        assert_eq!(body, input);
    }

    #[test]
    fn test_parse_frontmatter_unclosed() {
        let input = "---\nname: test\n";
        let result = parse_frontmatter(input);
        assert!(result.is_err());
    }

    #[test]
    fn test_yaml_string_array() {
        let yaml: serde_yaml::Value =
            serde_yaml::from_str("items:\n  - apple\n  - banana\nempty_key:\nmissing: true")
                .unwrap();

        let arr = yaml_string_array(&yaml, "items");
        assert_eq!(arr, vec!["apple", "banana"]);

        // Empty value (null-like)
        let empty = yaml_string_array(&yaml, "empty_key");
        assert!(empty.is_empty());

        // Missing key
        let missing = yaml_string_array(&yaml, "nonexistent");
        assert!(missing.is_empty());
    }

    #[test]
    fn test_skills_loader_nonexistent_dir() {
        let loader = SkillsLoader::new(PathBuf::from("/tmp/nonexistent_skills_dir_12345"));
        let skills = loader.load_all().unwrap();
        assert!(skills.is_empty());
    }

    #[test]
    fn test_skills_prompt_empty() {
        let prompt = SkillsLoader::skills_prompt(&[]);
        assert!(prompt.is_empty());
    }

    #[test]
    fn test_skills_prompt_nonempty() {
        let skills = vec![Skill {
            name: "test_skill".to_string(),
            description: "A test".to_string(),
            instructions: "Do the thing".to_string(),
            requires_bin: vec![],
            requires_env: vec![],
            tags: vec![],
        }];
        let prompt = SkillsLoader::skills_prompt(&skills);
        assert!(prompt.contains("## Available Skills"));
        assert!(prompt.contains("### test_skill"));
        assert!(prompt.contains("Do the thing"));
    }
}
