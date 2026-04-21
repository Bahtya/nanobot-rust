//! Constants for the kestrel project.

/// Current version of kestrel.
pub const VERSION: &str = "0.1.1";

/// Default maximum iterations for the agent loop.
pub const DEFAULT_MAX_ITERATIONS: usize = 50;

/// Default temperature for LLM calls.
pub const DEFAULT_TEMPERATURE: f32 = 0.7;

/// Default maximum tokens for LLM responses.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Maximum tool output length (characters).
pub const MAX_TOOL_OUTPUT_LENGTH: usize = 100_000;

/// Default heartbeat interval in seconds.
pub const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 1800; // 30 minutes

/// Default dream interval in seconds.
pub const DEFAULT_DREAM_INTERVAL_SECS: u64 = 7200; // 2 hours

/// Default session history limit (number of messages).
pub const DEFAULT_SESSION_HISTORY_LIMIT: usize = 200;

/// Bus channel capacity.
pub const BUS_CHANNEL_CAPACITY: usize = 1024;

/// Cron tick interval in seconds.
pub const CRON_TICK_INTERVAL_SECS: u64 = 30;

/// Maximum cron run history per job.
pub const MAX_CRON_RUN_HISTORY: usize = 20;

/// Streaming chunk interval in milliseconds.
pub const STREAMING_CHUNK_INTERVAL_MS: u64 = 100;

/// Default tool execution timeout in seconds.
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 120;

/// Default context window in tokens (approximate).
pub const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 128_000;

/// Fraction of context window at which compaction triggers.
pub const COMPACTION_THRESHOLD_RATIO: f64 = 0.8;

/// Number of recent messages to keep during compaction.
pub const COMPACTION_KEEP_RECENT: usize = 10;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_not_empty() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn test_defaults_sensible() {
        // Iterations should be positive and reasonable
        assert!(DEFAULT_MAX_ITERATIONS > 0);
        assert!(DEFAULT_MAX_ITERATIONS <= 500);

        // Temperature between 0 and 2
        assert!(DEFAULT_TEMPERATURE > 0.0);
        assert!(DEFAULT_TEMPERATURE <= 2.0);

        // Max tokens positive
        assert!(DEFAULT_MAX_TOKENS > 0);
        assert!(DEFAULT_MAX_TOKENS <= 128_000);

        // Tool output length positive
        assert!(MAX_TOOL_OUTPUT_LENGTH > 0);

        // Heartbeat interval: at least 60 seconds
        assert!(DEFAULT_HEARTBEAT_INTERVAL_SECS >= 60);

        // Dream interval: at least 60 seconds
        assert!(DEFAULT_DREAM_INTERVAL_SECS >= 60);

        // Session history limit positive
        assert!(DEFAULT_SESSION_HISTORY_LIMIT > 0);

        // Bus channel capacity positive
        assert!(BUS_CHANNEL_CAPACITY > 0);

        // Cron tick interval positive
        assert!(CRON_TICK_INTERVAL_SECS > 0);

        // Max cron run history positive
        assert!(MAX_CRON_RUN_HISTORY > 0);

        // Streaming chunk interval positive
        assert!(STREAMING_CHUNK_INTERVAL_MS > 0);

        // Tool timeout positive
        assert!(DEFAULT_TOOL_TIMEOUT_SECS > 0);
    }
}
