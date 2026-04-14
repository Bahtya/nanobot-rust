//! Error types for the nanobot-skill crate.

use thiserror::Error;

/// Errors that can occur during skill operations.
#[derive(Error, Debug)]
pub enum SkillError {
    /// A required field is missing from the TOML manifest.
    #[error("Missing required field '{field}' in skill manifest '{name}'")]
    MissingField {
        /// Name of the skill manifest.
        name: String,
        /// Name of the missing field.
        field: String,
    },

    /// The TOML manifest could not be parsed.
    #[error("Failed to parse TOML manifest '{path}': {source}")]
    ParseFailed {
        /// Path to the manifest file.
        path: String,
        /// The parse error.
        source: toml::de::Error,
    },

    /// A skill with the given name was not found.
    #[error("Skill not found: {0}")]
    NotFound(String),

    /// A skill with the given name already exists.
    #[error("Skill already registered: {0}")]
    AlreadyExists(String),

    /// An I/O error occurred.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// The skill manifest validation failed.
    #[error("Invalid skill manifest '{name}': {reason}")]
    ValidationFailed {
        /// Name of the skill manifest.
        name: String,
        /// Why validation failed.
        reason: String,
    },

    /// Skill execution failed.
    #[error("Skill execution failed for '{name}': {reason}")]
    ExecutionFailed {
        /// Name of the skill.
        name: String,
        /// Why execution failed.
        reason: String,
    },

    /// The skill directory does not exist.
    #[error("Skills directory not found: {0}")]
    DirectoryNotFound(String),
}

/// Convenience alias for results using [`SkillError`].
pub type SkillResult<T> = std::result::Result<T, SkillError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_missing_field_display() {
        let err = SkillError::MissingField {
            name: "deploy".to_string(),
            field: "version".to_string(),
        };
        assert!(format!("{err}").contains("version"));
        assert!(format!("{err}").contains("deploy"));
    }

    #[test]
    fn test_parse_failed_display() {
        let toml_err = toml::from_str::<toml::Value>("not valid [[[").unwrap_err();
        let err = SkillError::ParseFailed {
            path: "skills/deploy.toml".to_string(),
            source: toml_err,
        };
        assert!(format!("{err}").contains("skills/deploy.toml"));
    }

    #[test]
    fn test_not_found_display() {
        let err = SkillError::NotFound("my-skill".to_string());
        assert!(format!("{err}").contains("my-skill"));
    }

    #[test]
    fn test_already_exists_display() {
        let err = SkillError::AlreadyExists("dup".to_string());
        assert!(format!("{err}").contains("dup"));
    }

    #[test]
    fn test_validation_failed_display() {
        let err = SkillError::ValidationFailed {
            name: "bad".to_string(),
            reason: "name too long".to_string(),
        };
        assert!(format!("{err}").contains("name too long"));
    }

    #[test]
    fn test_execution_failed_display() {
        let err = SkillError::ExecutionFailed {
            name: "boom".to_string(),
            reason: "timeout".to_string(),
        };
        assert!(format!("{err}").contains("timeout"));
    }

    #[test]
    fn test_directory_not_found_display() {
        let err = SkillError::DirectoryNotFound("/tmp/nope".to_string());
        assert!(format!("{err}").contains("/tmp/nope"));
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let err: SkillError = io_err.into();
        assert!(matches!(err, SkillError::Io(_)));
    }

    #[test]
    fn test_result_alias() {
        let ok: SkillResult<i32> = Ok(42);
        assert_eq!(ok.unwrap(), 42);

        let err: SkillResult<String> = Err(SkillError::NotFound("x".to_string()));
        assert!(err.is_err());
    }
}
