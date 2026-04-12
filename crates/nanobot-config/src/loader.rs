//! Configuration loading with env var expansion and migration support.

use crate::migration::migrate_config;
use crate::paths::get_config_path;
use crate::schema::Config;
use anyhow::{Context, Result};
use regex::Regex;
use std::path::Path;
use tracing::{debug, info};

/// Load configuration from the default location.
///
/// If no config file exists, creates a default one.
/// Applies environment variable expansion and migrations.
pub fn load_config(config_path: Option<&Path>) -> Result<Config> {
    let path = match config_path {
        Some(p) => p.to_path_buf(),
        None => get_config_path()?,
    };

    let config = if path.exists() {
        info!("Loading config from {}", path.display());
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let expanded = expand_env_vars(&raw);
        let mut config: Config = serde_yaml::from_str(&expanded)
            .with_context(|| format!("Failed to parse config YAML: {}", path.display()))?;

        migrate_config(&mut config)?;
        config
    } else {
        info!("No config file found at {}, using defaults", path.display());
        Config::default()
    };

    debug!("Config loaded successfully");
    Ok(config)
}

/// Expand `${VAR}` and `${VAR:-default}` patterns in a string.
///
/// Matches the Python `_resolve_config_env_vars()` behavior.
pub fn expand_env_vars(input: &str) -> String {
    let re = Regex::new(r"\$\{([^}]+)\}").unwrap();
    re.replace_all(input, |caps: &regex::Captures| {
        let expr = &caps[1];
        if let Some((var, default)) = expr.split_once(":-") {
            std::env::var(var).unwrap_or_else(|_| default.to_string())
        } else {
            std::env::var(expr).unwrap_or_else(|_| String::new())
        }
    })
    .into_owned()
}

/// Save configuration to a YAML file.
pub fn save_config(config: &Config, path: &Path) -> Result<()> {
    let yaml = serde_yaml::to_string(config)?;
    std::fs::write(path, yaml)
        .with_context(|| format!("Failed to write config to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_env_vars_simple() {
        std::env::set_var("TEST_NANOBOT_KEY", "secret123");
        let result = expand_env_vars("key: ${TEST_NANOBOT_KEY}");
        assert_eq!(result, "key: secret123");
    }

    #[test]
    fn test_expand_env_vars_with_default() {
        let result = expand_env_vars("key: ${NONEXISTENT_VAR:-fallback}");
        assert_eq!(result, "key: fallback");
    }

    #[test]
    fn test_expand_env_vars_missing_no_default() {
        let result = expand_env_vars("key: ${NONEXISTENT_VAR_NO_DEFAULT}");
        assert_eq!(result, "key: ");
    }

    #[test]
    fn test_default_config_roundtrip() {
        let config = Config::default();
        let yaml = serde_yaml::to_string(&config).unwrap();
        let parsed: Config = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.agent.model, config.agent.model);
    }

    #[test]
    fn test_save_and_load_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.yaml");

        let config = Config::default();
        save_config(&config, &config_path).unwrap();
        assert!(config_path.exists());

        let loaded = load_config(Some(&config_path)).unwrap();
        assert_eq!(loaded.agent.model, config.agent.model);
        assert_eq!(loaded.agent.temperature, config.agent.temperature);
    }

    #[test]
    fn test_load_config_missing_file_returns_default() {
        let config = load_config(Some(std::path::Path::new(
            "/tmp/nonexistent_nanobot_config_9999.yaml",
        )))
        .unwrap();
        // Should return default config
        assert!(!config.agent.model.is_empty());
    }

    #[test]
    fn test_load_config_with_env_vars() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.yaml");

        std::env::set_var("TEST_NANOBOT_LOAD_KEY", "my-secret-key");

        let yaml_content = r#"
providers:
  openai:
    api_key: ${TEST_NANOBOT_LOAD_KEY}
    model: gpt-4o
agent:
  model: ${NONEXISTENT_MODEL:-gpt-4o}
"#;
        std::fs::write(&config_path, yaml_content).unwrap();

        let config = load_config(Some(&config_path)).unwrap();
        assert_eq!(
            config.providers.openai.as_ref().unwrap().api_key.as_deref(),
            Some("my-secret-key")
        );
        assert_eq!(config.agent.model, "gpt-4o");

        std::env::remove_var("TEST_NANOBOT_LOAD_KEY");
    }

    #[test]
    fn test_expand_env_vars_multiple() {
        std::env::set_var("TEST_VAR_A", "alpha");
        std::env::set_var("TEST_VAR_B", "beta");

        let result = expand_env_vars("a=${TEST_VAR_A}, b=${TEST_VAR_B}, c=${MISSING:-gamma}");
        assert_eq!(result, "a=alpha, b=beta, c=gamma");

        std::env::remove_var("TEST_VAR_A");
        std::env::remove_var("TEST_VAR_B");
    }
}
