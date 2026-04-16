//! Heartbeat health check types, HealthCheck trait, and status tracking.

use async_trait::async_trait;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Status of an individual health check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Check passed.
    Healthy,
    /// Partial failure — component is operational but with reduced capacity.
    Degraded,
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
    /// Whether the overall system is healthy (all checks Healthy, Degraded, or Skipped).
    pub healthy: bool,
    /// Whether any component is in a degraded state.
    pub degraded: bool,
    /// When this snapshot was taken.
    pub timestamp: DateTime<Local>,
}

impl HealthSnapshot {
    /// Build a snapshot from a list of check results.
    pub fn from_checks(checks: Vec<HealthCheckResult>) -> Self {
        let healthy = checks.iter().all(|c| c.status != CheckStatus::Unhealthy);
        let degraded = checks.iter().any(|c| c.status == CheckStatus::Degraded);
        Self {
            checks,
            healthy,
            degraded,
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

    /// Count degraded checks.
    pub fn degraded_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.status == CheckStatus::Degraded)
            .count()
    }

    /// Return a short human-readable summary of the snapshot.
    ///
    /// Example: `"healthy: 3 healthy, 1 degraded, 0 failed (4 total)"`
    pub fn summary(&self) -> String {
        let healthy = self
            .checks
            .iter()
            .filter(|c| c.status == CheckStatus::Healthy)
            .count();
        let skipped = self
            .checks
            .iter()
            .filter(|c| c.status == CheckStatus::Skipped)
            .count();
        let degraded = self.degraded_count();
        let failed = self.failed_count();

        let status = if self.healthy {
            if self.degraded {
                "degraded"
            } else {
                "healthy"
            }
        } else {
            "unhealthy"
        };

        format!(
            "{}: {} healthy, {} degraded, {} failed, {} skipped ({} total)",
            status,
            healthy,
            degraded,
            failed,
            skipped,
            self.checks.len()
        )
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
    /// Per-component failure tracking.
    #[serde(default)]
    pub component_failures: Vec<ComponentFailureState>,
}

/// Per-component consecutive failure tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentFailureState {
    /// Component name.
    pub component: String,
    /// Number of consecutive failures.
    pub consecutive_failures: usize,
    /// Total failures for this component.
    pub total_failures: usize,
    /// Whether a restart has been requested (and not yet resolved).
    pub restart_pending: bool,
    /// Current backoff delay in seconds (doubles on each restart).
    pub backoff_secs: u64,
    /// When the last failure occurred.
    pub last_failure_at: Option<DateTime<Local>>,
    /// When the last restart was requested.
    pub last_restart_at: Option<DateTime<Local>>,
}

// ── HealthCheck trait ─────────────────────────────────────────

/// Trait for components that can report their health status.
///
/// Each component (provider, bus, session store, etc.) implements this
/// to provide async health checking. The HeartbeatService polls all
/// registered components periodically.
#[async_trait]
pub trait HealthCheck: Send + Sync {
    /// The component name (e.g. "provider", "bus", "session_store").
    fn component_name(&self) -> &str;

    /// Perform the health check and return the result.
    async fn report_health(&self) -> HealthCheckResult;
}

/// Registry for health-checkable components.
pub struct HealthCheckRegistry {
    checks: Vec<Arc<dyn HealthCheck>>,
}

impl HealthCheckRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self { checks: Vec::new() }
    }

    /// Register a health check component.
    pub fn register(&mut self, check: Arc<dyn HealthCheck>) {
        let name = check.component_name().to_string();
        // Prevent duplicates
        if let Some(_existing) = self.checks.iter().find(|c| c.component_name() == name) {
            tracing::warn!("Health check '{}' already registered, replacing", name);
            self.checks.retain(|c| c.component_name() != name);
        }
        self.checks.push(check);
    }

    /// Deregister a health check component by name.
    pub fn deregister(&mut self, name: &str) {
        self.checks.retain(|c| c.component_name() != name);
    }

    /// Get all registered health checks.
    pub fn checks(&self) -> &[Arc<dyn HealthCheck>] {
        &self.checks
    }

    /// Get a health check by name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn HealthCheck>> {
        self.checks.iter().find(|c| c.component_name() == name)
    }

    /// Number of registered checks.
    pub fn len(&self) -> usize {
        self.checks.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.checks.is_empty()
    }
}

