//! Skill registry — register, query, match, and persist skills.
//!
//! The registry holds compiled skills in an `Arc<RwLock<HashMap>>` and provides
//! keyword-based matching against user input. When configured with a skills directory,
//! it can also create, update, and deprecate skills on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use rand::{rng, Rng};
use tokio::sync::{Mutex, RwLock as AsyncRwLock};

use crate::error::{SkillError, SkillResult};
use crate::manifest::SkillManifestBuilder;
use crate::skill::{CompiledSkill, Skill};

/// A scored skill match returned by [`SkillRegistry::match_skills`].
#[derive(Debug, Clone)]
pub struct SkillMatch {
    /// Name of the matched skill.
    pub name: String,
    /// Match score (0.0 – 1.0).
    pub score: f64,
}

/// Central registry for loaded skills.
///
/// Thread-safe via `Arc<AsyncRwLock>`. Skills are stored by name and can be
/// queried by keyword matching against trigger lists. When a `skills_dir` is
/// configured, the registry can also create, update, and deprecate skills on disk.
#[derive(Debug, Clone)]
pub struct SkillRegistry {
    skills: Arc<AsyncRwLock<HashMap<String, Arc<RwLock<CompiledSkill>>>>>,
    mutation_lock: Arc<Mutex<()>>,
    /// Optional directory where skill TOML manifests and Markdown instructions are stored.
    skills_dir: Option<PathBuf>,
}

