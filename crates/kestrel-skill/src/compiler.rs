//! Skill compiler — parses TOML manifests and validates required fields.
//!
//! This is the Phase 1 simplified compiler: parse TOML, validate, produce a [`CompiledSkill`].
//! Future phases may add DFA/regex trigger compilation.

use std::path::Path;

use crate::error::{SkillError, SkillResult};
use crate::manifest::SkillManifest;
use crate::skill::CompiledSkill;

/// Compiles raw TOML files into executable [`CompiledSkill`] instances.
#[derive(Debug, Default)]
pub struct SkillCompiler;

impl SkillCompiler {
    /// Create a new compiler.
    pub fn new() -> Self {
        Self
    }

    /// Compile a TOML string into a [`CompiledSkill`].
    ///
    /// Parses the TOML, validates required fields, and returns a compiled skill.
    pub fn compile_str(&self, name: &str, content: &str) -> SkillResult<CompiledSkill> {
        let manifest: SkillManifest =
            toml::from_str(content).map_err(|e| SkillError::ParseFailed {
                path: name.to_string(),
                source: e,
            })?;

        self.validate_manifest(&manifest)?;
        Ok(CompiledSkill::new(manifest))
    }

    /// Compile a TOML manifest file from disk.
    ///
    /// If a companion `<name>.md` file exists next to the TOML manifest, its content
    /// is loaded as the skill's detailed instructions.
    pub fn compile_file(&self, path: &Path) -> SkillResult<CompiledSkill> {
        let content = std::fs::read_to_string(path)?;
        let name = path.display().to_string();
        let mut skill = self.compile_str(&name, &content)?;

        // Check for companion instructions file
        let md_path = path.with_extension("md");
        if md_path.exists() {
            let instructions = std::fs::read_to_string(&md_path)?;
            skill.set_instructions(instructions);
        }

        Ok(skill)
    }

    /// Validate a parsed manifest, returning an error on failure.
    fn validate_manifest(&self, manifest: &SkillManifest) -> SkillResult<()> {
        manifest
            .validate()
            .map_err(|errors| SkillError::ValidationFailed {
                name: manifest.name.clone(),
                reason: errors.join("; "),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::Skill;

    const VALID_TOML: &str = r#"
name = "deploy-k8s"
version = "1.0.0"
description = "Deploy application to Kubernetes"
triggers = ["deploy", "k8s", "kubernetes"]
steps = ["Check kubeconfig", "Apply manifests"]
pitfalls = ["Do not deploy on Fridays"]
category = "devops"
"#;

    #[test]
    fn test_compile_valid_str() {
        let compiler = SkillCompiler::new();
        let skill = compiler.compile_str("test", VALID_TOML).unwrap();
        assert_eq!(skill.name(), "deploy-k8s");
        assert_eq!(skill.category(), "devops");
    }

    #[test]
    fn test_compile_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deploy.toml");
        std::fs::write(&path, VALID_TOML).unwrap();

        let compiler = SkillCompiler::new();
        let skill = compiler.compile_file(&path).unwrap();
        assert_eq!(skill.name(), "deploy-k8s");
    }

    #[test]
    fn test_compile_invalid_toml() {
        let compiler = SkillCompiler::new();
        let result = compiler.compile_str("bad", "not valid [[[");
        assert!(result.is_err());
        match result.unwrap_err() {
            SkillError::ParseFailed { path, .. } => assert_eq!(path, "bad"),
            other => panic!("expected ParseFailed, got {other}"),
        }
    }

    #[test]
    fn test_compile_missing_name() {
        let compiler = SkillCompiler::new();
        let toml = r#"
version = "1.0.0"
description = "desc"
triggers = ["x"]
"#;
        let result = compiler.compile_str("test", toml);
        assert!(result.is_err());
    }

    #[test]
    fn test_compile_missing_triggers() {
        let compiler = SkillCompiler::new();
        let toml = r#"
name = "x"
version = "1.0.0"
description = "desc"
triggers = []
"#;
        let result = compiler.compile_str("test", toml);
        assert!(matches!(
            result.unwrap_err(),
            SkillError::ValidationFailed { .. }
        ));
    }

    #[test]
    fn test_compile_missing_file() {
        let compiler = SkillCompiler::new();
        let result = compiler.compile_file(Path::new("/nonexistent/skill.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn test_compile_minimal_manifest() {
        let compiler = SkillCompiler::new();
        let toml = r#"
name = "minimal"
version = "0.1.0"
description = "Just the basics"
triggers = ["min"]
"#;
        let skill = compiler.compile_str("test", toml).unwrap();
        assert_eq!(skill.name(), "minimal");
        assert_eq!(skill.category(), "uncategorized");
    }
}
