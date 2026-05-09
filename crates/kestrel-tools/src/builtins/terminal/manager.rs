//! Terminal session registry and lifecycle manager.

use crate::builtins::terminal::session::{SessionInfo, TerminalSession};
use anyhow::{Context, Result};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::{debug, info};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Manages multiple PTY terminal sessions.
///
/// Thread-safe via `parking_lot::RwLock`. Sessions are identified by
/// auto-generated string IDs (`"ts-1"`, `"ts-2"`, ...).
///
/// Sessions are stored as `Arc<TerminalSession>` so async operations can
/// release the registry lock before awaiting on session-level I/O.
pub struct TerminalManager {
    sessions: RwLock<HashMap<String, Arc<TerminalSession>>>,
}

impl TerminalManager {
    /// Create a new empty session manager.
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
        }
    }

    /// Spawn a new terminal session.
    ///
    /// Returns the session ID on success.
    pub fn create_session(
        &self,
        shell: Option<String>,
        cwd: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> Result<String> {
        let id = format!("ts-{}", SESSION_COUNTER.fetch_add(1, Ordering::Relaxed));

        let session = TerminalSession::spawn(id.clone(), shell, cwd, cols, rows)
            .with_context(|| format!("Failed to spawn terminal session '{}'", id))?;

        self.sessions
            .write()
            .insert(id.clone(), Arc::new(session));
        info!("Created terminal session '{}'", id);
        Ok(id)
    }

    /// Send input (keystrokes) to a session.
    pub async fn send_input(&self, session_id: &str, input: &str) -> Result<()> {
        let session = {
            let sessions = self.sessions.read();
            sessions
                .get(session_id)
                .cloned()
                .context(format!("Session '{}' not found", session_id))?
        };
        session.send_input(input).await
    }

    /// Read output from a session.
    pub async fn read_output(
        &self,
        session_id: &str,
        timeout_ms: Option<u64>,
    ) -> Result<String> {
        let session = {
            let sessions = self.sessions.read();
            sessions
                .get(session_id)
                .cloned()
                .context(format!("Session '{}' not found", session_id))?
        };
        session.read_output(timeout_ms).await
    }

    /// List all sessions with their metadata.
    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.read();
        sessions.values().map(|s| s.info()).collect()
    }

    /// Kill and remove a session.
    pub fn kill_session(&self, session_id: &str) -> Result<()> {
        let session = {
            let mut sessions = self.sessions.write();
            sessions
                .remove(session_id)
                .context(format!("Session '{}' not found", session_id))?
        };
        session.kill();
        info!("Killed terminal session '{}'", session_id);
        Ok(())
    }

    /// Resize a session's PTY.
    pub fn resize_session(&self, session_id: &str, cols: u16, rows: u16) -> Result<()> {
        let sessions = self.sessions.read();
        let session = sessions
            .get(session_id)
            .context(format!("Session '{}' not found", session_id))?;
        debug!(session_id = session_id, cols = cols, rows = rows, "Resizing terminal session via manager");
        session.resize(cols, rows)
    }

    /// Get a session's info.
    pub fn get_session_info(&self, session_id: &str) -> Option<SessionInfo> {
        let sessions = self.sessions.read();
        sessions.get(session_id).map(|s| s.info())
    }

    /// Number of active sessions.
    pub fn len(&self) -> usize {
        self.sessions.read().len()
    }

    /// Whether there are no sessions.
    pub fn is_empty(&self) -> bool {
        self.sessions.read().is_empty()
    }
}

impl Default for TerminalManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manager_new_is_empty() {
        let mgr = TerminalManager::new();
        assert!(mgr.is_empty());
        assert_eq!(mgr.len(), 0);
    }

    #[test]
    fn test_manager_default() {
        let mgr = TerminalManager::default();
        assert!(mgr.is_empty());
    }

    #[test]
    fn test_list_sessions_empty() {
        let mgr = TerminalManager::new();
        assert!(mgr.list_sessions().is_empty());
    }

    #[tokio::test]
    async fn test_kill_nonexistent_session() {
        let mgr = TerminalManager::new();
        let result = mgr.kill_session("nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_send_input_nonexistent_session() {
        let mgr = TerminalManager::new();
        let result = mgr.send_input("nonexistent", "echo hi\n").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_output_nonexistent_session() {
        let mgr = TerminalManager::new();
        let result = mgr.read_output("nonexistent", None).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_resize_nonexistent_session() {
        let mgr = TerminalManager::new();
        let result = mgr.resize_session("nonexistent", 120, 40);
        assert!(result.is_err());
    }
}