impl SkillRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            skills: Arc::new(AsyncRwLock::new(HashMap::new())),
            mutation_lock: Arc::new(Mutex::new(())),
            skills_dir: None,
        }
    }

    /// Configure the skills directory for write operations.
    ///
    /// Must be set before calling [`create_skill`](Self::create_skill),
    /// [`update_skill_instructions`](Self::update_skill_instructions), or
    /// [`deprecate_skill`](Self::deprecate_skill).
    pub fn with_skills_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.skills_dir = Some(dir.into());
        self
    }

    /// Return the configured skills directory, if any.
    pub fn skills_dir(&self) -> Option<&Path> {
        self.skills_dir.as_deref()
    }

    /// Register a compiled skill.
    ///
    /// Returns an error if a skill with the same name already exists.
    pub async fn register(&self, skill: CompiledSkill) -> SkillResult<()> {
        let name = skill.name().to_string();
        let mut skills = self.skills.write().await;
        if skills.contains_key(&name) {
            return Err(SkillError::AlreadyExists(name));
        }
        skills.insert(name, Arc::new(RwLock::new(skill)));
        Ok(())
    }

    /// Get a skill by name.
    pub async fn get(&self, name: &str) -> Option<Arc<RwLock<CompiledSkill>>> {
        let skills = self.skills.read().await;
        skills.get(name).cloned()
    }

    /// Remove a skill by name. Returns true if the skill was present.
    pub async fn unregister(&self, name: &str) -> bool {
        let mut skills = self.skills.write().await;
        skills.remove(name).is_some()
    }

    /// Return the number of registered skills.
    pub async fn len(&self) -> usize {
        self.skills.read().await.len()
    }

    /// Check if the registry is empty.
    pub async fn is_empty(&self) -> bool {
        self.skills.read().await.is_empty()
    }

    /// List all registered skill names.
    pub async fn skill_names(&self) -> Vec<String> {
        self.skills.read().await.keys().cloned().collect()
    }

    /// Match skills against a query string, sorted by descending score.
    ///
    /// Returns non-deprecated skills with a score > 0.0, sorted from best to worst match.
    /// Deprecated skills are excluded from results.
    pub async fn match_skills(&self, query: &str) -> Vec<SkillMatch> {
        let skills = self.skills.read().await;
        let mut matches: Vec<SkillMatch> = Vec::new();

        for (name, skill) in skills.iter() {
            let guard = skill.read();
            if guard.is_deprecated() {
                continue;
            }
            let score = guard.matches(query);
            if score > 0.0 {
                matches.push(SkillMatch {
                    name: name.clone(),
                    score,
                });
            }
        }

        matches.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        matches
    }

    /// Update the confidence of a registered skill using a feedback event.
    ///
    /// When a skills directory is configured, the updated confidence is persisted
    /// to the on-disk TOML manifest so it survives restarts.
    pub async fn update_confidence(
        &self,
        name: &str,
        event: crate::skill::ConfidenceEvent,
    ) -> SkillResult<()> {
        let (skill, new_confidence) = {
            let skills = self.skills.read().await;
            let skill = skills
                .get(name)
                .cloned()
                .ok_or_else(|| SkillError::NotFound(name.to_string()))?;
            skill.write().update_confidence(event);
            let confidence = skill.read().confidence();
            (skill, confidence)
        };

        persist_confidence(&skill, &new_confidence, self.skills_dir.as_deref(), &self.mutation_lock).await?;

        Ok(())
    }

    /// Adjust the confidence of a registered skill by a raw delta.
    ///
    /// The resulting confidence is clamped to the inclusive range `0.0..=1.0`.
    /// When a skills directory is configured, the updated confidence is persisted
    /// to the on-disk TOML manifest so it survives restarts.
    pub async fn adjust_confidence(&self, name: &str, delta: f64) -> SkillResult<()> {
        let (skill, new_confidence) = {
            let skills = self.skills.read().await;
            let skill = skills
                .get(name)
                .cloned()
                .ok_or_else(|| SkillError::NotFound(name.to_string()))?;
            let mut guard = skill.write();
            guard.confidence = (guard.confidence + delta).clamp(0.0, 1.0);
            let confidence = guard.confidence;
            drop(guard);
            (skill, confidence)
        };

        persist_confidence(&skill, &new_confidence, self.skills_dir.as_deref(), &self.mutation_lock).await?;

        Ok(())
    }

    /// Create a new skill with a TOML manifest and Markdown instructions file.
    ///
    /// Writes a `<name>.toml` manifest and a `<name>.md` instructions file to the
    /// configured skills directory using atomic writes (temp file + rename). The new
    /// skill is automatically registered in the registry.
    ///
    /// # Errors
    ///
    /// Returns [`SkillError::SkillsDirNotSet`] if no skills directory is configured,
    /// [`SkillError::AlreadyExists`] if a skill with the same name already exists,
    /// or an I/O error if writing fails.
    pub async fn create_skill(
        &self,
        name: &str,
        description: &str,
        instructions: &str,
    ) -> SkillResult<()> {
        if instructions.is_empty() {
            return Err(SkillError::ValidationFailed {
                name: name.to_string(),
                reason: "instructions must not be empty".to_string(),
            });
        }

        let dir = self
            .skills_dir
            .as_deref()
            .ok_or(SkillError::SkillsDirNotSet)?;
        let _mutation_guard = self.mutation_lock.lock().await;

        // Ensure the directory exists
        std::fs::create_dir_all(dir)?;

        // Check for duplicate
        {
            let skills = self.skills.read().await;
            if skills.contains_key(name) {
                return Err(SkillError::AlreadyExists(name.to_string()));
            }
        }

        // Derive triggers from name parts (split on hyphens and whitespace)
        let triggers: Vec<String> = name
            .split(['-', '_'])
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .chain(std::iter::once(name.to_string()))
            .collect();

        let manifest = SkillManifestBuilder::new(name, "1.0.0", description)
            .triggers(triggers)
            .build();

        // Validate before writing
        manifest
            .validate()
            .map_err(|errors| SkillError::ValidationFailed {
                name: name.to_string(),
                reason: errors.join("; "),
            })?;

        // Write TOML manifest atomically
        let toml_path = dir.join(format!("{name}.toml"));
        atomic_write(&toml_path, &toml::to_string(&manifest)?)?;

        // Write Markdown instructions atomically (only if non-empty)
        if !instructions.is_empty() {
            let md_path = dir.join(format!("{name}.md"));
            atomic_write(&md_path, instructions)?;
        }

        // Build the compiled skill only after disk writes succeed.
        let mut skill = CompiledSkill::new(manifest);
        if !instructions.is_empty() {
            skill.set_instructions(instructions.to_string());
        }
        let mut skills = self.skills.write().await;
        if skills.contains_key(name) {
            return Err(SkillError::AlreadyExists(name.to_string()));
        }
        skills.insert(name.to_string(), Arc::new(RwLock::new(skill)));

        Ok(())
    }

    /// Update the instructions content for an existing skill.
    ///
    /// Replaces the content of the companion `<name>.md` file and updates the
    /// in-memory [`CompiledSkill`]. Uses atomic write (temp file + rename).
    ///
    /// # Errors
    ///
    /// Returns [`SkillError::SkillsDirNotSet`] if no skills directory is configured,
    /// or [`SkillError::NotFound`] if no skill with the given name exists.
    pub async fn update_skill_instructions(
        &self,
        name: &str,
        new_instructions: &str,
    ) -> SkillResult<()> {
        let dir = self
            .skills_dir
            .as_deref()
            .ok_or(SkillError::SkillsDirNotSet)?;
        let _mutation_guard = self.mutation_lock.lock().await;

        let skill = {
            let skills = self.skills.read().await;
            skills
                .get(name)
                .cloned()
                .ok_or_else(|| SkillError::NotFound(name.to_string()))?
        };

        // Persist to disk before mutating the in-memory skill.
        let md_path = dir.join(format!("{name}.md"));
        atomic_write(&md_path, new_instructions)?;
        skill.write().set_instructions(new_instructions.to_string());

        Ok(())
    }

    /// Mark a skill as deprecated.
    ///
    /// Sets the `deprecated` flag and reason in both the on-disk TOML manifest
    /// and the in-memory [`CompiledSkill`]. Deprecated skills are excluded from
    /// [`match_skills`](Self::match_skills) results.
    ///
    /// # Errors
    ///
    /// Returns [`SkillError::SkillsDirNotSet`] if no skills directory is configured,
    /// or [`SkillError::NotFound`] if no skill with the given name exists.
    pub async fn deprecate_skill(&self, name: &str, reason: &str) -> SkillResult<()> {
        let dir = self
            .skills_dir
            .as_deref()
            .ok_or(SkillError::SkillsDirNotSet)?;
        let _mutation_guard = self.mutation_lock.lock().await;

        // Build the updated manifest from the current in-memory skill.
        let skill = {
            let skills = self.skills.read().await;
            skills
                .get(name)
                .cloned()
                .ok_or_else(|| SkillError::NotFound(name.to_string()))?
        };
        let updated_manifest = {
            let mut manifest = skill.read().manifest().clone();
            manifest.deprecated = Some(true);
            manifest.deprecation_reason = Some(reason.to_string());
            manifest
        };

        // Write updated manifest to disk atomically
        let toml_path = dir.join(format!("{name}.toml"));
        atomic_write(&toml_path, &toml::to_string(&updated_manifest)?)?;

        // Update in-memory skill
        let mut guard = skill.write();
        // Replace the manifest after the on-disk update succeeds, while
        // preserving runtime fields that are not stored in the manifest.
        let confidence = guard.confidence();
        let usage_count = guard.usage_count();
        let instructions = guard.instructions().to_string();
        let mut new_skill = CompiledSkill::new(updated_manifest);
        new_skill.set_instructions(instructions);
        new_skill.confidence = confidence;
        new_skill.usage_count = usage_count;
        *guard = new_skill;

        Ok(())
    }
}

