//! Skills loader — loads markdown-based skill definitions.
//!
//! Recursively loads `.md` files from the skills directory, parses YAML
//! frontmatter (name, description, parameters), and provides hot-reload
//! via file modification time tracking.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A parameter definition for a skill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillParameter {
    /// Parameter name.
    pub name: String,
    /// Human-readable description of the parameter.
    #[serde(default)]
    pub description: String,
    /// Whether this parameter is required.
    #[serde(default)]
    pub required: bool,
}

/// A loaded skill definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// Skill name (from frontmatter or file stem).
    pub name: String,

    /// Human-readable description.
    pub description: String,

    /// The skill instructions (markdown body).
    pub instructions: String,

    /// Declared parameters for the skill.
    #[serde(default)]
    pub parameters: Vec<SkillParameter>,

    /// Required binaries.
    #[serde(default)]
    pub requires_bin: Vec<String>,

    /// Required environment variables.
    #[serde(default)]
    pub requires_env: Vec<String>,

    /// Tags for categorization.
    #[serde(default)]
    pub tags: Vec<String>,

    /// Source file path.
    #[serde(skip)]
    pub source_path: PathBuf,

    /// Last known modification time of the source file.
    #[serde(skip)]
    pub modified_at: Option<std::time::SystemTime>,
}

// ---------------------------------------------------------------------------
// SkillsLoader
// ---------------------------------------------------------------------------

/// Loads skills from markdown files with YAML frontmatter.
///
/// Recursively discovers all `.md` files under `skills_dir`, parses
/// frontmatter for metadata, and tracks modification times so that
/// `reload_changed()` can hot-reload modified files.
pub struct SkillsLoader {
    skills_dir: PathBuf,
    /// Loaded skills indexed by name.
    skills: HashMap<String, Skill>,
}

impl SkillsLoader {
    /// Create a new loader pointing at the given directory.
    pub fn new(skills_dir: PathBuf) -> Self {
        Self {
            skills_dir,
            skills: HashMap::new(),
        }
    }

    /// Load (or reload) all skills from the skills directory.
    ///
    /// Recursively walks `skills_dir` and loads every `.md` file.
    pub fn load_all(&mut self) -> Result<Vec<Skill>> {
        if !self.skills_dir.exists() {
            debug!(
                "Skills directory does not exist: {}",
                self.skills_dir.display()
            );
            return Ok(Vec::new());
        }

        let md_files = collect_md_files(&self.skills_dir)?;
        let mut loaded = Vec::new();

        for path in md_files {
            match Self::load_skill_file(&path) {
                Ok(skill) => {
                    debug!("Loaded skill '{}' from {}", skill.name, path.display());
                    self.skills.insert(skill.name.clone(), skill.clone());
                    loaded.push(skill);
                }
                Err(e) => {
                    warn!("Failed to load skill from {}: {}", path.display(), e);
                }
            }
        }

        Ok(loaded)
    }

    /// Hot-reload skills whose source files have changed on disk.
    ///
    /// Returns a list of skill names that were reloaded.
    pub fn reload_changed(&mut self) -> Result<Vec<String>> {
        let mut reloaded = Vec::new();

        for (name, skill) in &mut self.skills {
            let current_mtime = file_mtime(&skill.source_path);
            if current_mtime != skill.modified_at {
                match Self::load_skill_file(&skill.source_path) {
                    Ok(updated) => {
                        info!("Hot-reloaded skill '{}'", updated.name);
                        *skill = updated;
                        reloaded.push(name.clone());
                    }
                    Err(e) => {
                        warn!(
                            "Failed to reload skill '{}' from {}: {}",
                            name,
                            skill.source_path.display(),
                            e
                        );
                    }
                }
            }
        }

        // Also check for new files that weren't loaded before.
        if self.skills_dir.exists() {
            let md_files = collect_md_files(&self.skills_dir)?;
            for path in md_files {
                // Try to load and see if it's already known by source_path.
                let already_known = self.skills.values().any(|s| s.source_path == path);

                if !already_known {
                    if let Ok(skill) = Self::load_skill_file(&path) {
                        info!(
                            "Discovered new skill '{}' at {}",
                            skill.name,
                            path.display()
                        );
                        reloaded.push(skill.name.clone());
                        self.skills.insert(skill.name.clone(), skill);
                    }
                }
            }
        }

        Ok(reloaded)
    }