impl Default for HealthCheckRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ─── FullHealthReport ─────────────────────────────────────────

/// A comprehensive health report that aggregates all component checks
/// with categorised breakdowns and failure history.
///
/// Produced by [`HeartbeatService::generate_full_report`](crate::HeartbeatService::generate_full_report).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullHealthReport {
    /// Aggregate status: `"healthy"`, `"degraded"`, or `"unhealthy"`.
    pub overall_status: String,
    /// When this report was generated.
    pub generated_at: DateTime<Local>,
    /// Total number of checks registered.
    pub total_components: usize,
    /// Number of healthy components.
    pub healthy_count: usize,
    /// Number of degraded components.
    pub degraded_count: usize,
    /// Number of unhealthy components.
    pub failed_count: usize,
    /// Number of skipped components.
    pub skipped_count: usize,
    /// Details of each component check.
    pub components: Vec<ComponentReport>,
    /// Total checks performed since service start.
    pub total_checks_run: usize,
    /// Total failures seen since service start.
    pub total_failures_seen: usize,
    /// Number of restarts requested since service start.
    pub restarts_requested: usize,
}

/// Per-component detail within a [`FullHealthReport`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentReport {
    /// Component name.
    pub name: String,
    /// Check status string.
    pub status: String,
    /// Human-readable status message.
    pub message: String,
    /// Consecutive failures (0 if healthy).
    pub consecutive_failures: usize,
    /// Whether a restart is pending for this component.
    pub restart_pending: bool,
}

