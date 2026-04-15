//! TOML skill manifest types.
//!
//! Each skill is defined by a TOML file with required fields (name, version, description,
//! triggers) and optional fields (steps, pitfalls, category).

use serde::{Deserialize, Serialize};

/// Parsed TOML skill manifest.
///
/// Example TOML:
/// ```toml
/// name = "deploy-k8s"
/// version = "1.0.0"
/// description = "Deploy application to Kubernetes"
/// category = "devops"
/// triggers = ["deploy", "k8s", "kubernetes"]
/// steps = ["Check kubeconfig", "Apply manifests", "Verify rollout"]
/// pitfalls = ["Do not deploy to production on Fridays"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillManifest {
    /// Unique skill name in kebab-case (max 64 chars).
    pub name: String,
    /// Semantic version (e.g. "1.0.0").
    pub version: String,
    /// Human-readable description of what this skill does (max 1024 chars).
    pub description: String,
    /// Keywords that trigger this skill during matching.
    pub triggers: Vec<String>,
    /// Ordered list of steps to execute when the skill is activated.
    #[serde(default)]
    pub steps: Vec<String>,
    /// Common pitfalls or warnings for this skill.
    #[serde(default)]
    pub pitfalls: Vec<String>,
    /// Skill category for grouping (e.g. "devops", "security", "testing").
    #[serde(default = "default_category")]
    pub category: String,
}

fn default_category() -> String {
    "uncategorized".to_string()
}

impl SkillManifest {
    /// Maximum length for a skill name.
    pub const MAX_NAME_LEN: usize = 64;
    /// Maximum length for a description.
    pub const MAX_DESCRIPTION_LEN: usize = 1024;

    /// Validate the manifest fields.
    ///
    /// Ensures: non-empty name within length limit, non-empty version, non-empty description
    /// within length limit, and at least one trigger keyword.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        if self.name.is_empty() {
            errors.push("name must not be empty".to_string());
        } else if self.name.len() > Self::MAX_NAME_LEN {
            errors.push(format!(
                "name must be at most {} characters, got {}",
                Self::MAX_NAME_LEN,
                self.name.len()
            ));
        }

        if self.version.is_empty() {
            errors.push("version must not be empty".to_string());
        }

        if self.description.is_empty() {
            errors.push("description must not be empty".to_string());
        } else if self.description.len() > Self::MAX_DESCRIPTION_LEN {
            errors.push(format!(
                "description must be at most {} characters, got {}",
                Self::MAX_DESCRIPTION_LEN,
                self.description.len()
            ));
        }

        if self.triggers.is_empty() {
            errors.push("triggers must contain at least one keyword".to_string());
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Builder for constructing [`SkillManifest`] programmatically.
#[derive(Debug, Clone)]
pub struct SkillManifestBuilder {
    name: String,
    version: String,
    description: String,
    triggers: Vec<String>,
    steps: Vec<String>,
    pitfalls: Vec<String>,
    category: String,
}

impl SkillManifestBuilder {
    /// Create a new builder with the required fields.
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            description: description.into(),
            triggers: Vec::new(),
            steps: Vec::new(),
            pitfalls: Vec::new(),
            category: "uncategorized".to_string(),
        }
    }

    /// Add trigger keywords.
    pub fn triggers(mut self, triggers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.triggers = triggers.into_iter().map(Into::into).collect();
        self
    }

    /// Add execution steps.
    pub fn steps(mut self, steps: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.steps = steps.into_iter().map(Into::into).collect();
        self
    }

    /// Add pitfalls/warnings.
    pub fn pitfalls(mut self, pitfalls: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.pitfalls = pitfalls.into_iter().map(Into::into).collect();
        self
    }

    /// Set the category.
    pub fn category(mut self, category: impl Into<String>) -> Self {
        self.category = category.into();
        self
    }

    /// Build the manifest. Panics if required triggers are missing.
    pub fn build(self) -> SkillManifest {
        SkillManifest {
            name: self.name,
            version: self.version,
            description: self.description,
            triggers: self.triggers,
            steps: self.steps,
            pitfalls: self.pitfalls,
            category: self.category,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_manifest() -> SkillManifest {
        SkillManifestBuilder::new("deploy-k8s", "1.0.0", "Deploy to Kubernetes")
            .triggers(["deploy", "k8s"])
            .steps(["Apply manifests", "Verify rollout"])
            .pitfalls(["Do not deploy on Fridays"])
            .category("devops")
            .build()
    }

    #[test]
    fn test_valid_manifest() {
        let m = valid_manifest();
        assert_eq!(m.name, "deploy-k8s");
        assert_eq!(m.version, "1.0.0");
        assert_eq!(m.description, "Deploy to Kubernetes");
        assert_eq!(m.triggers, vec!["deploy", "k8s"]);
        assert_eq!(m.steps.len(), 2);
        assert_eq!(m.pitfalls.len(), 1);
        assert_eq!(m.category, "devops");
    }

    #[test]
    fn test_validate_ok() {
        let m = valid_manifest();
        assert!(m.validate().is_ok());
    }

    #[test]
    fn test_validate_empty_name() {
        let mut m = valid_manifest();
        m.name = String::new();
        let errors = m.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("name")));
    }

    #[test]
    fn test_validate_name_too_long() {
        let mut m = valid_manifest();
        m.name = "x".repeat(100);
        let errors = m.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("64 characters")));
    }

    #[test]
    fn test_validate_empty_version() {
        let mut m = valid_manifest();
        m.version = String::new();
        let errors = m.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("version")));
    }

    #[test]
    fn test_validate_empty_description() {
        let mut m = valid_manifest();
        m.description = String::new();
        let errors = m.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("description")));
    }

    #[test]
    fn test_validate_description_too_long() {
        let mut m = valid_manifest();
        m.description = "x".repeat(2000);
        let errors = m.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("1024 characters")));
    }

    #[test]
    fn test_validate_empty_triggers() {
        let mut m = valid_manifest();
        m.triggers = Vec::new();
        let errors = m.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("trigger")));
    }

    #[test]
    fn test_validate_multiple_errors() {
        let m = SkillManifest {
            name: String::new(),
            version: String::new(),
            description: String::new(),
            triggers: Vec::new(),
            steps: Vec::new(),
            pitfalls: Vec::new(),
            category: "uncategorized".to_string(),
        };
        let errors = m.validate().unwrap_err();
        assert!(errors.len() >= 4);
    }

    #[test]
    fn test_toml_roundtrip() {
        let m = valid_manifest();
        let toml_str = toml::to_string(&m).unwrap();
        let parsed: SkillManifest = toml::from_str(&toml_str).unwrap();
        assert_eq!(m, parsed);
    }

    #[test]
    fn test_toml_parse_with_defaults() {
        let toml_str = r#"
name = "hello"
version = "0.1.0"
description = "Says hello"
triggers = ["hi", "hello"]
"#;
        let m: SkillManifest = toml::from_str(toml_str).unwrap();
        assert_eq!(m.name, "hello");
        assert!(m.steps.is_empty());
        assert!(m.pitfalls.is_empty());
        assert_eq!(m.category, "uncategorized");
    }

    #[test]
    fn test_default_category() {
        assert_eq!(default_category(), "uncategorized");
    }
}
