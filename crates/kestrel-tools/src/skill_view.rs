//! Skill viewing tool backed by the runtime [`SkillRegistry`].

use crate::trait_def::{Tool, ToolError};
use async_trait::async_trait;
use kestrel_skill::SkillRegistry;
use serde::Serialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// Tool for loading a skill's full manifest and companion instructions.
pub struct SkillViewTool {
    registry: Arc<SkillRegistry>,
}

impl SkillViewTool {
    /// Create a new skill-view tool backed by the provided registry.
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        Self { registry }
    }

    fn resolve_instruction_path(
        &self,
        requested_path: Option<&str>,
        name: &str,
    ) -> Option<PathBuf> {
        let skills_dir = self.registry.skills_dir()?;
        let candidate = match requested_path {
            Some(path) if !path.trim().is_empty() => {
                let path = PathBuf::from(path);
                if path.is_absolute() {
                    path
                } else {
                    skills_dir.join(path)
                }
            }
            _ => skills_dir.join(format!("{name}.md")),
        };

        let canonical_skills_dir = std::fs::canonicalize(skills_dir).ok()?;
        let canonical_candidate = std::fs::canonicalize(&candidate).ok()?;
        if canonical_candidate.starts_with(&canonical_skills_dir) {
            Some(canonical_candidate)
        } else {
            None
        }
    }
}

#[derive(Debug, Serialize)]
struct SkillViewResponse {
    name: String,
    description: String,
    category: String,
    manifest: kestrel_skill::SkillManifest,
    instructions: String,
    instruction_file: Option<String>,
}

#[async_trait]
impl Tool for SkillViewTool {
    fn name(&self) -> &str {
        "skill_view"
    }

    fn description(&self) -> &str {
        "Load the full manifest and detailed instructions for a registered skill."
    }

    fn toolset(&self) -> &str {
        "skills"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Registered skill name to inspect" },
                "file_path": { "type": "string", "description": "Optional companion instructions file to read, relative to the skills directory" }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let name = args["name"]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ToolError::Validation("Missing 'name' parameter".to_string()))?;
        let requested_path = args["file_path"]
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let skill = self
            .registry
            .get(name)
            .await
            .ok_or_else(|| ToolError::Execution(format!("Skill not found: {name}")))?;
        let (manifest, default_instructions) = {
            let skill = skill.read();
            (skill.manifest().clone(), skill.instructions().to_string())
        };

        let (instructions, instruction_file) = match self
            .resolve_instruction_path(requested_path, name)
        {
            Some(path) => {
                let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
                    ToolError::Execution(format!(
                        "Failed to read instruction file {}: {}",
                        path.display(),
                        e
                    ))
                })?;
                (content, Some(path.display().to_string()))
            }
            None if requested_path.is_some() => {
                return Err(ToolError::Execution(
                    "Requested instruction file is unavailable or outside skills_dir".to_string(),
                ));
            }
            None => (default_instructions, None),
        };

        serde_json::to_string_pretty(&SkillViewResponse {
            name: manifest.name.clone(),
            description: manifest.description.clone(),
            category: manifest.category.clone(),
            manifest,
            instructions,
            instruction_file,
        })
        .map_err(|e| ToolError::Execution(format!("Failed to serialize skill payload: {e}")))
    }
}

impl Default for SkillViewTool {
    fn default() -> Self {
        Self::new(Arc::new(SkillRegistry::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_skill::manifest::SkillManifestBuilder;
    use kestrel_skill::skill::CompiledSkill;

    fn build_skill() -> CompiledSkill {
        let mut skill = CompiledSkill::new(
            SkillManifestBuilder::new("deploy-k8s", "1.0.0", "Deploy to Kubernetes")
                .triggers(vec!["deploy".to_string(), "k8s".to_string()])
                .steps(vec!["Apply manifests".to_string()])
                .pitfalls(vec!["Verify rollout".to_string()])
                .category("devops")
                .build(),
        );
        skill.set_instructions("# Deploy\nRun kubectl apply.".to_string());
        skill
    }

    #[tokio::test]
    async fn skill_view_returns_manifest_and_instructions() {
        let registry = Arc::new(SkillRegistry::new());
        registry.register(build_skill()).await.unwrap();
        let tool = SkillViewTool::new(registry);

        let result = tool.execute(json!({ "name": "deploy-k8s" })).await.unwrap();
        let payload: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(payload["name"], "deploy-k8s");
        assert_eq!(payload["category"], "devops");
        assert_eq!(payload["manifest"]["steps"][0], "Apply manifests");
        assert!(payload["instructions"]
            .as_str()
            .unwrap()
            .contains("kubectl apply"));
    }

    #[tokio::test]
    async fn skill_view_reads_requested_instruction_file() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(SkillRegistry::new().with_skills_dir(dir.path()));
        registry.register(build_skill()).await.unwrap();
        let requested = dir.path().join("custom.md");
        std::fs::write(&requested, "# Override\nUse canary.").unwrap();

        let tool = SkillViewTool::new(registry);
        let result = tool
            .execute(json!({ "name": "deploy-k8s", "file_path": "custom.md" }))
            .await
            .unwrap();
        let payload: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(payload["instruction_file"], requested.display().to_string());
        assert!(payload["instructions"]
            .as_str()
            .unwrap()
            .contains("Use canary"));
    }

    #[tokio::test]
    async fn skill_view_rejects_paths_outside_skills_dir() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(SkillRegistry::new().with_skills_dir(dir.path()));
        registry.register(build_skill()).await.unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();

        let tool = SkillViewTool::new(registry);
        let err = tool
            .execute(json!({
                "name": "deploy-k8s",
                "file_path": outside.path().display().to_string()
            }))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("outside skills_dir"));
    }

    #[tokio::test]
    async fn skill_view_requires_existing_skill() {
        let tool = SkillViewTool::default();
        let err = tool
            .execute(json!({ "name": "missing" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Skill not found"));
    }

    #[test]
    fn skill_view_schema_requires_name() {
        let schema = SkillViewTool::default().parameters_schema();
        assert_eq!(schema["required"][0], "name");
        assert!(schema["properties"]["file_path"].is_object());
    }

    #[test]
    fn resolve_instruction_path_uses_skill_default_file() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(SkillRegistry::new().with_skills_dir(dir.path()));
        let tool = SkillViewTool::new(registry);
        let expected = dir.path().join("deploy-k8s.md");
        std::fs::write(&expected, "# Deploy").unwrap();

        let resolved = tool.resolve_instruction_path(None, "deploy-k8s").unwrap();
        assert_eq!(resolved, std::fs::canonicalize(expected).unwrap());
    }

    #[test]
    fn resolve_instruction_path_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Arc::new(SkillRegistry::new().with_skills_dir(dir.path()));
        let tool = SkillViewTool::new(registry);

        let resolved = tool.resolve_instruction_path(Some("../outside.md"), "deploy-k8s");
        assert!(resolved.is_none());
    }
}
