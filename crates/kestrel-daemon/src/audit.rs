//! Lightweight JSONL audit log for key daemon events.
//!
//! Appends one JSON line per event to `kestrel.audit.jsonl` in the log directory.
//! Records message processing, skill invocations, and errors — providing a
//! structured audit trail alongside the rolling text/JSON log.

use serde::Serialize;
use std::io::Write;
use std::path::Path;

/// A single audit event written as one JSON line.
#[derive(Debug, Serialize)]
pub struct AuditEvent {
    /// ISO 8601 timestamp.
    pub timestamp: String,
    /// Event type (e.g. "message_received", "message_completed", "skill_started", "error").
    pub event_type: String,
    /// Optional trace ID for correlation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Optional session key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    /// Optional channel name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Optional duration in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Human-readable message or description.
    pub message: String,
}

/// Append an audit event to the JSONL file in `log_dir`.
///
/// Creates the file if it doesn't exist. Each call appends one line.
/// Errors are logged via `tracing::warn` but not propagated — audit
/// logging must not break the agent loop.
pub fn append_audit_event(log_dir: &str, event: &AuditEvent) {
    let path = Path::new(log_dir).join("kestrel.audit.jsonl");

    let line = match serde_json::to_string(event) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("Failed to serialize audit event: {}", e);
            return;
        }
    };

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut file) => {
            if let Err(e) = writeln!(file, "{}", line) {
                tracing::warn!("Failed to write audit event: {}", e);
            }
        }
        Err(e) => {
            tracing::warn!("Failed to open audit log {:?}: {}", path, e);
        }
    }
}

/// Create a timestamped audit event with the current UTC time.
pub fn audit_event(
    event_type: &str,
    trace_id: Option<String>,
    session_key: Option<String>,
    channel: Option<String>,
    duration_ms: Option<u64>,
    message: String,
) -> AuditEvent {
    AuditEvent {
        timestamp: chrono::Utc::now().to_rfc3339(),
        event_type: event_type.to_string(),
        trace_id,
        session_key,
        channel,
        duration_ms,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_append_audit_event_creates_file() {
        let tmp = TempDir::new().unwrap();
        let log_dir = tmp.path().to_str().unwrap();

        let event = audit_event(
            "message_received",
            Some("trace-123".to_string()),
            Some("session-1".to_string()),
            Some("telegram".to_string()),
            None,
            "User sent hello".to_string(),
        );

        append_audit_event(log_dir, &event);

        let content = std::fs::read_to_string(tmp.path().join("kestrel.audit.jsonl")).unwrap();
        assert!(content.contains("message_received"));
        assert!(content.contains("trace-123"));
        assert!(content.contains("telegram"));
        assert!(content.contains("User sent hello"));
    }

    #[test]
    fn test_append_multiple_events() {
        let tmp = TempDir::new().unwrap();
        let log_dir = tmp.path().to_str().unwrap();

        append_audit_event(
            log_dir,
            &audit_event(
                "message_received",
                None,
                None,
                None,
                None,
                "first".to_string(),
            ),
        );
        append_audit_event(
            log_dir,
            &audit_event(
                "message_completed",
                None,
                None,
                None,
                Some(150),
                "second".to_string(),
            ),
        );

        let content = std::fs::read_to_string(tmp.path().join("kestrel.audit.jsonl")).unwrap();
        let lines: Vec<&str> = content.trim().lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("first"));
        assert!(lines[1].contains("second"));
        assert!(lines[1].contains("150"));
    }

    #[test]
    fn test_audit_event_serializes_optional_fields() {
        let event = audit_event(
            "error",
            None,
            None,
            None,
            None,
            "something broke".to_string(),
        );
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("trace_id"));
        assert!(!json.contains("session_key"));
        assert!(!json.contains("channel"));
        assert!(!json.contains("duration_ms"));
        assert!(json.contains("error"));
    }

    #[test]
    fn test_append_audit_event_nonexistent_dir() {
        // Should not panic — just log a warning
        append_audit_event(
            "/tmp/nonexistent_kestrel_audit_test",
            &audit_event("test", None, None, None, None, "ok".to_string()),
        );
    }
}
