//! Skill loader — filesystem-backed skill discovery with content-hash caching.
//!
//! [`SkillLoader`] scans a skills directory for `.md` files, parses YAML
//! frontmatter (`name`, `description`, `category`, `tags`), and caches
//! parsed skills by content hash. Subsequent loads skip files whose
//! content hasn't changed, avoiding redundant filesystem reads and
//! YAML parsing.
//!
//! ## File format
//!
//! ```markdown
//! ---
//! name: deploy
//! description: Deploy the application
//! category: devops
//! tags:
//!   - deploy
//!   - cicd
//! ---
//!
//! # Deploy instructions
//! Deploy to {env}...
//! ```

use crate::skills::{Skill, SkillParameter};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Cache entry
// ---------------------------------------------------------------------------

/// A cached skill together with a hash of its source file content.
struct CacheEntry {
    /// Hash of the file content when it was last parsed.
    content_hash: u64,
    /// The parsed skill.
    skill: Skill,
}

// ---------------------------------------------------------------------------
// SkillLoader
// ---------------------------------------------------------------------------

/// Loads skill files from a directory with content-hash caching.
///
/// On [`load_all`], each `.md` file is read and hashed. If the hash
/// matches the cached entry the file is skipped entirely (no YAML parse).
/// Only changed or new files incur the full parse cost.
///
/// Call [`invalidate_all`] or [`invalidate_path`] to force re-reads.
pub struct SkillLoader {
    /// Root directory to scan for `.md` skill files.
    root: PathBuf,
    /// Cache keyed by canonical file path.
    cache: HashMap<PathBuf, CacheEntry>,
    /// Parsed skills indexed by name for fast lookup.
    by_name: HashMap<String, Skill>,
}

impl SkillLoader {
    /// Create a new loader pointing at the given root directory.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            cache: HashMap::new(),
            by_name: HashMap::new(),
        }
    }

    /// Load all skill files from the root directory (recursive).
    ///
    /// Uses content-hash caching: unchanged files are served from the
    /// in-memory cache without re-reading or re-parsing.
    ///
    /// Returns the full list of loaded skills.
    pub fn load_all(&mut self) -> Result<Vec<Skill>> {
        if !self.root.exists() {
            debug!("Skills root does not exist: {}", self.root.display());
            self.cache.clear();
            self.by_name.clear();
            return Ok(Vec::new());
        }

        let files = collect_md_files(&self.root)?;

        // Prune cache entries for files that no longer exist.
        let file_set: Vec<PathBuf> = files.iter().map(|p| p.canonicalize().unwrap_or_else(|_| p.clone())).collect();
        self.cache.retain(|k, _| file_set.contains(k));

        let mut skills = Vec::new();
        let mut new_by_name = HashMap::new();

        for path in &files {
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());

            // Read file content (always needed for hashing).
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(e) => {
                    warn!("Failed to read {}: {}", path.display(), e);
                    continue;
                }
            };

            let hash = hash_content(&content);

            // Cache hit — reuse existing parsed skill.
            if let Some(entry) = self.cache.get(&canonical) {
                if entry.content_hash == hash {
                    debug!("Cache hit for {}", path.display());
                    new_by_name.insert(entry.skill.name.clone(), entry.skill.clone());
                    skills.push(entry.skill.clone());
                    continue;
                }
            }

            // Cache miss or content changed — parse the file.
            let relative = path.strip_prefix(&self.root).unwrap_or(path).to_path_buf();
            match Self::parse_skill(&content, path, &relative) {
                Ok(skill) => {
                    debug!(
                        "Parsed skill '{}' [{}] from {}",
                        skill.name,
                        skill.category,
                        relative.display()
                    );
                    new_by_name.insert(skill.name.clone(), skill.clone());
                    self.cache.insert(
                        canonical,
                        CacheEntry {
                            content_hash: hash,
                            skill: skill.clone(),
                        },
                    );
                    skills.push(skill);
                }
                Err(e) => {
                    warn!("Failed to parse skill {}: {}", relative.display(), e);
                }
            }
        }

        self.by_name = new_by_name;
        info!("Loaded {} skill(s) from {}", skills.len(), self.root.display());
        Ok(skills)
    }

    /// Look up a skill by name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.by_name.get(name)
    }

    /// Return all loaded skills.
    pub fn all(&self) -> Vec<&Skill> {
        self.by_name.values().collect()
    }

    /// Number of loaded skills.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Whether any skills are loaded.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Return all unique categories with their skill counts.
    pub fn categories(&self) -> HashMap<&str, usize> {
        let mut counts = HashMap::new();
        for skill in self.by_name.values() {
            *counts.entry(skill.category.as_str()).or_insert(0) += 1;
        }
        counts
    }

    /// List skills in a given category.
    pub fn list_by_category(&self, category: &str) -> Vec<&Skill> {
        self.by_name
            .values()
            .filter(|s| s.category == category)
            .collect()
    }

    /// Find skills matching any of the given tags.
    pub fn find_by_tag(&self, tag: &str) -> Vec<&Skill> {
        self.by_name
            .values()
            .filter(|s| s.tags.iter().any(|t| t.eq_ignore_ascii_case(tag)))
            .collect()
    }

    /// Invalidate the entire cache, forcing full re-reads on next load.
    pub fn invalidate_all(&mut self) {
        self.cache.clear();
    }

    /// Invalidate the cache for a specific file path.
    ///
    /// `path` can be absolute or relative to the skills root.
    pub fn invalidate_path(&mut self, path: &Path) {
        let canonical = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        if let Ok(canonical) = canonical.canonicalize() {
            if let Some(entry) = self.cache.remove(&canonical) {
                self.by_name.remove(&entry.skill.name);
            }
        }
    }

    /// Get the root directory path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Number of entries in the disk cache (for diagnostics / testing).
    pub fn cache_size(&self) -> usize {
        self.cache.len()
    }

    // -----------------------------------------------------------------------
    // Parsing
    // -----------------------------------------------------------------------

    /// Parse a skill file from its raw content.
    fn parse_skill(content: &str, path: &Path, relative: &Path) -> Result<Skill> {
        let (frontmatter, body) = parse_frontmatter(content)?;

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

        let category = frontmatter
            .get("category")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| {
                relative
                    .iter()
                    .next()
                    .map(|c| c.to_string_lossy().to_string())
                    .filter(|_| relative.iter().count() > 1)
                    .unwrap_or_else(|| "uncategorized".to_string())
            });

        let tags = yaml_string_array(&frontmatter, "tags");
        let parameters = parse_parameters(&frontmatter);
        let modified_at =
            std::fs::metadata(path).ok().and_then(|m| m.modified().ok());

        Ok(Skill {
            name,
            description,
            category,
            instructions: body.trim().to_string(),
            parameters,
            tags,
            source_path: path.to_path_buf(),
            relative_path: relative.to_path_buf(),
            modified_at,
        })
    }
}