/// Persist a confidence change to the on-disk TOML manifest.
///
/// No-op when `skills_dir` is `None` (registry not configured for persistence).
async fn persist_confidence(
    skill: &Arc<RwLock<CompiledSkill>>,
    confidence: &f64,
    skills_dir: Option<&Path>,
    mutation_lock: &Mutex<()>,
) -> SkillResult<()> {
    let Some(dir) = skills_dir else {
        return Ok(());
    };

    let _mutation_guard = mutation_lock.lock().await;

    let name = skill.read().name().to_string();
    let updated_manifest = {
        let mut manifest = skill.read().manifest().clone();
        manifest.confidence = Some(*confidence);
        manifest
    };

    let toml_path = dir.join(format!("{name}.toml"));
    atomic_write(&toml_path, &toml::to_string(&updated_manifest)?)?;

    Ok(())
}

/// Write data to a file atomically using temp file + rename.
///
/// Creates a temporary file in the same directory, writes the content, then
/// renames to the target path. On Unix, rename is atomic when source and
/// destination are on the same filesystem.
fn atomic_write(path: &Path, content: &str) -> SkillResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent dir"))?;
    let temp_path = unique_temp_path(parent, path);

    std::fs::write(&temp_path, content)?;
    std::fs::rename(&temp_path, path)?;

    Ok(())
}

