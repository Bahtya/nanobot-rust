//! Skill loader — directory scanning, lazy loading, and caching.
//!
//! Scans a directory for `.toml` skill manifests, compiles them on first access,
//! and caches the results with a configurable TTL.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use crate::skill::Skill;

use crate::compiler::SkillCompiler;
use crate::config::SkillConfig;
use crate::error::{SkillError, SkillResult};
use crate::registry::SkillRegistry;
use crate::skill::CompiledSkill;

/// Cache entry with expiration tracking.
#[derive(Debug)]
struct CacheEntry {
    /// The compiled skill.
    skill: CompiledSkill,
    /// When the skill was loaded/last refreshed.
    loaded_at: Instant,
    /// Path to the source manifest.
    #[allow(dead_code)]
    source: PathBuf,
}

impl CacheEntry {
    fn new(skill: CompiledSkill, source: PathBuf) -> Self {
        Self {
            skill,
            loaded_at: Instant::now(),
            source,
        }
    }

    fn is_expired(&self, ttl: Duration) -> bool {
        self.loaded_at.elapsed() > ttl
    }
}

/// Loads skill manifests from a directory with lazy loading and TTL-based caching.
#[derive(Debug)]
pub struct SkillLoader {
    compiler: SkillCompiler,
    config: SkillConfig,
    cache: Arc<RwLock<HashMap<String, CacheEntry>>>,
}

impl SkillLoader {
    /// Create a new loader with the given config.
    pub fn new(config: SkillConfig) -> Self {
        Self {
            compiler: SkillCompiler::new(),
            config,
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Load all `.toml` skill manifests from the configured skills directory
    /// and register them in the given registry.
    ///
    /// Returns the list of skill names that were successfully loaded.
    pub async fn load_all(&self, registry: &SkillRegistry) -> SkillResult<Vec<String>> {
        let skills_dir = &self.config.skills_dir;
        self.load_from_dir(skills_dir, registry).await
    }

    /// Load all `.toml` skill manifests from a specific directory.
    pub async fn load_from_dir(
        &self,
        dir: &Path,
        registry: &SkillRegistry,
    ) -> SkillResult<Vec<String>> {
        if !dir.exists() {
            return Err(SkillError::DirectoryNotFound(dir.display().to_string()));
        }

        let mut loaded = Vec::new();
        let entries = std::fs::read_dir(dir).map_err(|e| {
            SkillError::DirectoryNotFound(format!("{}: {e}", dir.display()))
        })?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                match self.load_skill(&path) {
                    Ok(skill) => {
                        let name = skill.name().to_string();
                        if let Err(e) = registry.register(skill).await {
                            tracing::warn!("Skipping skill from {}: {e}", path.display());
                            // Store in cache anyway for get() access
                            continue;
                        }
                        loaded.push(name);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to load skill from {}: {e}", path.display());
                    }
                }
            }
        }

        Ok(loaded)
    }

    /// Load a single skill, using the cache if fresh or recompiling if expired.
    pub fn load_skill(&self, path: &Path) -> SkillResult<CompiledSkill> {
        let key = path.display().to_string();

        // Check cache first
        {
            let cache = self.cache.read();
            if let Some(entry) = cache.get(&key) {
                if !entry.is_expired(Duration::from_secs(self.config.cache_ttl_secs)) {
                    return Ok(entry.skill.clone());
                }
            }
        }

        // Compile fresh
        let skill = self.compiler.compile_file(path)?;
        let entry = CacheEntry::new(skill.clone(), path.to_path_buf());

        {
            let mut cache = self.cache.write();
            cache.insert(key, entry);
        }

        Ok(skill)
    }

    /// Force a reload, bypassing the cache.
    pub fn reload_skill(&self, path: &Path) -> SkillResult<CompiledSkill> {
        let skill = self.compiler.compile_file(path)?;
        let key = path.display().to_string();
        let entry = CacheEntry::new(skill.clone(), path.to_path_buf());

        {
            let mut cache = self.cache.write();
            cache.insert(key, entry);
        }

        Ok(skill)
    }

