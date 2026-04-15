//! Learning system configuration.
//!
//! Provides [`LearningConfig`] with TOML-compatible serde support for
//! configuring the event bus, store, and processors.

use nanobot_core::{NanobotError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Default maximum number of events retained after pruning.
const DEFAULT_MAX_EVENTS: usize = 10_000;

/// Default event log file name.
const DEFAULT_EVENT_LOG_NAME: &str = "learning_events.jsonl";

/// Default processor stats file name.
const DEFAULT_STATS_FILE_NAME: &str = "processor_stats.json";

/// Configuration for the learning subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningConfig {
    /// Path to the append-only event log directory.
    ///
    /// The event log file will be created inside this directory.
    /// Defaults to `~/.nanobot-rs/learning/`.
    #[serde(default)]
    pub event_log_path: Option<PathBuf>,

    /// Maximum number of events to keep in the log before pruning.
    ///
    /// When exceeded, the oldest events are removed. Defaults to 10000.
    #[serde(default = "default_max_events")]
    pub max_events: usize,

    /// Whether event processors are enabled.
    ///
    /// When `false`, events are still published and persisted but
    /// no processing actions are generated. Defaults to `true`.
    #[serde(default = "default_true")]
    pub processors_enabled: bool,
}

fn default_max_events() -> usize {
    DEFAULT_MAX_EVENTS
}

fn default_true() -> bool {
    true
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            event_log_path: None,
            max_events: DEFAULT_MAX_EVENTS,
            processors_enabled: true,
        }
    }
}

impl LearningConfig {
    /// Creates a new config with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the effective event log file path.
    ///
    /// If `event_log_path` is set, the log file is placed inside it.
    /// Otherwise, uses `~/.nanobot-rs/learning/`.
    pub fn event_log_file(&self) -> PathBuf {
        let dir = self.effective_log_dir();
        dir.join(DEFAULT_EVENT_LOG_NAME)
    }

    /// Returns the effective log directory, falling back to the default.
    pub fn effective_log_dir(&self) -> PathBuf {
        self.event_log_path.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".nanobot-rs")
                .join("learning")
        })
    }

    /// Validates the configuration.
    pub fn validate(&self) -> Result<()> {
        if self.max_events == 0 {
            return Err(NanobotError::Config(
                "learning.max_events must be > 0".into(),
            ));
        }
        Ok(())
    }

    /// Returns the path for the processor stats file.
    ///
    /// Placed in the same directory as the event log.
    pub fn stats_file(&self) -> PathBuf {
        self.effective_log_dir().join(DEFAULT_STATS_FILE_NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = LearningConfig::default();
        assert!(config.event_log_path.is_none());
        assert_eq!(config.max_events, 10_000);
        assert!(config.processors_enabled);
    }

    #[test]
    fn validate_rejects_zero_max_events() {
        let config = LearningConfig {
            max_events: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_accepts_valid_config() {
        let config = LearningConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn event_log_file_uses_configured_path() {
        let config = LearningConfig {
            event_log_path: Some(PathBuf::from("/tmp/learning")),
            ..Default::default()
        };
        assert_eq!(
            config.event_log_file(),
            PathBuf::from("/tmp/learning/learning_events.jsonl")
        );
    }

    #[test]
    fn toml_roundtrip() {
        let config = LearningConfig {
            event_log_path: Some(PathBuf::from("/data/events")),
            max_events: 5000,
            processors_enabled: false,
        };
        let toml_str = toml::to_string(&config).expect("to_toml");
        let parsed: LearningConfig = toml::from_str(&toml_str).expect("from_toml");
        assert_eq!(parsed.event_log_path, config.event_log_path);
        assert_eq!(parsed.max_events, config.max_events);
        assert_eq!(parsed.processors_enabled, config.processors_enabled);
    }

    #[test]
    fn stats_file_uses_effective_log_dir() {
        let config = LearningConfig {
            event_log_path: Some(PathBuf::from("/tmp/learning")),
            ..Default::default()
        };
        assert_eq!(
            config.stats_file(),
            PathBuf::from("/tmp/learning/processor_stats.json")
        );
    }

    #[test]
    fn stats_file_defaults_to_event_log_directory() {
        let config = LearningConfig::default();
        assert_eq!(
            config.stats_file().parent(),
            config.event_log_file().parent()
        );
    }

    #[test]
    fn toml_missing_processors_enabled_defaults_true() {
        let parsed: LearningConfig =
            toml::from_str("event_log_path = \"/tmp/learning\"\nmax_events = 42")
                .expect("from_toml");
        assert_eq!(parsed.event_log_path, Some(PathBuf::from("/tmp/learning")));
        assert_eq!(parsed.max_events, 42);
        assert!(parsed.processors_enabled);
    }
}
