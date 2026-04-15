//! Skill system configuration.
//!
//! Loaded from `~/.nanobot-rs/skills.toml` (or `NANOBOT_RS_HOME/skills.toml`).
//! Controls skills directory, max loaded skills, and cache TTL.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration for the skill subsystem.
///
/// Example TOML (`skills.toml`):
/// ```toml
/// skills_dir = "~/.nanobot-rs/skills"
/// max_skills = 100
/// cache_ttl_secs = 3600
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillConfig {
    /// Directory containing skill TOML manifests.
    ///
    /// Defaults to `$NANOBOT_RS_HOME/skills` or `~/.nanobot-rs/skills`.
    #[serde(default = "default_skills_dir")]
    pub skills_dir: PathBuf,

    /// Maximum number of skills to keep loaded at once.
    #[serde(default = "default_max_skills")]
    pub max_skills: usize,

    /// Cache time-to-live in seconds for loaded manifests.
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_secs: u64,
}

fn default_skills_dir() -> PathBuf {
    nanobot_home().join("skills")
}

fn nanobot_home() -> PathBuf {
    std::env::var("NANOBOT_RS_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".nanobot-rs"))
}

fn default_max_skills() -> usize {
    100
}

fn default_cache_ttl() -> u64 {
    3600
}

impl Default for SkillConfig {
    fn default() -> Self {
        Self {
            skills_dir: default_skills_dir(),
            max_skills: default_max_skills(),
            cache_ttl_secs: default_cache_ttl(),
        }
    }
}

impl SkillConfig {
    /// Create config with a specific skills directory (useful for testing).
    pub fn with_skills_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.skills_dir = dir.into();
        self
    }

    /// Load config from a TOML file.
    pub fn load_from_file(path: &std::path::Path) -> crate::SkillResult<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            crate::SkillError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Failed to read config from {}: {e}", path.display()),
            ))
        })?;
        let config: Self =
            toml::from_str(&content).map_err(|e| crate::SkillError::ParseFailed {
                path: path.display().to_string(),
                source: e,
            })?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = SkillConfig::default();
        assert!(config.skills_dir.to_string_lossy().contains("skills"));
        assert_eq!(config.max_skills, 100);
        assert_eq!(config.cache_ttl_secs, 3600);
    }

    #[test]
    fn test_with_skills_dir() {
        let config = SkillConfig::default().with_skills_dir("/tmp/test-skills");
        assert_eq!(config.skills_dir, PathBuf::from("/tmp/test-skills"));
    }

    #[test]
    fn test_load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skills.toml");
        std::fs::write(
            &path,
            r#"
skills_dir = "/custom/skills"
max_skills = 50
cache_ttl_secs = 1800
"#,
        )
        .unwrap();
        let config = SkillConfig::load_from_file(&path).unwrap();
        assert_eq!(config.skills_dir, PathBuf::from("/custom/skills"));
        assert_eq!(config.max_skills, 50);
        assert_eq!(config.cache_ttl_secs, 1800);
    }

    #[test]
    fn test_load_missing_file() {
        let config = SkillConfig::load_from_file(std::path::Path::new("/nonexistent/skills.toml"));
        assert!(config.is_err());
    }

    #[test]
    fn test_load_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skills.toml");
        std::fs::write(&path, "not valid toml [[[[").unwrap();
        let result = SkillConfig::load_from_file(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_toml_roundtrip() {
        let config = SkillConfig {
            skills_dir: PathBuf::from("/tmp/skills"),
            max_skills: 42,
            cache_ttl_secs: 600,
        };
        let toml_str = toml::to_string(&config).unwrap();
        let parsed: SkillConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config, parsed);
    }
}
