//! Error types for the kestrel project.

use thiserror::Error;

/// Core error type for kestrel operations.
#[derive(Error, Debug)]
pub enum KestrelError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Provider error: {0}")]
    Provider(String),

    #[error("Tool error: {0}")]
    Tool(String),

    #[error("Session error: {0}")]
    Session(String),

    #[error("Channel error: {0}")]
    Channel(String),

    #[error("Security error: {0}")]
    Security(String),

    #[error("Bus error: {0}")]
    Bus(String),

    #[error("Cron error: {0}")]
    Cron(String),

    #[error("Agent error: {0}")]
    Agent(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("YAML error: {0}")]
    Yaml(String),

    #[error("HTTP error: {0}")]
    Http(String),

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("Max iterations reached")]
    MaxIterations,

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Tool not found: {0}")]
    ToolNotFound(String),

    #[error("Provider not found: {0}")]
    ProviderNotFound(String),

    #[error("Channel not found: {0}")]
    ChannelNotFound(String),

    #[error("Memory error: {0}")]
    Memory(String),
}

/// Convenience type alias for Results using KestrelError.
pub type Result<T> = std::result::Result<T, KestrelError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_variants() {
        let config_err = KestrelError::Config("bad config".to_string());
        assert_eq!(format!("{}", config_err), "Configuration error: bad config");

        let provider_err = KestrelError::Provider("no provider".to_string());
        assert!(format!("{}", provider_err).contains("Provider error"));

        let tool_err = KestrelError::Tool("tool failed".to_string());
        assert!(format!("{}", tool_err).contains("Tool error"));

        let session_err = KestrelError::Session("no session".to_string());
        assert!(format!("{}", session_err).contains("Session error"));

        let channel_err = KestrelError::Channel("channel down".to_string());
        assert!(format!("{}", channel_err).contains("Channel error"));

        let security_err = KestrelError::Security("blocked".to_string());
        assert!(format!("{}", security_err).contains("Security error"));

        let bus_err = KestrelError::Bus("bus full".to_string());
        assert!(format!("{}", bus_err).contains("Bus error"));

        let cron_err = KestrelError::Cron("cron stuck".to_string());
        assert!(format!("{}", cron_err).contains("Cron error"));

        let agent_err = KestrelError::Agent("agent lost".to_string());
        assert!(format!("{}", agent_err).contains("Agent error"));

        let yaml_err = KestrelError::Yaml("parse fail".to_string());
        assert!(format!("{}", yaml_err).contains("YAML error"));

        let http_err = KestrelError::Http("503".to_string());
        assert!(format!("{}", http_err).contains("HTTP error"));

        let timeout_err = KestrelError::Timeout("30s".to_string());
        assert!(format!("{}", timeout_err).contains("Timeout"));

        let max_iter_err = KestrelError::MaxIterations;
        assert!(format!("{}", max_iter_err).contains("Max iterations"));

        let session_not_found = KestrelError::SessionNotFound("abc".to_string());
        assert!(format!("{}", session_not_found).contains("Session not found"));

        let tool_not_found = KestrelError::ToolNotFound("xyz".to_string());
        assert!(format!("{}", tool_not_found).contains("Tool not found"));

        let provider_not_found = KestrelError::ProviderNotFound("p".to_string());
        assert!(format!("{}", provider_not_found).contains("Provider not found"));

        let channel_not_found = KestrelError::ChannelNotFound("c".to_string());
        assert!(format!("{}", channel_not_found).contains("Channel not found"));
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err: KestrelError = io_err.into();
        assert!(matches!(err, KestrelError::Io(_)));
        assert!(format!("{}", err).contains("file missing"));
    }

    #[test]
    fn test_from_serde_error() {
        let serde_err = serde_json::from_str::<i32>("not a number").unwrap_err();
        let err: KestrelError = serde_err.into();
        assert!(matches!(err, KestrelError::Serialization(_)));
    }

    #[test]
    fn test_result_alias() {
        let ok_val: Result<i32> = Ok(42);
        assert!(matches!(ok_val, Ok(42)));

        let err_val: Result<String> = Err(KestrelError::Config("test".to_string()));
        assert!(err_val.is_err());
    }
}