impl FullHealthReport {
    /// Build a full report from a snapshot and heartbeat state.
    pub fn from_snapshot(snapshot: &HealthSnapshot, state: &HeartbeatState) -> Self {
        let mut healthy = 0;
        let mut degraded = 0;
        let mut failed = 0;
        let mut skipped = 0;

        let components: Vec<ComponentReport> = snapshot
            .checks
            .iter()
            .map(|c| {
                match c.status {
                    CheckStatus::Healthy => healthy += 1,
                    CheckStatus::Degraded => degraded += 1,
                    CheckStatus::Unhealthy => failed += 1,
                    CheckStatus::Skipped => skipped += 1,
                }

                let failure = state
                    .component_failures
                    .iter()
                    .find(|f| f.component == c.component);

                ComponentReport {
                    name: c.component.clone(),
                    status: match c.status {
                        CheckStatus::Healthy => "healthy".to_string(),
                        CheckStatus::Degraded => "degraded".to_string(),
                        CheckStatus::Unhealthy => "unhealthy".to_string(),
                        CheckStatus::Skipped => "skipped".to_string(),
                    },
                    message: c.message.clone(),
                    consecutive_failures: failure.map(|f| f.consecutive_failures).unwrap_or(0),
                    restart_pending: failure.map(|f| f.restart_pending).unwrap_or(false),
                }
            })
            .collect();

        let overall_status = if !snapshot.healthy {
            "unhealthy".to_string()
        } else if snapshot.degraded {
            "degraded".to_string()
        } else {
            "healthy".to_string()
        };

        Self {
            overall_status,
            generated_at: Local::now(),
            total_components: snapshot.checks.len(),
            healthy_count: healthy,
            degraded_count: degraded,
            failed_count: failed,
            skipped_count: skipped,
            components,
            total_checks_run: state.total_checks,
            total_failures_seen: state.total_failures,
            restarts_requested: state.restarts_requested,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_status_serde() {
        for status in &[
            CheckStatus::Healthy,
            CheckStatus::Degraded,
            CheckStatus::Unhealthy,
            CheckStatus::Skipped,
        ] {
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
    fn test_health_snapshot_degraded_is_healthy() {
        let checks = vec![
            HealthCheckResult {
                component: "a".to_string(),
                status: CheckStatus::Healthy,
                message: "ok".to_string(),
                timestamp: Local::now(),
            },
            HealthCheckResult {
                component: "b".to_string(),
                status: CheckStatus::Degraded,
                message: "1 of 2 channels disconnected".to_string(),
                timestamp: Local::now(),
            },
        ];
        let snap = HealthSnapshot::from_checks(checks);
        assert!(snap.healthy);
        assert!(snap.degraded);
        assert_eq!(snap.degraded_count(), 1);
        assert_eq!(snap.failed_count(), 0);
    }

    #[test]
    fn test_health_snapshot_summary_healthy() {
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
                message: "n/a".to_string(),
                timestamp: Local::now(),
            },
        ];
        let snap = HealthSnapshot::from_checks(checks);
        let summary = snap.summary();
        assert!(summary.starts_with("healthy:"));
        assert!(summary.contains("1 healthy"));
        assert!(summary.contains("0 degraded"));
        assert!(summary.contains("0 failed"));
        assert!(summary.contains("1 skipped"));
    }

    #[test]
    fn test_health_snapshot_summary_degraded() {
        let checks = vec![HealthCheckResult {
            component: "a".to_string(),
            status: CheckStatus::Degraded,
            message: "partial".to_string(),
            timestamp: Local::now(),
        }];
        let snap = HealthSnapshot::from_checks(checks);
        assert!(snap.summary().starts_with("degraded:"));
    }

    #[test]
    fn test_health_snapshot_summary_unhealthy() {
        let checks = vec![HealthCheckResult {
            component: "a".to_string(),
            status: CheckStatus::Unhealthy,
            message: "down".to_string(),
            timestamp: Local::now(),
        }];
        let snap = HealthSnapshot::from_checks(checks);
        assert!(snap.summary().starts_with("unhealthy:"));
    }

    #[test]
    fn test_heartbeat_state_default() {
        let state = HeartbeatState::default();
        assert!(state.last_snapshot.is_none());
        assert_eq!(state.total_checks, 0);
        assert_eq!(state.total_failures, 0);
        assert_eq!(state.restarts_requested, 0);
        assert!(state.component_failures.is_empty());
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

    #[test]
    fn test_component_failure_state_construction() {
        let state = ComponentFailureState {
            component: "provider".to_string(),
            consecutive_failures: 3,
            total_failures: 10,
            restart_pending: true,
            backoff_secs: 120,
            last_failure_at: Some(Local::now()),
            last_restart_at: None,
        };
        assert_eq!(state.component, "provider");
        assert_eq!(state.consecutive_failures, 3);
        assert!(state.restart_pending);
        assert_eq!(state.backoff_secs, 120);
    }

    #[test]
    fn test_component_failure_state_serde_roundtrip() {
        let state = ComponentFailureState {
            component: "test".to_string(),
            consecutive_failures: 2,
            total_failures: 5,
            restart_pending: false,
            backoff_secs: 30,
            last_failure_at: None,
            last_restart_at: Some(Local::now()),
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: ComponentFailureState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.component, "test");
        assert_eq!(back.consecutive_failures, 2);
        assert_eq!(back.backoff_secs, 30);
    }

    // === HealthCheckRegistry ===

    struct DummyCheck {
        name: String,
        healthy: bool,
    }

    #[async_trait]
    impl HealthCheck for DummyCheck {
        fn component_name(&self) -> &str {
            &self.name
        }

        async fn report_health(&self) -> HealthCheckResult {
            HealthCheckResult {
                component: self.name.clone(),
                status: if self.healthy {
                    CheckStatus::Healthy
                } else {
                    CheckStatus::Unhealthy
                },
                message: if self.healthy {
                    "ok".to_string()
                } else {
                    "failing".to_string()
                },
                timestamp: Local::now(),
            }
        }
    }

    #[test]
    fn test_registry_new() {
        let reg = HealthCheckRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn test_registry_default() {
        let reg = HealthCheckRegistry::default();
        assert!(reg.is_empty());
    }

    #[test]
    fn test_registry_register() {
        let mut reg = HealthCheckRegistry::new();
        reg.register(Arc::new(DummyCheck {
            name: "comp_a".to_string(),
            healthy: true,
        }));
        assert_eq!(reg.len(), 1);
        assert!(reg.get("comp_a").is_some());
        assert!(reg.get("comp_b").is_none());
    }

    #[test]
    fn test_registry_register_replaces_duplicate() {
        let mut reg = HealthCheckRegistry::new();
        reg.register(Arc::new(DummyCheck {
            name: "comp".to_string(),
            healthy: true,
        }));
        reg.register(Arc::new(DummyCheck {
            name: "comp".to_string(),
            healthy: false,
        }));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn test_registry_deregister() {
        let mut reg = HealthCheckRegistry::new();
        reg.register(Arc::new(DummyCheck {
            name: "a".to_string(),
            healthy: true,
        }));
        reg.register(Arc::new(DummyCheck {
            name: "b".to_string(),
            healthy: true,
        }));
        assert_eq!(reg.len(), 2);

        reg.deregister("a");
        assert_eq!(reg.len(), 1);
        assert!(reg.get("a").is_none());
        assert!(reg.get("b").is_some());
    }

    #[test]
    fn test_registry_deregister_missing() {
        let mut reg = HealthCheckRegistry::new();
        reg.deregister("nope"); // Should not panic
        assert!(reg.is_empty());
    }

    #[tokio::test]
    async fn test_registry_check_reports_health() {
        let _reg = HealthCheckRegistry::new();
        let check = DummyCheck {
            name: "test".to_string(),
            healthy: true,
        };
        let result = check.report_health().await;
        assert_eq!(result.component, "test");
        assert_eq!(result.status, CheckStatus::Healthy);
    }

    #[tokio::test]
    async fn test_registry_check_reports_unhealthy() {
        let check = DummyCheck {
            name: "broken".to_string(),
            healthy: false,
        };
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Unhealthy);
        assert_eq!(result.message, "failing");
    }

    // === FullHealthReport ===

    #[test]
    fn test_full_health_report_all_healthy() {
        let snapshot = HealthSnapshot::from_checks(vec![
            HealthCheckResult {
                component: "a".to_string(),
                status: CheckStatus::Healthy,
                message: "ok".to_string(),
                timestamp: Local::now(),
            },
            HealthCheckResult {
                component: "b".to_string(),
                status: CheckStatus::Skipped,
                message: "n/a".to_string(),
                timestamp: Local::now(),
            },
        ]);
        let state = HeartbeatState {
            total_checks: 5,
            total_failures: 0,
            ..Default::default()
        };
        let report = FullHealthReport::from_snapshot(&snapshot, &state);
        assert_eq!(report.overall_status, "healthy");
        assert_eq!(report.healthy_count, 1);
        assert_eq!(report.skipped_count, 1);
        assert_eq!(report.failed_count, 0);
        assert_eq!(report.total_components, 2);
        assert_eq!(report.total_checks_run, 5);
    }

    #[test]
    fn test_full_health_report_mixed() {
        let snapshot = HealthSnapshot::from_checks(vec![
            HealthCheckResult {
                component: "a".to_string(),
                status: CheckStatus::Healthy,
                message: "ok".to_string(),
                timestamp: Local::now(),
            },
            HealthCheckResult {
                component: "b".to_string(),
                status: CheckStatus::Degraded,
                message: "partial".to_string(),
                timestamp: Local::now(),
            },
            HealthCheckResult {
                component: "c".to_string(),
                status: CheckStatus::Unhealthy,
                message: "down".to_string(),
                timestamp: Local::now(),
            },
        ]);
        let state = HeartbeatState {
            total_checks: 10,
            total_failures: 3,
            restarts_requested: 1,
            component_failures: vec![ComponentFailureState {
                component: "c".to_string(),
                consecutive_failures: 3,
                total_failures: 3,
                restart_pending: true,
                backoff_secs: 60,
                last_failure_at: Some(Local::now()),
                last_restart_at: None,
            }],
            ..Default::default()
        };
        let report = FullHealthReport::from_snapshot(&snapshot, &state);
        assert_eq!(report.overall_status, "unhealthy");
        assert_eq!(report.healthy_count, 1);
        assert_eq!(report.degraded_count, 1);
        assert_eq!(report.failed_count, 1);
        assert_eq!(report.restarts_requested, 1);

        // Component "c" should show failure details
        let comp_c = report.components.iter().find(|c| c.name == "c").unwrap();
        assert_eq!(comp_c.consecutive_failures, 3);
        assert!(comp_c.restart_pending);
    }

    #[test]
    fn test_full_health_report_serde_roundtrip() {
        let snapshot = HealthSnapshot::from_checks(vec![HealthCheckResult {
            component: "test".to_string(),
            status: CheckStatus::Healthy,
            message: "ok".to_string(),
            timestamp: Local::now(),
        }]);
        let state = HeartbeatState::default();
        let report = FullHealthReport::from_snapshot(&snapshot, &state);
        let json = serde_json::to_string(&report).unwrap();
        let back: FullHealthReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.overall_status, "healthy");
        assert_eq!(back.total_components, 1);
    }
}
