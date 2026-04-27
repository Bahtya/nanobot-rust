//! Shared cancellation registry for agent run interruption.
//!
//! Provides a `CancelRegistry` that both the agent loop and channel
//! adapters can access. When a user sends `/stop`, the channel adapter
//! looks up the session's cancellation token and triggers it.

use dashmap::DashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Shared registry mapping session keys to cancellation tokens.
#[derive(Debug, Clone, Default)]
pub struct CancelRegistry {
    tokens: Arc<DashMap<String, CancellationToken>>,
}

impl CancelRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a cancellation token for a session.
    pub fn insert(&self, session_key: String, token: CancellationToken) {
        self.tokens.insert(session_key, token);
    }

    /// Remove and return the cancellation token for a session.
    pub fn remove(&self, session_key: &str) -> Option<CancellationToken> {
        self.tokens.remove(session_key).map(|(_, v)| v)
    }

    /// Cancel the running agent for a session. Returns true if a token was found.
    pub fn cancel(&self, session_key: &str) -> bool {
        if let Some(token) = self.tokens.get(session_key) {
            token.cancel();
            true
        } else {
            false
        }
    }
}
