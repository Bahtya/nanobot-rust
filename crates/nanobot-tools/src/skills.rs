//! Skill file system — loads markdown skill definitions from disk.
//!
//! Recursively scans `~/.nanobot-rs/skills/` (or a custom root) for `.md`
//! files, parses YAML frontmatter (`name`, `description`, `category`) and
//! the markdown body, and provides a [`SkillStore`] for lookup, listing,
//! and hot-reload.
//!
//! ## File format
//!
//! ```markdown
//! ---
//! name: deploy
//! description: Deploy the application
//! category: devops
//! parameters:
//!   - name: env
//!     description: Target environment
//!     required: true
//! tags:
//!   - deploy
//!   - cicd
//! ---
//!
//! # Deploy instructions
//! Deploy to {env}...
//! ```
//!
//! Subdirectories are used as implicit categories when no explicit
//! `category` is set in the frontmatter.

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
    /// Human-readable description.
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
    #[serde(default)]
    pub description: String,

    /// Category (from frontmatter, subdirectory name, or "uncategorized").
    #[serde(default)]
    pub category: String,

    /// The skill instructions (markdown body).
    pub instructions: String,

    /// Declared parameters.
    #[serde(default)]
    pub parameters: Vec<SkillParameter>,

    /// Tags for categorization / search.
    #[serde(default)]
    pub tags: Vec<String>,

    /// Semantic version string (e.g. `"1.2.3"`), parsed from frontmatter `version`.
    #[serde(default)]
    pub version: Option<String>,

    /// Declared dependency skill names, parsed from frontmatter `dependencies`.
    #[serde(default)]
    pub dependencies: Vec<String>,

    /// Source file path (relative to skills root).
    #[serde(skip)]
    pub source_path: PathBuf,

    /// Relative path from the skills root (used for category inference).
    #[serde(skip)]
    pub relative_path: PathBuf,

    /// Last known modification time for hot-reload detection.
    #[serde(skip)]
    pub modified_at: Option<std::time::SystemTime>,
}

// ---------------------------------------------------------------------------
// SkillStore
// ---------------------------------------------------------------------------

/// Stores and manages skills loaded from disk.
///
/// Supports lookup by name, listing by category, and hot-reload of changed
/// files via modification-time tracking.
pub struct SkillStore {
    /// Root directory to scan for skill files.
    root: PathBuf,

    /// Skills indexed by name.
    by_name: HashMap<String, Skill>,

    /// Name → category index.
    categories: HashMap<String, String>,
}