    /// Return a reference to the currently loaded skills.
    pub fn skills(&self) -> &HashMap<String, Skill> {
        &self.skills
    }

    /// Load a single skill from a `.md` file.
    fn load_skill_file(path: &Path) -> Result<Skill> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read skill file: {}", path.display()))?;

        let (frontmatter, body) = parse_frontmatter(&content)?;

        // Name: frontmatter > parent dir name > file stem
        let name = frontmatter
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| {
                path.file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default()
            });

        let description = frontmatter
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let parameters = parse_parameters(&frontmatter);
        let requires_bin = yaml_string_array(&frontmatter, "requires_bin");
        let requires_env = yaml_string_array(&frontmatter, "requires_env");
        let tags = yaml_string_array(&frontmatter, "tags");
        let modified_at = file_mtime(path);

        Ok(Skill {
            name,
            description,
            instructions: body.trim().to_string(),
            parameters,
            requires_bin,
            requires_env,
            tags,
            source_path: path.to_path_buf(),
            modified_at,
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

    /// Convert loaded skills into OpenAI function definitions.
    ///
    /// Each skill becomes a function the LLM can call. The function's
    /// description is the skill's description, and its parameters come
    /// from the skill's `parameters` field.
    pub fn skills_to_function_definitions(
        skills: &[Skill],
    ) -> Vec<kestrel_core::FunctionDefinition> {
        skills
            .iter()
            .map(|skill| {
                let properties: serde_json::Map<String, serde_json::Value> = skill
                    .parameters
                    .iter()
                    .map(|p| {
                        (
                            p.name.clone(),
                            serde_json::json!({
                                "type": "string",
                                "description": p.description,
                            }),
                        )
                    })
                    .collect();

                let required: Vec<&str> = skill
                    .parameters
                    .iter()
                    .filter(|p| p.required)
                    .map(|p| p.name.as_str())
                    .collect();

                kestrel_core::FunctionDefinition {
                    name: format!("skill_{}", skill.name),
                    description: Some(skill.description.clone()),
                    parameters: Some(serde_json::json!({
                        "type": "object",
                        "properties": properties,
                        "required": required,
                    })),
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Recursively collect all `.md` files under `dir`.
fn collect_md_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk_dir(dir, &mut files)?;
    files.sort();
    Ok(files)
}

fn walk_dir(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("Failed to read dir: {}", dir.display()))?;

    for entry in entries {
        let entry = entry.context("Failed to read dir entry")?;
        let path = entry.path();

        if path.is_dir() {
            walk_dir(&path, files)?;
        } else if path.extension().map(|e| e == "md").unwrap_or(false) {
            files.push(path);
        }
    }

    Ok(())
}

/// Get the modification time of a file (if available).
fn file_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// Parse the `parameters` field from frontmatter.
///
/// Expects either:
/// ```yaml
/// parameters:
///   - name: query
///     description: The search query
///     required: true
/// ```
/// or a simple list of strings (name only).
fn parse_parameters(frontmatter: &serde_yaml::Value) -> Vec<SkillParameter> {
    let params = match frontmatter.get("parameters").and_then(|v| v.as_sequence()) {
        Some(seq) => seq,
        None => return Vec::new(),
    };

    #[allow(clippy::manual_map)]
    params
        .iter()
        .filter_map(|v| {
            // Try structured object first.
            if v.is_mapping() {
                let name = v.get("name")?.as_str()?.to_string();
                let description = v
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                let required = v.get("required").and_then(|r| r.as_bool()).unwrap_or(false);
                Some(SkillParameter {
                    name,
                    description,
                    required,
                })
            } else if let Some(name) = v.as_str() {
                // Simple string shorthand.
                Some(SkillParameter {
                    name: name.to_string(),
                    description: String::new(),
                    required: false,
                })
            } else {
                None
            }
        })
        .collect()
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

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // -----------------------------------------------------------------------
    // parse_frontmatter tests
    // -----------------------------------------------------------------------

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
    fn test_parse_frontmatter_multiline_body() {
        let input = "---\nname: test\n---\n# Heading\n\nParagraph\n- item 1\n- item 2";
        let (fm, body) = parse_frontmatter(input).unwrap();
        assert_eq!(fm.get("name").unwrap().as_str(), Some("test"));
        assert!(body.contains("# Heading"));
        assert!(body.contains("- item 2"));
    }

    #[test]
    fn test_parse_frontmatter_complex_yaml() {
        let input = "---\nname: deploy\ndescription: Deploy the app\nparameters:\n  - name: env\n    description: Target environment\n    required: true\ntags:\n  - deploy\n  - cicd\n---\nDeploy to {env}.";
        let (fm, body) = parse_frontmatter(input).unwrap();
        assert_eq!(fm.get("name").unwrap().as_str(), Some("deploy"));
        assert_eq!(
            fm.get("description").unwrap().as_str(),
            Some("Deploy the app")
        );
        let params = fm.get("parameters").unwrap().as_sequence().unwrap();
        assert_eq!(params.len(), 1);
        assert_eq!(body.trim(), "Deploy to {env}.");
    }

    // -----------------------------------------------------------------------
    // parse_parameters tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_parameters_structured() {
        let yaml = serde_yaml::from_str(
            r#"
parameters:
  - name: query
    description: The search query
    required: true
  - name: limit
    description: Max results
    required: false
"#,
        )
        .unwrap();

        let params = parse_parameters(&yaml);
        assert_eq!(params.len(), 2);
        assert_eq!(
            params[0],
            SkillParameter {
                name: "query".into(),
                description: "The search query".into(),
                required: true,
            }
        );
        assert_eq!(
            params[1],
            SkillParameter {
                name: "limit".into(),
                description: "Max results".into(),
                required: false,
            }
        );
    }

    #[test]
    fn test_parse_parameters_string_shorthand() {
        let yaml = serde_yaml::from_str(
            r#"
parameters:
  - query
  - limit
"#,
        )
        .unwrap();

        let params = parse_parameters(&yaml);
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].name, "query");
        assert!(!params[0].required);
        assert_eq!(params[1].name, "limit");
    }

    #[test]
    fn test_parse_parameters_empty() {
        let yaml: serde_yaml::Value = serde_yaml::from_str("name: test").unwrap();
        let params = parse_parameters(&yaml);
        assert!(params.is_empty());
    }

    #[test]
    fn test_parse_parameters_mixed() {
        let yaml = serde_yaml::from_str(
            r#"
parameters:
  - name: env
    description: Target env
    required: true
  - dry_run
"#,
        )
        .unwrap();

        let params = parse_parameters(&yaml);
        assert_eq!(params.len(), 2);
        assert!(params[0].required);
        assert_eq!(params[0].name, "env");
        assert_eq!(params[1].name, "dry_run");
        assert!(!params[1].required);
    }

    // -----------------------------------------------------------------------
    // yaml_string_array tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_yaml_string_array() {
        let yaml: serde_yaml::Value =
            serde_yaml::from_str("items:\n  - apple\n  - banana\nempty_key:\nmissing: true")
                .unwrap();

        assert_eq!(yaml_string_array(&yaml, "items"), vec!["apple", "banana"]);
        assert!(yaml_string_array(&yaml, "empty_key").is_empty());
        assert!(yaml_string_array(&yaml, "nonexistent").is_empty());
    }

    // -----------------------------------------------------------------------
    // SkillsLoader — filesystem tests using tempdir
    // -----------------------------------------------------------------------

    fn write_skill(dir: &Path, filename: &str, content: &str) -> PathBuf {
        let path = dir.join(filename);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_load_all_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mut loader = SkillsLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();
        assert!(skills.is_empty());
    }

    #[test]
    fn test_load_all_nonexistent_dir() {
        let mut loader = SkillsLoader::new(PathBuf::from("/tmp/nonexistent_skills_xyz_12345"));
        let skills = loader.load_all().unwrap();
        assert!(skills.is_empty());
    }

    #[test]
    fn test_load_all_single_skill() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "deploy.md",
            "---\nname: deploy\ndescription: Deploy the app\n---\n# Deploy\nRun the deployment.",
        );

        let mut loader = SkillsLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "deploy");
        assert_eq!(skills[0].description, "Deploy the app");
        assert!(skills[0].instructions.contains("Run the deployment."));
        assert!(skills[0].source_path.ends_with("deploy.md"));
        assert!(skills[0].modified_at.is_some());
    }

    #[test]
    fn test_load_all_nested_dirs() {
        let tmp = tempfile::tempdir().unwrap();

        // skills/deploy/staging.md
        write_skill(
            tmp.path(),
            "deploy/staging.md",
            "---\nname: deploy_staging\ndescription: Deploy to staging\n---\nDeploy to staging.",
        );

        // skills/monitor/health.md
        write_skill(
            tmp.path(),
            "monitor/health.md",
            "---\nname: health_check\ndescription: Check health\n---\nRun health checks.",
        );

        // skills/deep/nested/skill.md
        write_skill(
            tmp.path(),
            "deep/nested/skill.md",
            "---\nname: deep_skill\ndescription: Deep nested\n---\nDeep skill body.",
        );

        let mut loader = SkillsLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();

        assert_eq!(skills.len(), 3);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"deploy_staging"));
        assert!(names.contains(&"health_check"));
        assert!(names.contains(&"deep_skill"));
    }

    #[test]
    fn test_load_all_ignores_non_md() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "skill.md", "---\nname: good\n---\nOK");
        write_skill(tmp.path(), "notes.txt", "Not a skill");
        write_skill(tmp.path(), "config.yaml", "key: value");

        let mut loader = SkillsLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "good");
    }

    #[test]
    fn test_load_all_name_from_file_stem_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "my_awesome_skill.md",
            "---\n---\nNo name in frontmatter.",
        );

        let mut loader = SkillsLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "my_awesome_skill");
    }

    #[test]
    fn test_load_all_no_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "plain.md",
            "Just plain markdown, no frontmatter at all.",
        );

        let mut loader = SkillsLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "plain");
        assert!(skills[0].instructions.contains("Just plain markdown"));
    }

    #[test]
    fn test_load_all_with_parameters() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "search.md",
            "---\nname: search\ndescription: Search the web\nparameters:\n  - name: query\n    description: Search terms\n    required: true\n  - name: count\n    description: Number of results\n---\nSearch for {query}.",
        );

        let mut loader = SkillsLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();
        assert_eq!(skills.len(), 1);

        let skill = &skills[0];
        assert_eq!(skill.name, "search");
        assert_eq!(skill.parameters.len(), 2);
        assert_eq!(skill.parameters[0].name, "query");
        assert!(skill.parameters[0].required);
        assert_eq!(skill.parameters[1].name, "count");
        assert!(!skill.parameters[1].required);
    }

    #[test]
    fn test_load_all_with_tags_and_requires() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "deploy.md",
            "---\nname: deploy\ndescription: Deploy app\nrequires_bin:\n  - kubectl\n  - docker\nrequires_env:\n  - KUBECONFIG\ntags:\n  - deploy\n  - cicd\n---\nDeploy instructions.",
        );

        let mut loader = SkillsLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();
        let skill = &skills[0];

        assert_eq!(skill.requires_bin, vec!["kubectl", "docker"]);
        assert_eq!(skill.requires_env, vec!["KUBECONFIG"]);
        assert_eq!(skill.tags, vec!["deploy", "cicd"]);
    }

    // -----------------------------------------------------------------------
    // Hot reload tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_reload_changed_no_changes() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "skill.md",
            "---\nname: test\n---\nOriginal content.",
        );

        let mut loader = SkillsLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        // No file changes — should return empty list.
        let reloaded = loader.reload_changed().unwrap();
        assert!(reloaded.is_empty());
    }

    #[test]
    fn test_reload_changed_modified_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_skill(
            tmp.path(),
            "skill.md",
            "---\nname: test\n---\nOriginal content.",
        );

        let mut loader = SkillsLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(
            loader.skills().get("test").unwrap().instructions,
            "Original content."
        );

        // Modify the file (bump mtime).
        // On some systems, writes within the same second may have the same mtime,
        // so we explicitly set a newer mtime.
        let new_time = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        let ft = filetime::FileTime::from_system_time(new_time);
        filetime::set_file_mtime(&path, ft).unwrap();

        // Overwrite content.
        fs::write(&path, "---\nname: test\n---\nUpdated content.").unwrap();
        filetime::set_file_mtime(&path, ft).unwrap();

        let reloaded = loader.reload_changed().unwrap();
        assert_eq!(reloaded, vec!["test"]);
        assert_eq!(
            loader.skills().get("test").unwrap().instructions,
            "Updated content."
        );
    }

    #[test]
    fn test_reload_changed_new_file_discovered() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "first.md",
            "---\nname: first\n---\nFirst skill.",
        );

        let mut loader = SkillsLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.skills().len(), 1);

        // Add a new skill file.
        write_skill(
            tmp.path(),
            "second.md",
            "---\nname: second\n---\nSecond skill.",
        );

        let reloaded = loader.reload_changed().unwrap();
        assert!(reloaded.contains(&"second".to_string()));
        assert_eq!(loader.skills().len(), 2);
        assert!(loader.skills().contains_key("second"));
    }

    // -----------------------------------------------------------------------
    // skills_prompt tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_skills_prompt_empty() {
        assert!(SkillsLoader::skills_prompt(&[]).is_empty());
    }

    #[test]
    fn test_skills_prompt_nonempty() {
        let skills = vec![Skill {
            name: "test_skill".to_string(),
            description: "A test".to_string(),
            instructions: "Do the thing".to_string(),
            parameters: vec![],
            requires_bin: vec![],
            requires_env: vec![],
            tags: vec![],
            source_path: PathBuf::new(),
            modified_at: None,
        }];
        let prompt = SkillsLoader::skills_prompt(&skills);
        assert!(prompt.contains("## Available Skills"));
        assert!(prompt.contains("### test_skill"));
        assert!(prompt.contains("Do the thing"));
    }

    // -----------------------------------------------------------------------
    // skills_to_function_definitions tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_skills_to_function_definitions_basic() {
        let skills = vec![Skill {
            name: "deploy".to_string(),
            description: "Deploy the application".to_string(),
            instructions: String::new(),
            parameters: vec![SkillParameter {
                name: "env".to_string(),
                description: "Target environment".to_string(),
                required: true,
            }],
            requires_bin: vec![],
            requires_env: vec![],
            tags: vec![],
            source_path: PathBuf::new(),
            modified_at: None,
        }];

        let defs = SkillsLoader::skills_to_function_definitions(&skills);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "skill_deploy");
        assert_eq!(
            defs[0].description.as_deref(),
            Some("Deploy the application")
        );

        let params = defs[0].parameters.as_ref().unwrap();
        assert_eq!(params["required"].as_array().unwrap().len(), 1);
        assert_eq!(params["required"].as_array().unwrap()[0], "env");
        assert!(params["properties"]["env"]["type"].is_string());
    }

    #[test]
    fn test_skills_to_function_definitions_no_params() {
        let skills = vec![Skill {
            name: "hello".to_string(),
            description: "Say hello".to_string(),
            instructions: String::new(),
            parameters: vec![],
            requires_bin: vec![],
            requires_env: vec![],
            tags: vec![],
            source_path: PathBuf::new(),
            modified_at: None,
        }];

        let defs = SkillsLoader::skills_to_function_definitions(&skills);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "skill_hello");

        let params = defs[0].parameters.as_ref().unwrap();
        let props = params["properties"].as_object().unwrap();
        assert!(props.is_empty());
    }

    #[test]
    fn test_skills_to_function_definitions_multiple() {
        let skills = vec![
            Skill {
                name: "a".to_string(),
                description: "Skill A".to_string(),
                instructions: String::new(),
                parameters: vec![],
                requires_bin: vec![],
                requires_env: vec![],
                tags: vec![],
                source_path: PathBuf::new(),
                modified_at: None,
            },
            Skill {
                name: "b".to_string(),
                description: "Skill B".to_string(),
                instructions: String::new(),
                parameters: vec![SkillParameter {
                    name: "x".to_string(),
                    description: "param x".to_string(),
                    required: false,
                }],
                requires_bin: vec![],
                requires_env: vec![],
                tags: vec![],
                source_path: PathBuf::new(),
                modified_at: None,
            },
        ];

        let defs = SkillsLoader::skills_to_function_definitions(&skills);
        assert_eq!(defs.len(), 2);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"skill_a"));
        assert!(names.contains(&"skill_b"));
    }

    // -----------------------------------------------------------------------
    // collect_md_files tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_collect_md_files_flat() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "A");
        write_skill(tmp.path(), "b.md", "B");
        write_skill(tmp.path(), "c.txt", "not md");

        let files = collect_md_files(tmp.path()).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_collect_md_files_recursive() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "root.md", "root");
        write_skill(tmp.path(), "sub/nested.md", "nested");
        write_skill(tmp.path(), "sub/deep/deeper.md", "deeper");
        write_skill(tmp.path(), "sub/deep/readme.txt", "txt");

        let files = collect_md_files(tmp.path()).unwrap();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn test_collect_md_files_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let files = collect_md_files(tmp.path()).unwrap();
        assert!(files.is_empty());
    }
}