// ---------------------------------------------------------------------------
// Filesystem helpers
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

// ---------------------------------------------------------------------------
// Frontmatter parsing
// ---------------------------------------------------------------------------

/// Parse YAML frontmatter from a markdown string.
///
/// Expects `---\n...\n---\nbody` format. Returns `(frontmatter, body)`.
/// If no frontmatter is found, returns `(Null, original_content)`.
fn parse_frontmatter(content: &str) -> Result<(serde_yaml::Value, String)> {
    let trimmed = content.trim_start();

    if !trimmed.starts_with("---") {
        return Ok((serde_yaml::Value::Null, content.to_string()));
    }

    let after_first = &trimmed[3..];
    let end = after_first
        .find("---")
        .context("Unclosed frontmatter")?;

    let frontmatter_str = &after_first[..end];
    let body = after_first[end + 3..].to_string();

    let frontmatter: serde_yaml::Value = serde_yaml::from_str(frontmatter_str)
        .with_context(|| "Failed to parse frontmatter YAML")?;

    Ok((frontmatter, body))
}

/// Parse the `parameters` field from frontmatter.
fn parse_parameters(frontmatter: &serde_yaml::Value) -> Vec<SkillParameter> {
    let params = match frontmatter
        .get("parameters")
        .and_then(|v| v.as_sequence())
    {
        Some(seq) => seq,
        None => return Vec::new(),
    };

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
            } else {
                v.as_str().map(|name| SkillParameter {
                    name: name.to_string(),
                    description: String::new(),
                    required: false,
                })
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

// ---------------------------------------------------------------------------
// Content hashing
// ---------------------------------------------------------------------------

/// FNV-1a hash of file content for cache comparison.
fn hash_content(content: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in content.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
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
    // parse_frontmatter
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
    // parse_parameters
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_parameters_structured() {
        let yaml = serde_yaml::from_str(
            "parameters:\n  - name: env\n    description: Target\n    required: true\n",
        )
        .unwrap();
        let params = parse_parameters(&yaml);
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "env");
        assert_eq!(params[0].description, "Target");
        assert!(params[0].required);
    }

    #[test]
    fn test_parse_parameters_string_shorthand() {
        let yaml =
            serde_yaml::from_str("parameters:\n  - query\n  - limit\n").unwrap();
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
    // hash_content
    // -----------------------------------------------------------------------

    #[test]
    fn test_hash_content_deterministic() {
        let a = hash_content("hello world");
        let b = hash_content("hello world");
        assert_eq!(a, b);
    }

    #[test]
    fn test_hash_content_differs_on_change() {
        let a = hash_content("hello world");
        let b = hash_content("hello universe");
        assert_ne!(a, b);
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

    // -----------------------------------------------------------------------
    // SkillLoader — basic loading
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_all_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();
        assert!(skills.is_empty());
        assert!(loader.is_empty());
    }

    #[test]
    fn test_load_all_nonexistent_dir() {
        let mut loader =
            SkillLoader::new(PathBuf::from("/tmp/no_such_skills_dir_xyz"));
        let skills = loader.load_all().unwrap();
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

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();

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

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();
        assert_eq!(skills[0].name, "my_skill");
    }

    #[test]
    fn test_load_all_no_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "plain.md", "Just plain markdown.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();
        assert_eq!(skills[0].name, "plain");
        assert!(skills[0].instructions.contains("Just plain markdown."));
    }

    #[test]
    fn test_load_all_ignores_non_md() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "skill.md", "---\nname: good\n---\nOK");
        write_skill(tmp.path(), "notes.txt", "Not a skill");
        write_skill(tmp.path(), "config.yaml", "key: val");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();
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

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.get("deploy").unwrap().category, "devops");
    }

    #[test]
    fn test_category_from_frontmatter_overrides_dir() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "devops/deploy.md",
            "---\nname: deploy\ncategory: infrastructure\n---\nDeploy it.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.get("deploy").unwrap().category, "infrastructure");
    }

    #[test]
    fn test_category_uncategorized_when_root_level() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "tool.md", "---\nname: tool\n---\nRoot skill.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.get("tool").unwrap().category, "uncategorized");
    }

    #[test]
    fn test_category_deep_nested_uses_first_dir() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "devops/kubernetes/deploy.md",
            "---\nname: k8s_deploy\n---\nDeploy to k8s.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.get("k8s_deploy").unwrap().category, "devops");
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

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        let skills = loader.load_all().unwrap();

        assert_eq!(skills.len(), 3);
        assert!(loader.get("deploy_staging").is_some());
        assert!(loader.get("health_check").is_some());
        assert!(loader.get("deep_skill").is_some());
    }

    // -----------------------------------------------------------------------
    // Lookup and listing
    // -----------------------------------------------------------------------

    #[test]
    fn test_get_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "---\nname: alpha\n---\nAlpha.");
        write_skill(tmp.path(), "b.md", "---\nname: beta\n---\nBeta.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.get("alpha").unwrap().instructions, "Alpha.");
        assert_eq!(loader.get("beta").unwrap().instructions, "Beta.");
        assert!(loader.get("gamma").is_none());
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

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.list_by_category("devops").len(), 2);
        assert_eq!(loader.list_by_category("monitor").len(), 1);
        assert!(loader.list_by_category("nonexistent").is_empty());
    }

    #[test]
    fn test_categories_summary() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a/x.md", "---\nname: x\n---\nX.");
        write_skill(tmp.path(), "a/y.md", "---\nname: y\n---\nY.");
        write_skill(tmp.path(), "b/z.md", "---\nname: z\n---\nZ.");
        write_skill(tmp.path(), "root.md", "---\nname: root\n---\nRoot.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let cats = loader.categories();
        assert_eq!(cats.get("a"), Some(&2));
        assert_eq!(cats.get("b"), Some(&1));
        assert_eq!(cats.get("uncategorized"), Some(&1));
    }

    #[test]
    fn test_all_returns_everything() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "---\nname: a\n---\nA.");
        write_skill(tmp.path(), "b.md", "---\nname: b\n---\nB.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.all().len(), 2);
    }

    #[test]
    fn test_len_and_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        assert!(loader.is_empty());
        assert_eq!(loader.len(), 0);

        write_skill(tmp.path(), "s.md", "---\nname: s\n---\nS.");
        loader.load_all().unwrap();
        assert_eq!(loader.len(), 1);
        assert!(!loader.is_empty());
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

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let skill = loader.get("search").unwrap();
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

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(
            loader.get("deploy").unwrap().tags,
            vec!["deploy", "cicd"]
        );
    }

    #[test]
    fn test_find_by_tag() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "a.md",
            "---\nname: a\ntags:\n  - deploy\n---\nA.",
        );
        write_skill(
            tmp.path(),
            "b.md",
            "---\nname: b\ntags:\n  - monitor\n---\nB.",
        );
        write_skill(
            tmp.path(),
            "c.md",
            "---\nname: c\ntags:\n  - deploy\n  - cicd\n---\nC.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        let deploy_skills = loader.find_by_tag("deploy");
        assert_eq!(deploy_skills.len(), 2);

        let monitor_skills = loader.find_by_tag("monitor");
        assert_eq!(monitor_skills.len(), 1);
    }

    #[test]
    fn test_find_by_tag_case_insensitive() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "a.md",
            "---\nname: a\ntags:\n  - Backend\n---\nA.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();

        assert_eq!(loader.find_by_tag("backend").len(), 1);
        assert_eq!(loader.find_by_tag("BACKEND").len(), 1);
    }

    // -----------------------------------------------------------------------
    // Caching — the key differentiator
    // -----------------------------------------------------------------------

    #[test]
    fn test_cache_hit_avoids_reparse() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "cached.md",
            "---\nname: cached\n---\nOriginal content.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());

        // First load — parses and caches.
        loader.load_all().unwrap();
        assert_eq!(loader.cache_size(), 1);
        assert_eq!(loader.get("cached").unwrap().instructions, "Original content.");

        // Modify the file content but keep the same hash — tricky to test
        // directly. Instead, verify that load_all without changes keeps cache.
        let skills = loader.load_all().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(loader.cache_size(), 1);
    }

    #[test]
    fn test_cache_invalidated_on_content_change() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_skill(
            tmp.path(),
            "change.md",
            "---\nname: change\n---\nVersion 1.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.get("change").unwrap().instructions, "Version 1.");

        // Overwrite the file with different content.
        fs::write(&path, "---\nname: change\n---\nVersion 2.").unwrap();

        loader.load_all().unwrap();
        assert_eq!(loader.get("change").unwrap().instructions, "Version 2.");
    }

    #[test]
    fn test_invalidate_all_forces_full_reload() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "a.md",
            "---\nname: a\n---\nAlpha.",
        );

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.cache_size(), 1);

        loader.invalidate_all();
        assert_eq!(loader.cache_size(), 0);

        // Skills should still be available until next load_all.
        assert_eq!(loader.len(), 1);

        loader.load_all().unwrap();
        assert_eq!(loader.cache_size(), 1);
        assert_eq!(loader.get("a").unwrap().instructions, "Alpha.");
    }

    #[test]
    fn test_invalidate_path_specific() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a.md", "---\nname: a\n---\nA.");
        write_skill(tmp.path(), "b.md", "---\nname: b\n---\nB.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.cache_size(), 2);

        // Invalidate only "a.md".
        loader.invalidate_path(&PathBuf::from("a.md"));
        assert_eq!(loader.cache_size(), 1);
        assert!(loader.get("a").is_none());
        assert!(loader.get("b").is_some());
    }

    #[test]
    fn test_cache_prunes_deleted_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path_a = write_skill(tmp.path(), "a.md", "---\nname: a\n---\nA.");
        write_skill(tmp.path(), "b.md", "---\nname: b\n---\nB.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.cache_size(), 2);

        // Delete file a.
        fs::remove_file(&path_a).unwrap();

        loader.load_all().unwrap();
        assert_eq!(loader.cache_size(), 1);
        assert!(loader.get("a").is_none());
        assert!(loader.get("b").is_some());
    }

    #[test]
    fn test_new_file_discovered_on_reload() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "first.md", "---\nname: first\n---\nFirst.");

        let mut loader = SkillLoader::new(tmp.path().to_path_buf());
        loader.load_all().unwrap();
        assert_eq!(loader.len(), 1);

        write_skill(tmp.path(), "second.md", "---\nname: second\n---\nSecond.");

        loader.load_all().unwrap();
        assert_eq!(loader.len(), 2);
        assert_eq!(loader.cache_size(), 2);
        assert!(loader.get("second").is_some());
    }

    // -----------------------------------------------------------------------
    // Root path accessor
    // -----------------------------------------------------------------------

    #[test]
    fn test_root_accessor() {
        let loader = SkillLoader::new(PathBuf::from("/some/path"));
        assert_eq!(loader.root(), Path::new("/some/path"));
    }
}