fn unique_temp_path(parent: &Path, path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    let timestamp_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let random_suffix: u64 = rng().random();
    let temp_name = format!(
        ".{file_name}.{}.{}.{}.tmp",
        std::process::id(),
        timestamp_nanos,
        random_suffix
    );
    parent.join(temp_name)
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::SkillManifestBuilder;
    use tokio::sync::Barrier;

    fn make_skill(name: &str, triggers: &[&str]) -> CompiledSkill {
        CompiledSkill::new(
            SkillManifestBuilder::new(name, "1.0.0", format!("Skill {name}"))
                .triggers(triggers.to_vec())
                .build(),
        )
    }

    #[tokio::test]
    async fn test_register_and_get() {
        let registry = SkillRegistry::new();
        let skill = make_skill("deploy", &["deploy", "release"]);
        registry.register(skill).await.unwrap();

        let got = registry.get("deploy").await;
        assert!(got.is_some());
        assert_eq!(got.unwrap().read().name(), "deploy");
    }

    #[tokio::test]
    async fn test_register_duplicate() {
        let registry = SkillRegistry::new();
        registry.register(make_skill("dup", &["x"])).await.unwrap();
        let result = registry.register(make_skill("dup", &["x"])).await;
        assert!(matches!(result.unwrap_err(), SkillError::AlreadyExists(_)));
    }

    #[tokio::test]
    async fn test_get_missing() {
        let registry = SkillRegistry::new();
        assert!(registry.get("nope").await.is_none());
    }

    #[tokio::test]
    async fn test_unregister() {
        let registry = SkillRegistry::new();
        registry.register(make_skill("rm", &["x"])).await.unwrap();
        assert!(registry.unregister("rm").await);
        assert!(!registry.unregister("rm").await);
        assert!(registry.get("rm").await.is_none());
    }

    #[tokio::test]
    async fn test_len_and_is_empty() {
        let registry = SkillRegistry::new();
        assert!(registry.is_empty().await);
        assert_eq!(registry.len().await, 0);

        registry.register(make_skill("a", &["a"])).await.unwrap();
        assert_eq!(registry.len().await, 1);
        assert!(!registry.is_empty().await);
    }

    #[tokio::test]
    async fn test_skill_names() {
        let registry = SkillRegistry::new();
        registry
            .register(make_skill("alpha", &["a"]))
            .await
            .unwrap();
        registry.register(make_skill("beta", &["b"])).await.unwrap();
        let mut names = registry.skill_names().await;
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[tokio::test]
    async fn test_match_skills_single_hit() {
        let registry = SkillRegistry::new();
        registry
            .register(make_skill("deploy", &["deploy", "k8s"]))
            .await
            .unwrap();
        registry
            .register(make_skill("test", &["test", "unit"]))
            .await
            .unwrap();

        let matches = registry.match_skills("please deploy").await;
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "deploy");
        assert!(matches[0].score > 0.0);
    }

    #[tokio::test]
    async fn test_match_skills_multiple_hits() {
        let registry = SkillRegistry::new();
        registry
            .register(make_skill("deploy-k8s", &["deploy", "k8s"]))
            .await
            .unwrap();
        registry
            .register(make_skill("deploy-docker", &["deploy", "docker"]))
            .await
            .unwrap();

        let matches = registry.match_skills("deploy k8s").await;
        // Both match on "deploy", deploy-k8s also matches on "k8s"
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].name, "deploy-k8s");
    }

    #[tokio::test]
    async fn test_match_skills_no_hits() {
        let registry = SkillRegistry::new();
        registry
            .register(make_skill("deploy", &["deploy"]))
            .await
            .unwrap();
        let matches = registry.match_skills("run tests").await;
        assert!(matches.is_empty());
    }

    #[tokio::test]
    async fn test_match_skills_sorted_by_score() {
        let registry = SkillRegistry::new();
        // high confidence skill
        let mut s1 = make_skill("skill-a", &["search"]);
        s1.update_confidence(crate::skill::ConfidenceEvent::UsedSuccessfully);
        s1.update_confidence(crate::skill::ConfidenceEvent::UsedSuccessfully);

        // low confidence skill
        let mut s2 = make_skill("skill-b", &["search"]);
        s2.update_confidence(crate::skill::ConfidenceEvent::UsedButFailed);

        registry.register(s1).await.unwrap();
        registry.register(s2).await.unwrap();

        let matches = registry.match_skills("search").await;
        assert_eq!(matches.len(), 2);
        assert!(matches[0].score > matches[1].score);
        assert_eq!(matches[0].name, "skill-a");
    }

    #[tokio::test]
    async fn test_update_confidence() {
        let registry = SkillRegistry::new();
        registry.register(make_skill("conf", &["x"])).await.unwrap();
        registry
            .update_confidence("conf", crate::skill::ConfidenceEvent::UserConfirmed)
            .await
            .unwrap();

        let skill = registry.get("conf").await.unwrap();
        assert!(skill.read().confidence() > 0.5);
    }

    #[tokio::test]
    async fn test_update_confidence_missing() {
        let registry = SkillRegistry::new();
        let result = registry
            .update_confidence("ghost", crate::skill::ConfidenceEvent::UsedSuccessfully)
            .await;
        assert!(matches!(result.unwrap_err(), SkillError::NotFound(_)));
    }

    // --- Write method tests ---

    fn make_registry_with_dir() -> (SkillRegistry, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::new().with_skills_dir(dir.path());
        (registry, dir)
    }

    #[tokio::test]
    async fn test_create_skill_writes_files_and_registers() {
        let (registry, dir) = make_registry_with_dir();

        registry
            .create_skill(
                "my-skill",
                "Does cool things",
                "Step 1: Do this\nStep 2: Do that",
            )
            .await
            .unwrap();

        // Verify TOML file exists and is valid
        let toml_path = dir.path().join("my-skill.toml");
        assert!(toml_path.exists(), "TOML manifest should exist");
        let content = std::fs::read_to_string(&toml_path).unwrap();
        let manifest: crate::SkillManifest = toml::from_str(&content).unwrap();
        assert_eq!(manifest.name, "my-skill");
        assert_eq!(manifest.description, "Does cool things");

        // Verify MD file exists
        let md_path = dir.path().join("my-skill.md");
        assert!(md_path.exists(), "Instructions MD file should exist");
        let instructions = std::fs::read_to_string(&md_path).unwrap();
        assert!(instructions.contains("Step 1: Do this"));

        // Verify get() returns the skill
        let skill = registry.get("my-skill").await;
        assert!(skill.is_some());
        assert_eq!(skill.unwrap().read().name(), "my-skill");

        // Verify match_skills finds it
        let matches = registry.match_skills("my-skill").await;
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "my-skill");
    }

    #[tokio::test]
    async fn test_create_skill_outputs_compile_with_instructions() {
        let (registry, dir) = make_registry_with_dir();

        registry
            .create_skill(
                "compile-check",
                "Verifies compile path",
                "Detailed instructions",
            )
            .await
            .unwrap();

        let compiler = crate::compiler::SkillCompiler::new();
        let compiled = compiler
            .compile_file(&dir.path().join("compile-check.toml"))
            .unwrap();

        assert_eq!(compiled.name(), "compile-check");
        assert_eq!(compiled.description(), "Verifies compile path");
        assert_eq!(compiled.instructions(), "Detailed instructions");
    }

    #[tokio::test]
    async fn test_create_skill_duplicate_returns_error() {
        let (registry, _dir) = make_registry_with_dir();

        registry
            .create_skill("dup", "First", "Instructions")
            .await
            .unwrap();
        let result = registry.create_skill("dup", "Second", "Other").await;
        assert!(matches!(result.unwrap_err(), SkillError::AlreadyExists(_)));
    }

    #[tokio::test]
    async fn test_create_skill_concurrent_only_one_succeeds() {
        let (registry, dir) = make_registry_with_dir();
        let barrier = Arc::new(Barrier::new(3));
        let first_registry = registry.clone();
        let second_registry = registry.clone();
        let first_barrier = barrier.clone();
        let second_barrier = barrier.clone();

        let first = tokio::spawn(async move {
            first_barrier.wait().await;
            first_registry
                .create_skill("race", "First attempt", "Instructions")
                .await
        });
        let second = tokio::spawn(async move {
            second_barrier.wait().await;
            second_registry
                .create_skill("race", "Second attempt", "Instructions")
                .await
        });

        barrier.wait().await;

        let first_result = first.await.unwrap();
        let second_result = second.await.unwrap();
        let success_count = usize::from(first_result.is_ok()) + usize::from(second_result.is_ok());
        let already_exists_count = usize::from(matches!(
            first_result.as_ref().err(),
            Some(SkillError::AlreadyExists(_))
        )) + usize::from(matches!(
            second_result.as_ref().err(),
            Some(SkillError::AlreadyExists(_))
        ));

        assert_eq!(success_count, 1);
        assert_eq!(already_exists_count, 1);
        assert_eq!(registry.len().await, 1);
        assert!(dir.path().join("race.toml").exists());
    }

    #[tokio::test]
    async fn test_create_skill_no_dir_returns_error() {
        let registry = SkillRegistry::new();
        let result = registry.create_skill("x", "y", "z").await;
        assert!(matches!(result.unwrap_err(), SkillError::SkillsDirNotSet));
    }

    #[tokio::test]
    async fn test_create_skill_rejects_empty_instructions() {
        let (registry, dir) = make_registry_with_dir();

        let result = registry
            .create_skill("no-md", "No instructions", "")
            .await;

        assert!(matches!(result.unwrap_err(), SkillError::ValidationFailed { .. }));

        // Nothing should be written to disk
        assert!(!dir.path().join("no-md.toml").exists());
        assert!(!dir.path().join("no-md.md").exists());
    }

    #[tokio::test]
    async fn test_update_skill_instructions_modifies_file_and_memory() {
        let (registry, dir) = make_registry_with_dir();

        registry
            .create_skill("updatable", "A skill", "Original instructions")
            .await
            .unwrap();

        // Update instructions
        registry
            .update_skill_instructions("updatable", "Updated instructions here")
            .await
            .unwrap();

        // Verify file content updated
        let md_path = dir.path().join("updatable.md");
        let content = std::fs::read_to_string(&md_path).unwrap();
        assert_eq!(content, "Updated instructions here");

        // Verify in-memory updated
        let skill = registry.get("updatable").await.unwrap();
        assert_eq!(skill.read().instructions(), "Updated instructions here");
    }

    #[tokio::test]
    async fn test_update_skill_instructions_failed_disk_write_leaves_memory_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let invalid_skills_dir = dir.path().join("not-a-directory");
        std::fs::write(&invalid_skills_dir, "blocking file").unwrap();
        let registry = SkillRegistry::new().with_skills_dir(&invalid_skills_dir);
        let mut skill = make_skill("stays-put", &["stays"]);
        skill.set_instructions("Original instructions".to_string());
        registry.register(skill).await.unwrap();

        let result = registry
            .update_skill_instructions("stays-put", "Updated instructions")
            .await;

        assert!(matches!(result.unwrap_err(), SkillError::Io(_)));
        let skill = registry.get("stays-put").await.unwrap();
        assert_eq!(skill.read().instructions(), "Original instructions");
    }

    #[tokio::test]
    async fn test_update_skill_instructions_not_found() {
        let (registry, _dir) = make_registry_with_dir();

        let result = registry
            .update_skill_instructions("ghost", "new content")
            .await;
        assert!(matches!(result.unwrap_err(), SkillError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_update_skill_instructions_no_dir() {
        let registry = SkillRegistry::new();
        registry.register(make_skill("x", &["x"])).await.unwrap();
        let result = registry.update_skill_instructions("x", "y").await;
        assert!(matches!(result.unwrap_err(), SkillError::SkillsDirNotSet));
    }

    #[tokio::test]
    async fn test_deprecate_skill_filters_from_matching() {
        let (registry, _dir) = make_registry_with_dir();

        registry
            .create_skill("old-skill", "An old skill", "Old instructions")
            .await
            .unwrap();

        // Verify it matches before deprecation
        let matches = registry.match_skills("old-skill").await;
        assert_eq!(matches.len(), 1);

        // Deprecate
        registry
            .deprecate_skill("old-skill", "Replaced by new-skill")
            .await
            .unwrap();

        // Verify match_skills no longer returns it
        let matches = registry.match_skills("old-skill").await;
        assert!(matches.is_empty(), "Deprecated skill should not match");

        // Verify still accessible via get()
        let skill = registry.get("old-skill").await;
        assert!(
            skill.is_some(),
            "Deprecated skill should still be retrievable"
        );
        assert!(skill.unwrap().read().is_deprecated());
    }

    #[tokio::test]
    async fn test_deprecate_skill_updates_manifest_on_disk() {
        let (registry, dir) = make_registry_with_dir();

        registry
            .create_skill("dep-test", "To deprecate", "Instructions")
            .await
            .unwrap();

        registry
            .deprecate_skill("dep-test", "Obsolete")
            .await
            .unwrap();

        // Verify TOML manifest on disk has deprecated flag
        let toml_path = dir.path().join("dep-test.toml");
        let content = std::fs::read_to_string(&toml_path).unwrap();
        let manifest: crate::SkillManifest = toml::from_str(&content).unwrap();
        assert_eq!(manifest.deprecated, Some(true));
        assert_eq!(manifest.deprecation_reason.as_deref(), Some("Obsolete"));
    }

    #[tokio::test]
    async fn test_deprecate_skill_preserves_confidence_and_usage() {
        let (registry, _dir) = make_registry_with_dir();

        registry
            .create_skill("preserve", "Test", "Instructions")
            .await
            .unwrap();

        // Boost confidence
        registry
            .update_confidence("preserve", crate::skill::ConfidenceEvent::UserConfirmed)
            .await
            .unwrap();
        let pre_confidence = registry.get("preserve").await.unwrap().read().confidence();

        registry.deprecate_skill("preserve", "Old").await.unwrap();

        let skill = registry.get("preserve").await.unwrap();
        assert_eq!(skill.read().confidence(), pre_confidence);
    }

    #[tokio::test]
    async fn test_deprecate_skill_not_found() {
        let (registry, _dir) = make_registry_with_dir();
        let result = registry.deprecate_skill("ghost", "reason").await;
        assert!(matches!(result.unwrap_err(), SkillError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_deprecate_skill_no_dir() {
        let registry = SkillRegistry::new();
        registry.register(make_skill("x", &["x"])).await.unwrap();
        let result = registry.deprecate_skill("x", "old").await;
        assert!(matches!(result.unwrap_err(), SkillError::SkillsDirNotSet));
    }

    #[test]
    fn test_atomic_write_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        atomic_write(&path, "hello world").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
        // No temp file left behind
        assert!(!dir.path().join(".test.toml.tmp").exists());
    }

    #[test]
    fn test_atomic_write_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overwrite.toml");
        atomic_write(&path, "first").unwrap();
        atomic_write(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }

    #[test]
    fn test_unique_temp_path_changes_between_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unique.toml");

        let first = unique_temp_path(dir.path(), &path);
        let second = unique_temp_path(dir.path(), &path);

        assert_ne!(first, second);
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".unique.toml."));
        assert!(second
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".unique.toml."));
    }

    #[tokio::test]
    async fn test_match_skills_filters_deprecated_skill() {
        let registry = SkillRegistry::new();
        let mut skill = make_skill("active", &["search"]);
        skill.update_confidence(crate::skill::ConfidenceEvent::UsedSuccessfully);
        let manifest = SkillManifestBuilder::new("deprecated-skill", "1.0.0", "Old skill")
            .triggers(["search"])
            .deprecated("No longer used")
            .build();
        let deprecated = CompiledSkill::new(manifest);

        registry.register(skill).await.unwrap();
        registry.register(deprecated).await.unwrap();

        let matches = registry.match_skills("search").await;
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "active");
    }

    #[test]
    fn test_skills_dir_accessor() {
        let registry = SkillRegistry::new();
        assert!(registry.skills_dir().is_none());

        let registry = SkillRegistry::new().with_skills_dir("/tmp/skills");
        assert_eq!(registry.skills_dir(), Some(Path::new("/tmp/skills")));
    }

    #[tokio::test]
    async fn test_adjust_confidence_persists_to_disk() {
        let (registry, dir) = make_registry_with_dir();

        registry
            .create_skill("persist-skill", "Test persistence", "Instructions")
            .await
            .unwrap();

        registry
            .adjust_confidence("persist-skill", 0.3)
            .await
            .unwrap();

        // Verify TOML manifest on disk has the updated confidence
        let toml_path = dir.path().join("persist-skill.toml");
        let content = std::fs::read_to_string(&toml_path).unwrap();
        let manifest: crate::SkillManifest = toml::from_str(&content).unwrap();
        assert!(
            manifest.confidence.is_some(),
            "confidence should be persisted in manifest"
        );
        let persisted = manifest.confidence.unwrap();
        assert!(
            (persisted - 0.8).abs() < f64::EPSILON,
            "expected confidence ~0.8, got {persisted}"
        );

        // Verify in-memory skill matches
        let skill = registry.get("persist-skill").await.unwrap();
        let mem_confidence = skill.read().confidence();
        assert!(
            (mem_confidence - persisted).abs() < f64::EPSILON,
            "in-memory confidence {mem_confidence} should match persisted {persisted}"
        );
    }

    #[tokio::test]
    async fn test_adjust_confidence_survives_reload() {
        let (registry, dir) = make_registry_with_dir();

        registry
            .create_skill("reload-skill", "Test reload", "Instructions")
            .await
            .unwrap();

        registry
            .adjust_confidence("reload-skill", 0.4)
            .await
            .unwrap();

        // Simulate a restart: create a fresh registry and reload from disk
        let new_registry = SkillRegistry::new().with_skills_dir(dir.path());
        let loader = crate::loader::SkillLoader::new(
            crate::config::SkillConfig::default().with_skills_dir(dir.path()),
        );
        loader
            .load_all(&new_registry)
            .await
            .expect("reload should succeed");

        let skill = new_registry
            .get("reload-skill")
            .await
            .expect("skill should be loaded");
        let confidence = skill.read().confidence();
        assert!(
            (confidence - 0.9).abs() < f64::EPSILON,
            "confidence should survive reload, expected ~0.9, got {confidence}"
        );
    }

    #[tokio::test]
    async fn test_update_confidence_persists_to_disk() {
        let (registry, dir) = make_registry_with_dir();

        registry
            .create_skill("event-skill", "Test event confidence", "Instructions")
            .await
            .unwrap();

        registry
            .update_confidence("event-skill", crate::skill::ConfidenceEvent::UserConfirmed)
            .await
            .unwrap();

        // Verify TOML manifest on disk has updated confidence
        let toml_path = dir.path().join("event-skill.toml");
        let content = std::fs::read_to_string(&toml_path).unwrap();
        let manifest: crate::SkillManifest = toml::from_str(&content).unwrap();
        assert!(manifest.confidence.is_some());
        let persisted = manifest.confidence.unwrap();
        assert!(
            persisted > 0.5,
            "UserConfirmed should increase confidence above default, got {persisted}"
        );
    }

    #[tokio::test]
    async fn test_adjust_confidence_no_dir_still_updates_memory() {
        let registry = SkillRegistry::new();
        registry
            .register(make_skill("no-dir", &["x"]))
            .await
            .unwrap();

        // Should not error — just skips disk persistence
        registry
            .adjust_confidence("no-dir", 0.2)
            .await
            .unwrap();

        let skill = registry.get("no-dir").await.unwrap();
        assert!(
            skill.read().confidence() > 0.5,
            "in-memory confidence should still be updated"
        );
    }
}
