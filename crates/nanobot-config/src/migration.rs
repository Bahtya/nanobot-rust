//! Config version migration support.
//!
//! Handles backward compatibility when the config format changes between versions.

use crate::schema::Config;
use anyhow::Result;

/// Apply any necessary migrations to the config.
pub fn migrate_config(config: &mut Config) -> Result<()> {
    let version = config._config_version.unwrap_or(0);

    // Migration from version 0 (no version field) to version 1
    if version < 1 {
        tracing::debug!("Migrating config from unversioned to v1");
        // No-op for now, but this is where format changes would go
    }

    // Ensure the config version is up to date
    config._config_version = Some(CURRENT_CONFIG_VERSION);
    Ok(())
}

/// Current config version.
const CURRENT_CONFIG_VERSION: u64 = 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migrate_config_from_v0() {
        let mut config = Config::default();
        config._config_version = None;
        migrate_config(&mut config).expect("migration should succeed");
        assert_eq!(config._config_version, Some(4));
    }

    #[test]
    fn test_migrate_config_already_current() {
        let mut config = Config::default();
        config._config_version = Some(4);
        let agent_model_before = config.agent.model.clone();
        migrate_config(&mut config).expect("migration should succeed");
        assert_eq!(config._config_version, Some(4));
        assert_eq!(config.agent.model, agent_model_before);
    }
}
