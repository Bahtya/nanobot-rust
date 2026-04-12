//! Heartbeat health check types and status tracking.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

/// Status of an individual health check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Check passed.
    Healthy,
    /// Check failed.
    Unhealthy,
    /// Check was skipped (component not configured).
    Skipped,
}

/// Result of a single health check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckResult {
    /// Name of the component checked.
    pub component: String,
    /// Check status.
    pub status: CheckStatus,
    /// Human-readable detail message.
    pub message: String,
    /// When this check ran.
    pub timestamp: DateTime<Local>,
}

/// Overall health snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HealthSnapshot {
    /// Individual check results.
    pub checks: Vec<HealthCheckResult>,
    /// Whether the overall system is healthy (all checks Healthy or Skipped).
    pub healthy: bool,
    /// When this snapshot was taken.
    pub timestamp: DateTime<Local>,
}

impl HealthSnapshot {
    /// Build a snapshot from a list of check results.
    pub fn from_checks(checks: Vec<HealthCheckResult>) -> Self {
        let healthy = checks.iter().all(|c| c.status != CheckStatus::Unhealthy);
        Self {
            checks,
            healthy,
            timestamp: Local::now(),
        }
    }

    /// Count failed checks.
    pub fn failed_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Unhealthy)
            .count()
    }
}

/// Persistent heartbeat state (written to disk).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HeartbeatState {
    /// Last known health snapshot.
    pub last_snapshot: Option<HealthSnapshot>,
    /// Total checks performed.
    pub total_checks: usize,
    /// Total failures seen.
    pub total_failures: usize,
    /// Number of component restarts requested.
    pub restarts_requested: usize,
    /// When the service was last started.
    #[serde(default)]
    pub started_at: Option<DateTime<Local>>,
    /// When the service was last stopped.
    #[serde(default)]
    pub stopped_at: Option<DateTime<Local>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_status_serde() {
        for status in &[CheckStatus::Healthy, CheckStatus::Unhealthy, CheckStatus::Skipped] {
            let json = serde_json::to_string(status).unwrap();
            let back: CheckStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(*status, back);
        }
    }

    #[test]
    fn test_health_check_result_construction() {
        let result = HealthCheckResult {
            component: "provider".to_string(),
            status: CheckStatus::Healthy,
            message: "reachable".to_string(),
            timestamp: Local::now(),
        };
        assert_eq!(result.component, "provider");
        assert_eq!(result.status, CheckStatus::Healthy);
    }

    #[test]
    fn test_health_snapshot_all_healthy() {
        let checks = vec![
            HealthCheckResult {
                component: "a".to_string(),
                status: CheckStatus::Healthy,
                message: "ok".to_string(),
                timestamp: Local::now(),
            },
            HealthCheckResult {
                component: "b".to_string(),
                status: CheckStatus::Skipped,
                message: "not configured".to_string(),
                timestamp: Local::now(),
            },
        ];
        let snap = HealthSnapshot::from_checks(checks);
        assert!(snap.healthy);
        assert_eq!(snap.failed_count(), 0);
    }

    #[test]
    fn test_health_snapshot_has_failure() {
        let checks = vec![
            HealthCheckResult {
                component: "a".to_string(),
                status: CheckStatus::Healthy,
                message: "ok".to_string(),
                timestamp: Local::now(),
            },
            HealthCheckResult {
                component: "b".to_string(),
                status: CheckStatus::Unhealthy,
                message: "unreachable".to_string(),
                timestamp: Local::now(),
            },
        ];
        let snap = HealthSnapshot::from_checks(checks);
        assert!(!snap.healthy);
        assert_eq!(snap.failed_count(), 1);
    }

    #[test]
    fn test_heartbeat_state_default() {
        let state = HeartbeatState::default();
        assert!(state.last_snapshot.is_none());
        assert_eq!(state.total_checks, 0);
        assert_eq!(state.total_failures, 0);
        assert_eq!(state.restarts_requested, 0);
    }

    #[test]
    fn test_health_snapshot_serde_roundtrip() {
        let snap = HealthSnapshot::from_checks(vec![HealthCheckResult {
            component: "test".to_string(),
            status: CheckStatus::Healthy,
            message: "ok".to_string(),
            timestamp: Local::now(),
        }]);
        let json = serde_json::to_string(&snap).unwrap();
        let back: HealthSnapshot = serde_json::from_str(&json).unwrap();
        assert!(back.healthy);
        assert_eq!(back.checks.len(), 1);
    }
}