    /// Clear the entire cache.
    pub fn clear_cache(&self) {
        self.cache.write().clear();
    }

    /// Return the number of cached entries.
    pub fn cache_size(&self) -> usize {
        self.cache.read().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::SkillManifestBuilder;

    fn test_config(dir: &Path) -> SkillConfig {
        SkillConfig::default().with_skills_dir(dir)
    }

    fn write_skill(dir: &Path, name: &str, triggers: &[&str]) -> PathBuf {
        let manifest = SkillManifestBuilder::new(name, "1.0.0", format!("Skill {name}"))
            .triggers(triggers.to_vec())
            .build();
        let path = dir.join(format!("{name}.toml"));
        std::fs::write(&path, toml::to_string(&manifest).unwrap()).unwrap();
        path
    }

    #[tokio::test]
    async fn test_load_all_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "deploy", &["deploy"]);
        write_skill(dir.path(), "test", &["test"]);

        let loader = SkillLoader::new(test_config(dir.path()));
        let registry = SkillRegistry::new();
        let loaded = loader.load_all(&registry).await.unwrap();

        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains(&"deploy".to_string()));
        assert!(loaded.contains(&"test".to_string()));
    }

    #[tokio::test]
    async fn test_load_missing_dir() {
        let loader = SkillLoader::new(SkillConfig::default().with_skills_dir("/nonexistent"));
        let registry = SkillRegistry::new();
        let result = loader.load_all(&registry).await;
        assert!(matches!(result.unwrap_err(), SkillError::DirectoryNotFound(_)));
    }

    #[test]
    fn test_load_single_skill() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(dir.path(), "single", &["s"]);

        let loader = SkillLoader::new(test_config(dir.path()));
        let skill = loader.load_skill(&path).unwrap();
        assert_eq!(skill.name(), "single");
    }

    #[test]
    fn test_load_skill_caches() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(dir.path(), "cached", &["c"]);

        let loader = SkillLoader::new(test_config(dir.path()));
        let _ = loader.load_skill(&path).unwrap();
        assert_eq!(loader.cache_size(), 1);

        // Second load hits cache
        let skill = loader.load_skill(&path).unwrap();
        assert_eq!(skill.name(), "cached");
        assert_eq!(loader.cache_size(), 1);
    }

    #[test]
    fn test_reload_skill_bypasses_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(dir.path(), "reload", &["r"]);

        let loader = SkillLoader::new(test_config(dir.path()));
        let _ = loader.load_skill(&path).unwrap();

        let skill = loader.reload_skill(&path).unwrap();
        assert_eq!(skill.name(), "reload");
    }

    #[test]
    fn test_clear_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_skill(dir.path(), "clear", &["x"]);

        let loader = SkillLoader::new(test_config(dir.path()));
        let _ = loader.load_skill(&path).unwrap();
        assert_eq!(loader.cache_size(), 1);

        loader.clear_cache();
        assert_eq!(loader.cache_size(), 0);
    }

    #[tokio::test]
    async fn test_load_skips_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "valid", &["v"]);
        // Write an invalid TOML file
        std::fs::write(dir.path().join("bad.toml"), "not valid toml [[[[").unwrap();
        // Write a non-TOML file (should be skipped)
        std::fs::write(dir.path().join("readme.md"), "# Skills").unwrap();

        let loader = SkillLoader::new(test_config(dir.path()));
        let registry = SkillRegistry::new();
        let loaded = loader.load_all(&registry).await.unwrap();

        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains(&"valid".to_string()));
    }

    #[test]
    fn test_load_invalid_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "not valid toml [[[[ ").unwrap();

        let loader = SkillLoader::new(test_config(dir.path()));
        let result = loader.load_skill(&path);
        assert!(result.is_err());
    }
}