impl SkillStore {
    /// Create a new empty store pointing at the given root directory.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            by_name: HashMap::new(),
            categories: HashMap::new(),
        }
    }

    /// Load all skill files from the root directory (recursive).
    ///
    /// Clears any previously loaded skills. Returns the loaded skills.
    pub fn load_all(&mut self) -> Result<Vec<Skill>> {
        self.by_name.clear();
        self.categories.clear();

        if !self.root.exists() {
            debug!("Skills root does not exist: {}", self.root.display());
            return Ok(Vec::new());
        }

        let files = collect_md_files(&self.root)?;
        let mut loaded = Vec::new();

        for path in files {
            let relative = path.strip_prefix(&self.root).unwrap_or(&path).to_path_buf();
            match Self::parse_skill_file(&path, &relative) {
                Ok(skill) => {
                    debug!(
                        "Loaded skill '{}' [{}] from {}",
                        skill.name,
                        skill.category,
                        relative.display()
                    );
                    self.categories
                        .insert(skill.name.clone(), skill.category.clone());
                    self.by_name.insert(skill.name.clone(), skill.clone());
                    loaded.push(skill);
                }
                Err(e) => {
                    warn!("Failed to load skill from {}: {}", relative.display(), e);
                }
            }
        }

        info!("Loaded {} skill(s) from {}", loaded.len(), self.root.display());
        Ok(loaded)
    }

    /// Hot-reload skills whose source files have changed on disk.
    ///
    /// Also discovers newly added files. Returns names of reloaded skills.
    pub fn reload_changed(&mut self) -> Result<Vec<String>> {
        let mut reloaded = Vec::new();

        // Reload existing skills whose files changed.
        for (name, skill) in &mut self.by_name {
            let current_mtime = file_mtime(&skill.source_path);
            if current_mtime != skill.modified_at {
                let relative = skill.relative_path.clone();
                match Self::parse_skill_file(&skill.source_path, &relative) {
                    Ok(updated) => {
                        info!("Hot-reloaded skill '{}'", updated.name);
                        self.categories
                            .insert(updated.name.clone(), updated.category.clone());
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

        // Discover new files.
        if self.root.exists() {
            let files = collect_md_files(&self.root)?;
            for path in files {
                let already_known = self
                    .by_name
                    .values()
                    .any(|s| s.source_path == path);

                if !already_known {
                    let relative =
                        path.strip_prefix(&self.root).unwrap_or(&path).to_path_buf();
                    if let Ok(skill) = Self::parse_skill_file(&path, &relative) {
                        info!(
                            "Discovered new skill '{}' at {}",
                            skill.name,
                            relative.display()
                        );
                        reloaded.push(skill.name.clone());
                        self.categories
                            .insert(skill.name.clone(), skill.category.clone());
                        self.by_name.insert(skill.name.clone(), skill);
                    }
                }
            }
        }

        Ok(reloaded)
    }

    /// Look up a skill by name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.by_name.get(name)
    }

    /// Return all loaded skills.
    pub fn all(&self) -> Vec<&Skill> {
        self.by_name.values().collect()
    }

    /// List skills in a given category.
    pub fn list_by_category(&self, category: &str) -> Vec<&Skill> {
        self.by_name
            .values()
            .filter(|s| s.category == category)
            .collect()
    }

    /// Return all unique categories and the skill count in each.
    pub fn categories(&self) -> HashMap<&str, usize> {
        let mut counts = HashMap::new();
        for skill in self.by_name.values() {
            *counts.entry(skill.category.as_str()).or_insert(0) += 1;
        }
        counts
    }

    /// Number of loaded skills.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Get the root directory path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Parse a single skill file.
    fn parse_skill_file(path: &Path, relative: &Path) -> Result<Skill> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read skill file: {}", path.display()))?;

        let (frontmatter, body) = parse_frontmatter(&content)?;

        // Name: frontmatter > file stem
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

        // Category: frontmatter > parent subdirectory > "uncategorized"
        let category = frontmatter
            .get("category")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| {
                // Use the first subdirectory under root as the category.
                relative
                    .iter()
                    .next()
                    .map(|c| c.to_string_lossy().to_string())
                    .filter(|_c| relative.iter().count() > 1) // only if nested
                    .unwrap_or_else(|| "uncategorized".to_string())
            });

        let parameters = parse_parameters(&frontmatter);
        let tags = yaml_string_array(&frontmatter, "tags");
        let version = frontmatter
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from);
        let dependencies = yaml_string_array(&frontmatter, "dependencies");
        let modified_at = file_mtime(path);

        Ok(Skill {
            name,
            description,
            category,
            instructions: body.trim().to_string(),
            parameters,
            tags,
            version,
            dependencies,
            source_path: path.to_path_buf(),
            relative_path: relative.to_path_buf(),
            modified_at,
        })
    }
}

// ---------------------------------------------------------------------------
// Frontmatter parsing helpers
// ---------------------------------------------------------------------------

/// Recursively collect all `.md` files under `dir`, sorted.
fn collect_md_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk_dir(dir, &mut files)?;
    files.sort();
    Ok(files)
}

fn walk_dir(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("Failed to read dir: {}", dir.display()))?;

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

/// Parse YAML frontmatter from a markdown file.
///
/// Expects `---\n...\n---\nbody` format. Returns `(frontmatter, body)`.
/// If no frontmatter is found, returns `(Null, original_content)`.
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

