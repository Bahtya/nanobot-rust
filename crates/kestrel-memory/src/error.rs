//! Memory-specific error types.

use thiserror::Error;

/// Errors that can occur in memory operations.
#[derive(Error, Debug)]
pub enum MemoryError {
    /// An I/O error occurred during persistence.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// A serialization/deserialization error occurred.
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// A memory entry was not found.
    #[error("Memory entry not found: {0}")]
    NotFound(String),

    /// The memory store has reached its capacity limit.
    #[error("Memory store capacity exceeded: max {max}, current {current}")]
    CapacityExceeded {
        /// Maximum allowed entries.
        max: usize,
        /// Current number of entries.
        current: usize,
    },

    /// An invalid embedding vector was provided.
    #[error("Invalid embedding: expected dimension {expected}, got {actual}")]
    InvalidEmbedding {
        /// Expected embedding dimension.
        expected: usize,
        /// Actual embedding dimension provided.
        actual: usize,
    },

    /// A configuration error occurred.
    #[error("Configuration error: {0}")]
    Config(String),

    /// A search engine error occurred.
    #[error("Search engine error: {0}")]
    SearchEngine(String),

    /// A security violation was detected in a memory entry.
    #[error("Security violation: {0}")]
    SecurityViolation(String),

    /// A concurrent write conflict occurred.
    #[error("Concurrent write conflict: {0}")]
    ConcurrentWrite(String),
}

/// Convenience type alias for Results using MemoryError.
pub type Result<T> = std::result::Result<T, MemoryError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = MemoryError::NotFound("entry-123".to_string());
        assert!(err.to_string().contains("entry-123"));

        let err = MemoryError::CapacityExceeded {
            max: 100,
            current: 100,
        };
        assert!(err.to_string().contains("100"));

        let err = MemoryError::InvalidEmbedding {
            expected: 1536,
            actual: 512,
        };
        assert!(err.to_string().contains("1536"));
        assert!(err.to_string().contains("512"));

        let err = MemoryError::Config("bad config".to_string());
        assert!(err.to_string().contains("bad config"));

        let err = MemoryError::SearchEngine("index not found".to_string());
        assert!(err.to_string().contains("index not found"));
    }

    #[test]
    fn test_security_violation_display() {
        let err = MemoryError::SecurityViolation(
            "Prompt injection pattern detected: \"jailbreak\"".to_string(),
        );
        let msg = err.to_string();
        assert!(msg.contains("Security violation"));
        assert!(msg.contains("jailbreak"));
    }

    #[test]
    fn test_concurrent_write_display() {
        let err = MemoryError::ConcurrentWrite("lock acquisition failed".to_string());
        let msg = err.to_string();
        assert!(msg.contains("Concurrent write conflict"));
        assert!(msg.contains("lock acquisition failed"));
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err: MemoryError = io_err.into();
        assert!(matches!(err, MemoryError::Io(_)));
    }

    #[test]
    fn test_result_alias() {
        let ok: Result<i32> = Ok(42);
        assert!(matches!(ok, Ok(42)));

        let err: Result<String> = Err(MemoryError::NotFound("x".to_string()));
        assert!(err.is_err());
    }

    #[test]
    fn test_result_with_security_violation() {
        let err: Result<()> = Err(MemoryError::SecurityViolation("bad input".to_string()));
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("Security violation"));
    }

    #[test]
    fn test_result_with_concurrent_write() {
        let err: Result<()> = Err(MemoryError::ConcurrentWrite("conflict".to_string()));
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("Concurrent write conflict"));
    }
}