/// Parse the `parameters` field from frontmatter.
///
/// Supports both structured objects and string shorthand:
/// ```yaml
/// parameters:
///   - name: query       # structured
///     description: ...
///     required: true
///   - limit              # shorthand
/// ```
fn parse_parameters(frontmatter: &serde_yaml::Value) -> Vec<SkillParameter> {
    let params = match frontmatter.get("parameters").and_then(|v| v.as_sequence()) {
        Some(seq) => seq,
        None => return Vec::new(),
    };

    #[allow(clippy::manual_map)]
    params
        .iter()
        .filter_map(|v| {
            if v.is_mapping() {
                let name = v.get("name")?.as_str()?.to_string();
                let description = v
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                let required = v
                    .get("required")
                    .and_then(|r| r.as_bool())
                    .unwrap_or(false);
                Some(SkillParameter {
                    name,
                    description,
                    required,
                })
            } else if let Some(name) = v.as_str() {
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

/// Extract a string array from a YAML frontmatter field.
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

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn write_skill(dir: &Path, filename: &str, content: &str) -> PathBuf {
        let path = dir.join(filename);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        path
    }

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
        let input = "Just regular text";
        let (fm, body) = parse_frontmatter(input).unwrap();
        assert!(fm.is_null());
        assert_eq!(body, input);
    }

    #[test]
    fn test_parse_frontmatter_unclosed() {
        assert!(parse_frontmatter("---\nname: test\n").is_err());
    }

    #[test]
    fn test_parse_frontmatter_multiline_body() {
        let input = "---\nname: test\n---\n# Heading\n\nParagraph\n- item";
        let (fm, body) = parse_frontmatter(input).unwrap();
        assert_eq!(fm.get("name").unwrap().as_str(), Some("test"));
        assert!(body.contains("# Heading"));
    }

    // -----------------------------------------------------------------------
    // parse_parameters tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_parameters_structured() {
        let yaml = serde_yaml::from_str(
            "parameters:\n  - name: env\n    description: Target\n    required: true\n",
        )
        .unwrap();
        let params = parse_parameters(&yaml);
        assert_eq!(params.len(), 1);
        assert_eq!(
            params[0],
            SkillParameter {
                name: "env".into(),
                description: "Target".into(),
                required: true,
            }
        );
    }

    #[test]
    fn test_parse_parameters_string_shorthand() {
        let yaml = serde_yaml::from_str("parameters:\n  - query\n  - limit\n").unwrap();
        let params = parse_parameters(&yaml);
        assert_eq!(params.len(), 2);
        assert!(!params[0].required);
    }

    #[test]
    fn test_parse_parameters_empty() {
        let yaml: serde_yaml::Value = serde_yaml::from_str("name: x").unwrap();
        assert!(parse_parameters(&yaml).is_empty());
    }

    // -----------------------------------------------------------------------
    // SkillStore — basic loading
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_all_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = SkillStore::new(tmp.path().to_path_buf());
        let skills = store.load_all().unwrap();
        assert!(skills.is_empty());
        assert!(store.is_empty());
    }

    #[test]
    fn test_load_all_nonexistent_dir() {
        let mut store = SkillStore::new(PathBuf::from("/tmp/no_such_skills_dir_xyz"));
        let skills = store.load_all().unwrap();
        assert!(skills.is_empty());
    }

    #[test]
    fn test_load_all_single_skill() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "deploy.md",
            "---\nname: deploy\ndescription: Deploy the app\n---\n# Deploy\nRun it.",
        );

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        let skills = store.load_all().unwrap();

        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "deploy");
        assert_eq!(skills[0].description, "Deploy the app");
        assert!(skills[0].instructions.contains("Run it."));
        assert!(skills[0].source_path.ends_with("deploy.md"));
        assert!(skills[0].modified_at.is_some());
    }

    #[test]
    fn test_load_all_name_defaults_to_file_stem() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "my_skill.md", "---\n---\nNo name field.");

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        let skills = store.load_all().unwrap();
        assert_eq!(skills[0].name, "my_skill");
    }

    #[test]
    fn test_load_all_no_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "plain.md", "Just plain markdown.");

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        let skills = store.load_all().unwrap();
        assert_eq!(skills[0].name, "plain");
        assert!(skills[0].instructions.contains("Just plain markdown."));
    }

    #[test]
    fn test_load_all_ignores_non_md() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "skill.md", "---\nname: good\n---\nOK");
        write_skill(tmp.path(), "notes.txt", "Not a skill");
        write_skill(tmp.path(), "config.yaml", "key: val");

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        let skills = store.load_all().unwrap();
        assert_eq!(skills.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Category from subdirectories
    // -----------------------------------------------------------------------

    #[test]
    fn test_category_from_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "devops/deploy.md",
            "---\nname: deploy\n---\nDeploy it.",
        );

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        store.load_all().unwrap();

        let skill = store.get("deploy").unwrap();
        assert_eq!(skill.category, "devops");
    }

    #[test]
    fn test_category_from_frontmatter_overrides_dir() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "devops/deploy.md",
            "---\nname: deploy\ncategory: infrastructure\n---\nDeploy it.",
        );

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        store.load_all().unwrap();

        let skill = store.get("deploy").unwrap();
        assert_eq!(skill.category, "infrastructure");
    }

    #[test]
    fn test_category_uncategorized_when_root_level() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "tool.md", "---\nname: tool\n---\nRoot skill.");

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        store.load_all().unwrap();

        let skill = store.get("tool").unwrap();
        assert_eq!(skill.category, "uncategorized");
    }

    #[test]
    fn test_category_deep_nested_uses_first_dir() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "devops/kubernetes/deploy.md",
            "---\nname: k8s_deploy\n---\nDeploy to k8s.",
        );

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        store.load_all().unwrap();

        let skill = store.get("k8s_deploy").unwrap();
        assert_eq!(skill.category, "devops");
    }

    // -----------------------------------------------------------------------
    // Subdirectory scanning
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_all_nested_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "deploy/staging.md",
            "---\nname: deploy_staging\n---\nStaging deploy.",
        );
        write_skill(
            tmp.path(),
            "monitor/health.md",
            "---\nname: health_check\n---\nHealth check.",
        );
        write_skill(
            tmp.path(),
            "deep/nested/skill.md",
            "---\nname: deep_skill\n---\nDeep.",
        );

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        let skills = store.load_all().unwrap();

        assert_eq!(skills.len(), 3);
        assert!(store.get("deploy_staging").is_some());
        assert!(store.get("health_check").is_some());
        assert!(store.get("deep_skill").is_some());
    }

    // -----------------------------------------------------------------------
    // Lookup and listing
    // -----------------------------------------------------------------------

    #[test]
    fn test_get_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "---\nname: alpha\n---\nAlpha.");
        write_skill(tmp.path(), "b.md", "---\nname: beta\n---\nBeta.");

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        store.load_all().unwrap();

        assert_eq!(store.get("alpha").unwrap().instructions, "Alpha.");
        assert_eq!(store.get("beta").unwrap().instructions, "Beta.");
        assert!(store.get("gamma").is_none());
    }

    #[test]
    fn test_list_by_category() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "devops/deploy.md",
            "---\nname: deploy\n---\nDeploy.",
        );
        write_skill(
            tmp.path(),
            "devops/rollback.md",
            "---\nname: rollback\n---\nRollback.",
        );
        write_skill(
            tmp.path(),
            "monitor/health.md",
            "---\nname: health\n---\nHealth.",
        );

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        store.load_all().unwrap();

        let devops = store.list_by_category("devops");
        assert_eq!(devops.len(), 2);

        let monitor = store.list_by_category("monitor");
        assert_eq!(monitor.len(), 1);

        let empty = store.list_by_category("nonexistent");
        assert!(empty.is_empty());
    }

    #[test]
    fn test_categories_summary() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a/x.md", "---\nname: x\n---\nX.");
        write_skill(tmp.path(), "a/y.md", "---\nname: y\n---\nY.");
        write_skill(tmp.path(), "b/z.md", "---\nname: z\n---\nZ.");
        write_skill(tmp.path(), "root.md", "---\nname: root\n---\nRoot.");

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        store.load_all().unwrap();

        let cats = store.categories();
        assert_eq!(cats.get("a"), Some(&2));
        assert_eq!(cats.get("b"), Some(&1));
        assert_eq!(cats.get("uncategorized"), Some(&1));
    }

    #[test]
    fn test_all_returns_everything() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "---\nname: a\n---\nA.");
        write_skill(tmp.path(), "b.md", "---\nname: b\n---\nB.");

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        store.load_all().unwrap();

        assert_eq!(store.all().len(), 2);
    }

    #[test]
    fn test_len_and_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = SkillStore::new(tmp.path().to_path_buf());
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);

        write_skill(tmp.path(), "s.md", "---\nname: s\n---\nS.");
        store.load_all().unwrap();
        assert_eq!(store.len(), 1);
        assert!(!store.is_empty());
    }

    // -----------------------------------------------------------------------
    // Parameters and tags
    // -----------------------------------------------------------------------

    #[test]
    fn test_parameters_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "search.md",
            "---\nname: search\nparameters:\n  - name: query\n    description: Terms\n    required: true\n  - count\n---\nSearch.",
        );

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        store.load_all().unwrap();

        let skill = store.get("search").unwrap();
        assert_eq!(skill.parameters.len(), 2);
        assert_eq!(skill.parameters[0].name, "query");
        assert!(skill.parameters[0].required);
        assert_eq!(skill.parameters[1].name, "count");
        assert!(!skill.parameters[1].required);
    }

    #[test]
    fn test_tags_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "deploy.md",
            "---\nname: deploy\ntags:\n  - deploy\n  - cicd\n---\nDeploy.",
        );

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        store.load_all().unwrap();

        let skill = store.get("deploy").unwrap();
        assert_eq!(skill.tags, vec!["deploy", "cicd"]);
    }

    // -----------------------------------------------------------------------
    // Hot reload
    // -----------------------------------------------------------------------

    #[test]
    fn test_reload_changed_no_changes() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "skill.md", "---\nname: test\n---\nOriginal.");

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        store.load_all().unwrap();

        let reloaded = store.reload_changed().unwrap();
        assert!(reloaded.is_empty());
    }

    #[test]
    fn test_reload_changed_modified_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_skill(tmp.path(), "skill.md", "---\nname: test\n---\nOriginal.");

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        store.load_all().unwrap();
        assert_eq!(store.get("test").unwrap().instructions, "Original.");

        // Bump mtime and overwrite.
        let new_time = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
        filetime::set_file_mtime(
            &path,
            filetime::FileTime::from_system_time(new_time),
        )
        .unwrap();
        fs::write(&path, "---\nname: test\n---\nUpdated.").unwrap();
        filetime::set_file_mtime(
            &path,
            filetime::FileTime::from_system_time(new_time),
        )
        .unwrap();

        let reloaded = store.reload_changed().unwrap();
        assert_eq!(reloaded, vec!["test"]);
        assert_eq!(store.get("test").unwrap().instructions, "Updated.");
    }

    #[test]
    fn test_reload_changed_new_file_discovered() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "first.md", "---\nname: first\n---\nFirst.");

        let mut store = SkillStore::new(tmp.path().to_path_buf());
        store.load_all().unwrap();
        assert_eq!(store.len(), 1);

        write_skill(tmp.path(), "second.md", "---\nname: second\n---\nSecond.");

        let reloaded = store.reload_changed().unwrap();
        assert!(reloaded.contains(&"second".to_string()));
        assert_eq!(store.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Root path accessor
    // -----------------------------------------------------------------------

    #[test]
    fn test_root_accessor() {
        let store = SkillStore::new(PathBuf::from("/some/path"));
        assert_eq!(store.root(), Path::new("/some/path"));
    }

    // -----------------------------------------------------------------------
    // yaml_string_array
    // -----------------------------------------------------------------------

    #[test]
    fn test_yaml_string_array_present() {
        let yaml: serde_yaml::Value =
            serde_yaml::from_str("tags:\n  - a\n  - b\n").unwrap();
        assert_eq!(yaml_string_array(&yaml, "tags"), vec!["a", "b"]);
    }

    #[test]
    fn test_yaml_string_array_missing() {
        let yaml: serde_yaml::Value = serde_yaml::from_str("name: x\n").unwrap();
        assert!(yaml_string_array(&yaml, "tags").is_empty());
    }

    // -----------------------------------------------------------------------
    // collect_md_files
    // -----------------------------------------------------------------------

    #[test]
    fn test_collect_md_files_recursive() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "root.md", "r");
        write_skill(tmp.path(), "sub/nested.md", "n");
        write_skill(tmp.path(), "sub/deep/deeper.md", "d");
        write_skill(tmp.path(), "sub/deep/readme.txt", "t");

        let files = collect_md_files(tmp.path()).unwrap();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn test_collect_md_files_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let files = collect_md_files(tmp.path()).unwrap();
        assert!(files.is_empty());
    }
}
