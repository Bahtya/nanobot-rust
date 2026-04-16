//! Heartbeat service — periodic health checks, auto-restart, and state persistence.
//!
//! Runs on a configurable interval and:
//! 1. Polls all registered `HealthCheck` components
//! 2. Tracks consecutive failures per component
//! 3. Triggers restart via the bus after N consecutive failures (with exponential backoff)
//! 4. Persists state to `heartbeat_state.json`

use crate::types::*;
use anyhow::{Context, Result};
use chrono::Local;
use nanobot_bus::events::AgentEvent;
use nanobot_bus::MessageBus;
use nanobot_providers::ProviderRegistry;
use nanobot_session::SessionManager;
use nanobot_tools::ToolRegistry;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Default interval between health checks (seconds).
const DEFAULT_INTERVAL_SECS: u64 = 30;

/// Number of consecutive failures before triggering a restart.
const FAILURES_BEFORE_RESTART: usize = 3;

/// Initial backoff in seconds for restart delays.
const INITIAL_BACKOFF_SECS: u64 = 30;

/// Maximum backoff in seconds (cap for exponential growth).
const MAX_BACKOFF_SECS: u64 = 3600;

/// The heartbeat service.
pub struct HeartbeatService {
    interval: Duration,
    running: Arc<RwLock<bool>>,
    bus: Option<Arc<MessageBus>>,
    providers: Option<Arc<ProviderRegistry>>,
    tools: Option<Arc<ToolRegistry>>,
    sessions: Option<Arc<SessionManager>>,
    state_path: PathBuf,
    state: parking_lot::Mutex<HeartbeatState>,
    registry: parking_lot::RwLock<HealthCheckRegistry>,
    snapshot_sinks: parking_lot::RwLock<Vec<Arc<parking_lot::RwLock<Option<HealthSnapshot>>>>>,
    failures_before_restart: usize,
}

impl HeartbeatService {
    /// Create a new heartbeat service with default state directory.
    pub fn new(config: nanobot_config::Config) -> Self {
        let interval_secs = config.heartbeat.interval_secs.max(DEFAULT_INTERVAL_SECS);
        let data_dir =
            nanobot_config::paths::get_data_dir().unwrap_or_else(|_| std::env::temp_dir());
        let state_path = data_dir.join("heartbeat_state.json");
        let state = load_state(&state_path).unwrap_or_default();

        Self {
            interval: Duration::from_secs(interval_secs),
            running: Arc::new(RwLock::new(false)),
            bus: None,
            providers: None,
            tools: None,
            sessions: None,
            state_path,
            state: parking_lot::Mutex::new(state),
            registry: parking_lot::RwLock::new(HealthCheckRegistry::new()),
            snapshot_sinks: parking_lot::RwLock::new(Vec::new()),
            failures_before_restart: FAILURES_BEFORE_RESTART,
        }
    }

    /// Create with a custom data directory.
    pub fn with_data_dir(config: nanobot_config::Config, data_dir: PathBuf) -> Self {
        let interval_secs = config.heartbeat.interval_secs.max(DEFAULT_INTERVAL_SECS);
        let state_path = data_dir.join("heartbeat_state.json");
        let state = load_state(&state_path).unwrap_or_default();

        Self {
            interval: Duration::from_secs(interval_secs),
            running: Arc::new(RwLock::new(false)),
            bus: None,
            providers: None,
            tools: None,
            sessions: None,
            state_path,
            state: parking_lot::Mutex::new(state),
            registry: parking_lot::RwLock::new(HealthCheckRegistry::new()),
            snapshot_sinks: parking_lot::RwLock::new(Vec::new()),
            failures_before_restart: FAILURES_BEFORE_RESTART,
        }
    }

    /// Create with external registries (for gateway/heartbeat commands).
    #[allow(clippy::too_many_arguments)]
    pub fn with_registries(
        config: nanobot_config::Config,
        providers: ProviderRegistry,
        tools: ToolRegistry,
        sessions: SessionManager,
    ) -> Self {
        let mut service = Self::new(config);
        service.providers = Some(Arc::new(providers));
        service.tools = Some(Arc::new(tools));
        service.sessions = Some(Arc::new(sessions));
        service
    }

    /// Wire a MessageBus for event emission during health checks.
    pub fn set_bus(&mut self, bus: MessageBus) {
        self.bus = Some(Arc::new(bus));
    }

    /// Get the attached provider registry, if one was supplied.
    pub fn provider_registry(&self) -> Option<Arc<ProviderRegistry>> {
        self.providers.clone()
    }

    /// Get the attached tool registry, if one was supplied.
    pub fn tool_registry(&self) -> Option<Arc<ToolRegistry>> {
        self.tools.clone()
    }

    /// Get the attached session manager, if one was supplied.
    pub fn session_manager(&self) -> Option<Arc<SessionManager>> {
        self.sessions.clone()
    }

    /// Register an external sink that should receive every health snapshot.
    pub fn add_snapshot_sink(&self, sink: Arc<parking_lot::RwLock<Option<HealthSnapshot>>>) {
        if let Some(snapshot) = self.last_snapshot() {
            *sink.write() = Some(snapshot);
        }
        self.snapshot_sinks.write().push(sink);
    }

    /// Register a health check component.
    pub fn register_check(&self, check: Arc<dyn HealthCheck>) {
        self.registry.write().register(check);
    }

    /// Deregister a health check component by name.
    pub fn deregister_check(&self, name: &str) {
        self.registry.write().deregister(name);
    }

    /// Set the number of consecutive failures before triggering a restart.
    pub fn with_failures_before_restart(mut self, n: usize) -> Self {
        self.failures_before_restart = n.max(1);
        self
    }

    /// Start the heartbeat loop.
    pub async fn run(&self) -> Result<()> {
        {
            let mut running = self.running.write().await;
            if *running {
                return Ok(());
            }
            *running = true;
        }

        self.state.lock().started_at = Some(Local::now());
        let _ = self.persist_state();

        info!(
            "Heartbeat service started (interval: {}s, {} check(s) registered)",
            self.interval.as_secs(),
            self.registry.read().len()
        );

        loop {
            if !*self.running.read().await {
                break;
            }

            tokio::time::sleep(self.interval).await;

            if !*self.running.read().await {
                break;
            }

            match self.run_checks().await {
                Ok(snapshot) => {
                    if snapshot.healthy && !snapshot.degraded {
                        debug!("Heartbeat: all {} checks healthy", snapshot.checks.len());
                    } else if snapshot.healthy && snapshot.degraded {
                        warn!(
                            "Heartbeat: {} degraded: {}",
                            snapshot.degraded_count(),
                            snapshot.summary(),
                        );
                    } else {
                        warn!(
                            "Heartbeat: {} of {} checks failed",
                            snapshot.failed_count(),
                            snapshot.checks.len()
                        );
                    }
                }
                Err(e) => {
                    error!("Heartbeat check cycle failed: {}", e);
                }
            }
        }

        self.state.lock().stopped_at = Some(Local::now());
        let _ = self.persist_state();
        info!("Heartbeat service stopped");
        Ok(())
    }

    /// Run all health checks and return a snapshot.
    pub async fn run_checks(&self) -> Result<HealthSnapshot> {
        // Clone checks out of the lock before awaiting to keep the future Send-safe.
        let checks_snapshot: Vec<Arc<dyn HealthCheck>> = {
            let registry = self.registry.read();
            registry.checks().to_vec()
        };

        let mut checks = Vec::new();
        for check in &checks_snapshot {
            let result = check.report_health().await;
            checks.push(result);
        }

        let snapshot = HealthSnapshot::from_checks(checks);

        // Detect state transitions and emit HealthStatusChanged event
        let previous_snapshot = {
            let state = self.state.lock();
            state.last_snapshot.clone()
        };
        let previous_status = previous_snapshot.as_ref().map(Self::aggregate_status);
        let current_status = Self::aggregate_status(&snapshot);

        // Update persistent state and track failures
        {
            let mut state = self.state.lock();
            state.total_checks += 1;
            state.total_failures += snapshot.failed_count();
            state.last_snapshot = Some(snapshot.clone());
        }
        self.publish_snapshot(snapshot.clone());

        // Emit state change notification if status transitioned
        if let Some(ref prev) = previous_status {
            if prev != &current_status {
                info!("Health status changed: {} → {}", prev, current_status);
                if let Some(bus) = &self.bus {
                    bus.emit_event(AgentEvent::HealthStatusChanged {
                        from: prev.clone(),
                        to: current_status.clone(),
                        failed_count: snapshot.failed_count(),
                        degraded_count: snapshot.degraded_count(),
                    });
                }
            }
        }

        // Detect per-component status transitions and emit events
        // Only compare when we have a previous snapshot (skip on first check)
        if let Some(ref prev_snap) = previous_snapshot {
            for check in &snapshot.checks {
                let prev_status = prev_snap
                    .checks
                    .iter()
                    .find(|c| c.component == check.component)
                    .map(|c| format!("{:?}", c.status))
                    .unwrap_or_else(|| "unknown".to_string());
                let curr_status = format!("{:?}", check.status);
                if prev_status != curr_status {
                    debug!(
                        "Component '{}' status changed: {} → {}",
                        check.component, prev_status, curr_status
                    );
                    if let Some(bus) = &self.bus {
                        bus.emit_event(AgentEvent::ComponentStatusChanged {
                            component: check.component.clone(),
                            from: prev_status,
                            to: curr_status,
                            message: check.message.clone(),
                        });
                    }
                }
            }
        }

        // Process failures and trigger restarts if needed
        self.process_failures(&snapshot);

        // Always emit heartbeat check event
        if let Some(bus) = &self.bus {
            bus.emit_event(AgentEvent::HeartbeatCheck {
                healthy: snapshot.healthy,
                checks_total: snapshot.checks.len(),
                checks_failed: snapshot.failed_count(),
            });
        }

        let _ = self.persist_state();
        Ok(snapshot)
    }

    /// Compute an aggregate status string from a snapshot.
    fn aggregate_status(snapshot: &HealthSnapshot) -> String {
        if !snapshot.healthy {
            "unhealthy".to_string()
        } else if snapshot.degraded {
            "degraded".to_string()
        } else {
            "healthy".to_string()
        }
    }

    /// Process check results, track consecutive failures, and trigger restarts.
    fn process_failures(&self, snapshot: &HealthSnapshot) {
        // First pass: update failure tracking and collect restart actions
        let mut restarts: Vec<(String, String, u64)> = Vec::new();
        let mut new_restart_count: usize = 0;

        {
            let mut state = self.state.lock();

            for check in &snapshot.checks {
                match check.status {
                    CheckStatus::Unhealthy => {
                        // Extract existing or create new failure state
                        let (_consecutive, should_restart) = {
                            if let Some(f) = state
                                .component_failures
                                .iter_mut()
                                .find(|f| f.component == check.component)
                            {
                                f.consecutive_failures += 1;
                                f.total_failures += 1;
                                f.last_failure_at = Some(Local::now());
                                let should = f.consecutive_failures >= self.failures_before_restart
                                    && !f.restart_pending;
                                (f.consecutive_failures, should)
                            } else {
                                (1, self.failures_before_restart <= 1)
                            }
                        };

                        // Now apply updates (borrow is released)
                        if let Some(f) = state
                            .component_failures
                            .iter_mut()
                            .find(|f| f.component == check.component)
                        {
                            if should_restart {
                                let backoff = f.backoff_secs;
                                f.restart_pending = true;
                                f.last_restart_at = Some(Local::now());
                                f.backoff_secs = (f.backoff_secs * 2).min(MAX_BACKOFF_SECS);
                                new_restart_count += 1;
                                restarts.push((
                                    f.component.clone(),
                                    check.message.clone(),
                                    backoff,
                                ));
                            }
                        } else {
                            // New component failure entry
                            let state_entry = ComponentFailureState {
                                component: check.component.clone(),
                                consecutive_failures: 1,
                                total_failures: 1,
                                restart_pending: should_restart,
                                backoff_secs: if should_restart {
                                    (INITIAL_BACKOFF_SECS * 2).min(MAX_BACKOFF_SECS)
                                } else {
                                    INITIAL_BACKOFF_SECS
                                },
                                last_failure_at: Some(Local::now()),
                                last_restart_at: if should_restart {
                                    Some(Local::now())
                                } else {
                                    None
                                },
                            };
                            if should_restart {
                                new_restart_count += 1;
                                restarts.push((
                                    check.component.clone(),
                                    check.message.clone(),
                                    INITIAL_BACKOFF_SECS,
                                ));
                            }
                            state.component_failures.push(state_entry);
                        }
                    }
                    CheckStatus::Healthy | CheckStatus::Degraded | CheckStatus::Skipped => {
                        if let Some(f) = state
                            .component_failures
                            .iter_mut()
                            .find(|f| f.component == check.component)
                        {
                            f.consecutive_failures = 0;
                            f.restart_pending = false;
                        }
                    }
                }
            }

            state.restarts_requested += new_restart_count;
        }

        // Second pass: emit bus events for restarts
        for (component, reason, backoff) in restarts {
            info!(
                "Triggering restart for '{}' (backoff: {}s): {}",
                component, backoff, reason
            );

            if let Some(bus) = &self.bus {
                bus.emit_event(AgentEvent::RestartRequested {
                    component: component.clone(),
                    reason: reason.clone(),
                });
            }
        }
    }

    /// Stop the heartbeat service.
    pub async fn stop(&self) {
        *self.running.write().await = false;
    }

    /// Check if the service is running.
    pub async fn is_running(&self) -> bool {
        *self.running.read().await
    }

    /// Get the current state (for inspection).
    pub fn state(&self) -> HeartbeatState {
        self.state.lock().clone()
    }

    /// Get the configured interval.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Get the list of registered check names.
    pub fn registered_checks(&self) -> Vec<String> {
        self.registry
            .read()
            .checks()
            .iter()
            .map(|c| c.component_name().to_string())
            .collect()
    }

    /// Get the failure state for a specific component.
    pub fn component_failure_state(&self, name: &str) -> Option<ComponentFailureState> {
        self.state
            .lock()
            .component_failures
            .iter()
            .find(|f| f.component == name)
            .cloned()
    }

    /// Get the latest health snapshot, if one has been taken.
    pub fn last_snapshot(&self) -> Option<HealthSnapshot> {
        self.state.lock().last_snapshot.clone()
    }

    /// Generate a comprehensive health report.
    ///
    /// Runs all registered health checks and produces a [`FullHealthReport`]
    /// that aggregates per-component status with failure history. Useful for
    /// HTTP endpoints and diagnostic tooling.
    ///
    /// Returns an error if the check cycle itself fails (should be rare).
    pub async fn generate_full_report(&self) -> Result<FullHealthReport> {
        let snapshot = self.run_checks().await?;
        let state = self.state.lock().clone();
        Ok(FullHealthReport::from_snapshot(&snapshot, &state))
    }

    /// Generate a report from the last cached snapshot without re-running checks.
    ///
    /// Returns `None` if no checks have been run yet.
    pub fn cached_full_report(&self) -> Option<FullHealthReport> {
        let state = self.state.lock();
        let snapshot = state.last_snapshot.as_ref()?;
        Some(FullHealthReport::from_snapshot(snapshot, &state))
    }

    /// Persist state to disk.
    fn persist_state(&self) -> Result<()> {
        let state = self.state.lock();
        let json = serde_json::to_string_pretty(&*state)
            .with_context(|| "Failed to serialize heartbeat state")?;
        std::fs::write(&self.state_path, json)
            .with_context(|| format!("Failed to write {}", self.state_path.display()))?;
        Ok(())
    }

    /// Publish the latest snapshot to all registered sinks.
    fn publish_snapshot(&self, snapshot: HealthSnapshot) {
        let sinks = self.snapshot_sinks.read().clone();
        for sink in sinks {
            *sink.write() = Some(snapshot.clone());
        }
    }
}

/// Load state from disk.
fn load_state(path: &PathBuf) -> Result<HeartbeatState> {
    if !path.exists() {
        return Ok(HeartbeatState::default());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| "Failed to parse heartbeat state")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_service(dir: &std::path::Path) -> HeartbeatService {
        let config = nanobot_config::Config::default();
        HeartbeatService::with_data_dir(config, dir.to_path_buf())
    }

    struct MockCheck {
        name: String,
        healthy: bool,
    }

    #[async_trait::async_trait]
    impl HealthCheck for MockCheck {
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

    /// Wrapper that allows toggling health via interior mutability.
    struct ToggleCheck {
        name: String,
        healthy: parking_lot::Mutex<bool>,
    }

    #[async_trait::async_trait]
    impl HealthCheck for ToggleCheck {
        fn component_name(&self) -> &str {
            &self.name
        }
        async fn report_health(&self) -> HealthCheckResult {
            let healthy = *self.healthy.lock();
            HealthCheckResult {
                component: self.name.clone(),
                status: if healthy {
                    CheckStatus::Healthy
                } else {
                    CheckStatus::Unhealthy
                },
                message: if healthy {
                    "ok".to_string()
                } else {
                    "failing".to_string()
                },
                timestamp: Local::now(),
            }
        }
    }

    impl ToggleCheck {
        fn new(name: &str, healthy: bool) -> Self {
            Self {
                name: name.to_string(),
                healthy: parking_lot::Mutex::new(healthy),
            }
        }

        fn set_healthy(&self, healthy: bool) {
            *self.healthy.lock() = healthy;
        }
    }

    // === Construction ===

    #[test]
    fn test_construction() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        assert!(svc.interval() >= Duration::from_secs(DEFAULT_INTERVAL_SECS));
        assert!(svc.registered_checks().is_empty());
    }

    #[test]
    fn test_construction_with_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = nanobot_config::Config::default();
        let svc = HeartbeatService::with_data_dir(config, dir.path().to_path_buf());
        assert!(svc.interval() >= Duration::from_secs(DEFAULT_INTERVAL_SECS));
    }

    #[tokio::test]
    async fn test_not_running_initially() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        assert!(!svc.is_running().await);
    }

    #[tokio::test]
    async fn test_stop_when_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.stop().await;
        assert!(!svc.is_running().await);
    }

    // === Interval config ===

    #[test]
    fn test_interval_minimum_30s() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = nanobot_config::Config::default();
        config.heartbeat.interval_secs = 10;
        let svc = HeartbeatService::with_data_dir(config, dir.path().to_path_buf());
        assert!(svc.interval() >= Duration::from_secs(DEFAULT_INTERVAL_SECS));
    }

    #[test]
    fn test_interval_uses_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = nanobot_config::Config::default();
        config.heartbeat.interval_secs = 300;
        let svc = HeartbeatService::with_data_dir(config, dir.path().to_path_buf());
        assert_eq!(svc.interval(), Duration::from_secs(300));
    }

    #[test]
    fn test_default_disabled() {
        let config = nanobot_config::Config::default();
        assert!(!config.heartbeat.enabled);
    }

    // === Register / Deregister ===

    #[test]
    fn test_register_check() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.register_check(Arc::new(MockCheck {
            name: "comp_a".to_string(),
            healthy: true,
        }));
        assert_eq!(svc.registered_checks(), vec!["comp_a"]);
    }

    #[test]
    fn test_register_multiple() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.register_check(Arc::new(MockCheck {
            name: "a".to_string(),
            healthy: true,
        }));
        svc.register_check(Arc::new(MockCheck {
            name: "b".to_string(),
            healthy: false,
        }));
        let checks = svc.registered_checks();
        assert_eq!(checks.len(), 2);
        assert!(checks.contains(&"a".to_string()));
        assert!(checks.contains(&"b".to_string()));
    }

    #[test]
    fn test_deregister_check() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.register_check(Arc::new(MockCheck {
            name: "x".to_string(),
            healthy: true,
        }));
        assert_eq!(svc.registered_checks().len(), 1);
        svc.deregister_check("x");
        assert!(svc.registered_checks().is_empty());
    }

    // === Run checks ===

    #[tokio::test]
    async fn test_run_checks_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.register_check(Arc::new(MockCheck {
            name: "comp".to_string(),
            healthy: true,
        }));
        let snapshot = svc.run_checks().await.unwrap();
        assert!(snapshot.healthy);
        assert_eq!(snapshot.checks.len(), 1);
        assert_eq!(snapshot.checks[0].status, CheckStatus::Healthy);
    }

    #[tokio::test]
    async fn test_run_checks_unhealthy() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.register_check(Arc::new(MockCheck {
            name: "comp".to_string(),
            healthy: false,
        }));
        let snapshot = svc.run_checks().await.unwrap();
        assert!(!snapshot.healthy);
        assert_eq!(snapshot.failed_count(), 1);
    }

    #[tokio::test]
    async fn test_run_checks_no_checks() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let snapshot = svc.run_checks().await.unwrap();
        assert!(snapshot.healthy);
        assert!(snapshot.checks.is_empty());
    }

    #[tokio::test]
    async fn test_run_checks_mixed() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.register_check(Arc::new(MockCheck {
            name: "healthy_comp".to_string(),
            healthy: true,
        }));
        svc.register_check(Arc::new(MockCheck {
            name: "broken_comp".to_string(),
            healthy: false,
        }));
        let snapshot = svc.run_checks().await.unwrap();
        assert!(!snapshot.healthy);
        assert_eq!(snapshot.checks.len(), 2);
        assert_eq!(snapshot.failed_count(), 1);
    }

    #[tokio::test]
    async fn test_run_checks_updates_state() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        assert_eq!(svc.state().total_checks, 0);
        svc.run_checks().await.unwrap();
        assert_eq!(svc.state().total_checks, 1);
        assert!(svc.state().last_snapshot.is_some());
    }

    // === Consecutive failure tracking ===

    #[tokio::test]
    async fn test_consecutive_failures_tracked() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.register_check(Arc::new(MockCheck {
            name: "comp".to_string(),
            healthy: false,
        }));

        // Run 2 checks
        svc.run_checks().await.unwrap();
        svc.run_checks().await.unwrap();

        let failure = svc.component_failure_state("comp").unwrap();
        assert_eq!(failure.consecutive_failures, 2);
        assert_eq!(failure.total_failures, 2);
    }

    #[tokio::test]
    async fn test_consecutive_failures_reset_on_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());

        let check = Arc::new(ToggleCheck::new("comp", false));
        svc.register_check(check.clone());

        // Fail twice
        svc.run_checks().await.unwrap();
        svc.run_checks().await.unwrap();

        let failure = svc.component_failure_state("comp").unwrap();
        assert_eq!(failure.consecutive_failures, 2);

        // Now recover
        check.set_healthy(true);
        svc.run_checks().await.unwrap();

        let failure = svc.component_failure_state("comp").unwrap();
        assert_eq!(failure.consecutive_failures, 0);
        assert_eq!(failure.total_failures, 2);
    }

    // === Auto-restart ===

    #[tokio::test]
    async fn test_restart_triggered_after_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path()).with_failures_before_restart(3);

        svc.register_check(Arc::new(ToggleCheck::new("comp", false)));

        // Run 3 checks to trigger restart
        svc.run_checks().await.unwrap();
        svc.run_checks().await.unwrap();
        svc.run_checks().await.unwrap();

        let state = svc.state();
        assert_eq!(state.restarts_requested, 1);

        let failure = svc.component_failure_state("comp").unwrap();
        assert!(failure.restart_pending);
        assert_eq!(failure.backoff_secs, INITIAL_BACKOFF_SECS * 2);
    }

    #[tokio::test]
    async fn test_restart_not_triggered_below_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path()).with_failures_before_restart(5);

        svc.register_check(Arc::new(ToggleCheck::new("comp", false)));

        // Run 3 checks (below threshold of 5)
        for _ in 0..3 {
            svc.run_checks().await.unwrap();
        }

        let state = svc.state();
        assert_eq!(state.restarts_requested, 0);
    }

    #[tokio::test]
    async fn test_exponential_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path()).with_failures_before_restart(2);

        let check = Arc::new(ToggleCheck::new("comp", false));
        svc.register_check(check.clone());

        // First restart cycle
        svc.run_checks().await.unwrap();
        svc.run_checks().await.unwrap();
        let failure = svc.component_failure_state("comp").unwrap();
        assert!(failure.restart_pending);
        assert_eq!(failure.backoff_secs, INITIAL_BACKOFF_SECS * 2);

        // Recover and fail again to trigger second restart
        check.set_healthy(true);
        svc.run_checks().await.unwrap();
        check.set_healthy(false);
        svc.run_checks().await.unwrap();
        svc.run_checks().await.unwrap();

        let failure = svc.component_failure_state("comp").unwrap();
        assert_eq!(failure.backoff_secs, INITIAL_BACKOFF_SECS * 4);
    }

    #[tokio::test]
    async fn test_no_double_restart() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path()).with_failures_before_restart(2);

        svc.register_check(Arc::new(MockCheck {
            name: "comp".to_string(),
            healthy: false,
        }));

        // Trigger restart
        svc.run_checks().await.unwrap();
        svc.run_checks().await.unwrap();

        // Keep failing
        svc.run_checks().await.unwrap();
        svc.run_checks().await.unwrap();

        // Should only have 1 restart (not re-triggered while pending)
        let state = svc.state();
        assert_eq!(state.restarts_requested, 1);
    }

    // === Bus events ===

    #[tokio::test]
    async fn test_heartbeat_event_emitted() {
        let dir = tempfile::tempdir().unwrap();
        let bus = MessageBus::new();
        let mut events_rx = bus.subscribe_events();
        let mut svc = make_service(dir.path());
        svc.set_bus(bus);

        svc.register_check(Arc::new(MockCheck {
            name: "comp".to_string(),
            healthy: true,
        }));

        svc.run_checks().await.unwrap();

        let event = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            AgentEvent::HeartbeatCheck { healthy: true, .. }
        ));
    }

    #[tokio::test]
    async fn test_restart_event_emitted() {
        let dir = tempfile::tempdir().unwrap();
        let bus = MessageBus::new();
        let mut events_rx = bus.subscribe_events();
        let mut svc = make_service(dir.path()).with_failures_before_restart(2);
        svc.set_bus(bus);

        svc.register_check(Arc::new(MockCheck {
            name: "comp".to_string(),
            healthy: false,
        }));

        svc.run_checks().await.unwrap();
        svc.run_checks().await.unwrap();

        // Should receive: 2 HeartbeatCheck + 1 RestartRequested = 3 events
        let mut got_restart = false;
        for _ in 0..3 {
            let event = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
                .await
                .unwrap()
                .unwrap();
            if matches!(event, AgentEvent::RestartRequested { component, .. } if component == "comp")
            {
                got_restart = true;
            }
        }
        assert!(got_restart);
    }

    #[tokio::test]
    async fn test_no_restart_event_when_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let bus = MessageBus::new();
        let mut events_rx = bus.subscribe_events();
        let mut svc = make_service(dir.path());
        svc.set_bus(bus);

        svc.register_check(Arc::new(MockCheck {
            name: "comp".to_string(),
            healthy: true,
        }));

        svc.run_checks().await.unwrap();

        let event = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            AgentEvent::HeartbeatCheck { healthy: true, .. }
        ));
        assert_eq!(svc.state().restarts_requested, 0);
    }

    // === State persistence ===

    #[test]
    fn test_state_persists_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = nanobot_config::Config::default();
        let svc = HeartbeatService::with_data_dir(config, dir.path().to_path_buf());

        {
            let mut state = svc.state.lock();
            state.total_checks = 42;
        }
        svc.persist_state().unwrap();

        let state_path = dir.path().join("heartbeat_state.json");
        assert!(state_path.exists());

        let loaded: HeartbeatState =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        assert_eq!(loaded.total_checks, 42);
    }

    #[tokio::test]
    async fn test_state_persists_after_checks() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("heartbeat_state.json");

        {
            let svc = make_service(dir.path());
            svc.register_check(Arc::new(MockCheck {
                name: "comp".to_string(),
                healthy: true,
            }));
            svc.run_checks().await.unwrap();
        }

        assert!(state_path.exists());
        let loaded: HeartbeatState =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        assert_eq!(loaded.total_checks, 1);
        assert!(loaded.last_snapshot.is_some());
    }

    #[tokio::test]
    async fn test_state_persists_failure_tracking() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("heartbeat_state.json");

        {
            let svc = make_service(dir.path());
            svc.register_check(Arc::new(MockCheck {
                name: "comp".to_string(),
                healthy: false,
            }));
            svc.run_checks().await.unwrap();
        }

        let loaded: HeartbeatState =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        assert_eq!(loaded.component_failures.len(), 1);
        assert_eq!(loaded.component_failures[0].component, "comp");
        assert_eq!(loaded.component_failures[0].consecutive_failures, 1);
    }

    #[test]
    fn test_state_file_is_heartbeat_state_json() {
        let dir = tempfile::tempdir().unwrap();
        let config = nanobot_config::Config::default();
        let svc = HeartbeatService::with_data_dir(config, dir.path().to_path_buf());
        svc.state.lock().total_checks = 1;
        svc.persist_state().unwrap();

        assert!(dir.path().join("heartbeat_state.json").exists());
    }

    // === State inspection ===

    #[tokio::test]
    async fn test_state_reflects_checks() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let initial = svc.state();
        assert_eq!(initial.total_checks, 0);

        svc.register_check(Arc::new(MockCheck {
            name: "comp".to_string(),
            healthy: true,
        }));
        svc.run_checks().await.unwrap();
        let after = svc.state();
        assert_eq!(after.total_checks, 1);
        assert!(after.last_snapshot.is_some());
    }

    // === Multiple check cycles ===

    #[tokio::test]
    async fn test_multiple_check_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.register_check(Arc::new(MockCheck {
            name: "comp".to_string(),
            healthy: true,
        }));

        for _ in 0..5 {
            svc.run_checks().await.unwrap();
        }

        let state = svc.state();
        assert_eq!(state.total_checks, 5);
    }

    // === with_failures_before_restart ===

    #[test]
    fn test_with_failures_before_restart_min_1() {
        let dir = tempfile::tempdir().unwrap();
        let config = nanobot_config::Config::default();
        let svc = HeartbeatService::with_data_dir(config, dir.path().to_path_buf())
            .with_failures_before_restart(0);
        assert_eq!(svc.failures_before_restart, 1);
    }

    // === Health status change notifications ===

    #[tokio::test]
    async fn test_health_status_changed_healthy_to_unhealthy() {
        let dir = tempfile::tempdir().unwrap();
        let bus = MessageBus::new();
        let mut events_rx = bus.subscribe_events();
        let mut svc = make_service(dir.path());
        svc.set_bus(bus);

        let check = Arc::new(ToggleCheck::new("comp", true));
        svc.register_check(check.clone());

        // First check — healthy (no previous, no change event)
        svc.run_checks().await.unwrap();

        // Now fail
        check.set_healthy(false);
        svc.run_checks().await.unwrap();

        // Should receive: 2 HeartbeatCheck + 1 HealthStatusChanged = 3 events
        let mut got_change = false;
        for _ in 0..5 {
            let event = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
                .await
                .unwrap()
                .unwrap();
            if let AgentEvent::HealthStatusChanged { from, to, .. } = &event {
                assert_eq!(from, "healthy");
                assert_eq!(to, "unhealthy");
                got_change = true;
                break;
            }
        }
        assert!(got_change, "Expected HealthStatusChanged event");
    }

    #[tokio::test]
    async fn test_health_status_changed_unhealthy_to_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let bus = MessageBus::new();
        let mut events_rx = bus.subscribe_events();
        let mut svc = make_service(dir.path());
        svc.set_bus(bus);

        let check = Arc::new(ToggleCheck::new("comp", false));
        svc.register_check(check.clone());

        // Start unhealthy
        svc.run_checks().await.unwrap();

        // Now recover
        check.set_healthy(true);
        svc.run_checks().await.unwrap();

        let mut got_change = false;
        for _ in 0..5 {
            let event = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
                .await
                .unwrap()
                .unwrap();
            if let AgentEvent::HealthStatusChanged { from, to, .. } = &event {
                assert_eq!(from, "unhealthy");
                assert_eq!(to, "healthy");
                got_change = true;
                break;
            }
        }
        assert!(got_change, "Expected HealthStatusChanged event on recovery");
    }

    #[tokio::test]
    async fn test_no_change_event_on_first_check() {
        let dir = tempfile::tempdir().unwrap();
        let bus = MessageBus::new();
        let mut events_rx = bus.subscribe_events();
        let mut svc = make_service(dir.path());
        svc.set_bus(bus);

        svc.register_check(Arc::new(MockCheck {
            name: "comp".to_string(),
            healthy: true,
        }));

        svc.run_checks().await.unwrap();

        // Should only get HeartbeatCheck, no HealthStatusChanged
        let event = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, AgentEvent::HeartbeatCheck { .. }));
    }

    #[tokio::test]
    async fn test_no_change_event_when_status_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let bus = MessageBus::new();
        let mut events_rx = bus.subscribe_events();
        let mut svc = make_service(dir.path());
        svc.set_bus(bus);

        svc.register_check(Arc::new(MockCheck {
            name: "comp".to_string(),
            healthy: true,
        }));

        // Two checks, both healthy
        svc.run_checks().await.unwrap();
        svc.run_checks().await.unwrap();

        // Should only get HeartbeatCheck events, no HealthStatusChanged
        let mut change_count = 0;
        for _ in 0..10 {
            match tokio::time::timeout(Duration::from_millis(100), events_rx.recv()).await {
                Ok(Ok(AgentEvent::HealthStatusChanged { .. })) => change_count += 1,
                _ => break,
            }
        }
        assert_eq!(
            change_count, 0,
            "Should not emit change when status unchanged"
        );
    }

    // === ComponentStatusChanged events ===

    #[tokio::test]
    async fn test_component_status_changed_on_transition() {
        let dir = tempfile::tempdir().unwrap();
        let bus = MessageBus::new();
        let mut events_rx = bus.subscribe_events();
        let mut svc = make_service(dir.path());
        svc.set_bus(bus);

        let check = Arc::new(ToggleCheck::new("comp", true));
        svc.register_check(check.clone());

        // First check — healthy (no previous, so no ComponentStatusChanged)
        svc.run_checks().await.unwrap();

        // Fail the component
        check.set_healthy(false);
        svc.run_checks().await.unwrap();

        // Should receive ComponentStatusChanged event
        let mut got_component_change = false;
        for _ in 0..10 {
            let event = tokio::time::timeout(Duration::from_millis(100), events_rx.recv())
                .await
                .unwrap()
                .unwrap();
            if let AgentEvent::ComponentStatusChanged {
                component,
                from,
                to,
                ..
            } = &event
            {
                if component == "comp" && from == "Healthy" && to == "Unhealthy" {
                    got_component_change = true;
                    break;
                }
            }
        }
        assert!(
            got_component_change,
            "Expected ComponentStatusChanged event"
        );
    }

    #[tokio::test]
    async fn test_component_status_no_change_when_stable() {
        let dir = tempfile::tempdir().unwrap();
        let bus = MessageBus::new();
        let mut events_rx = bus.subscribe_events();
        let mut svc = make_service(dir.path());
        svc.set_bus(bus);

        svc.register_check(Arc::new(MockCheck {
            name: "comp".to_string(),
            healthy: true,
        }));

        svc.run_checks().await.unwrap();
        svc.run_checks().await.unwrap();

        let mut component_changes = 0;
        for _ in 0..10 {
            match tokio::time::timeout(Duration::from_millis(100), events_rx.recv()).await {
                Ok(Ok(AgentEvent::ComponentStatusChanged { .. })) => component_changes += 1,
                _ => break,
            }
        }
        assert_eq!(
            component_changes, 0,
            "Should not emit ComponentStatusChanged when stable"
        );
    }

    #[tokio::test]
    async fn test_component_recovery_event() {
        let dir = tempfile::tempdir().unwrap();
        let bus = MessageBus::new();
        let mut events_rx = bus.subscribe_events();
        let mut svc = make_service(dir.path());
        svc.set_bus(bus);

        let check = Arc::new(ToggleCheck::new("comp", false));
        svc.register_check(check.clone());

        // Start unhealthy
        svc.run_checks().await.unwrap();

        // Recover
        check.set_healthy(true);
        svc.run_checks().await.unwrap();

        let mut got_recovery = false;
        for _ in 0..10 {
            let event = tokio::time::timeout(Duration::from_millis(100), events_rx.recv())
                .await
                .unwrap()
                .unwrap();
            if let AgentEvent::ComponentStatusChanged {
                component,
                from,
                to,
                ..
            } = &event
            {
                if component == "comp" && from == "Unhealthy" && to == "Healthy" {
                    got_recovery = true;
                    break;
                }
            }
        }
        assert!(
            got_recovery,
            "Expected ComponentStatusChanged recovery event"
        );
    }

    // === Full health report ===

    #[tokio::test]
    async fn test_generate_full_report_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.register_check(Arc::new(MockCheck {
            name: "comp_a".to_string(),
            healthy: true,
        }));
        svc.register_check(Arc::new(MockCheck {
            name: "comp_b".to_string(),
            healthy: true,
        }));

        let report = svc.generate_full_report().await.unwrap();
        assert_eq!(report.overall_status, "healthy");
        assert_eq!(report.total_components, 2);
        assert_eq!(report.healthy_count, 2);
        assert_eq!(report.failed_count, 0);
        assert_eq!(report.components.len(), 2);
    }

    #[tokio::test]
    async fn test_generate_full_report_unhealthy() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.register_check(Arc::new(MockCheck {
            name: "healthy_comp".to_string(),
            healthy: true,
        }));
        svc.register_check(Arc::new(MockCheck {
            name: "broken_comp".to_string(),
            healthy: false,
        }));

        let report = svc.generate_full_report().await.unwrap();
        assert_eq!(report.overall_status, "unhealthy");
        assert_eq!(report.healthy_count, 1);
        assert_eq!(report.failed_count, 1);
    }

    #[tokio::test]
    async fn test_cached_full_report_none_initially() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        assert!(svc.cached_full_report().is_none());
    }

    #[tokio::test]
    async fn test_cached_full_report_after_checks() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.register_check(Arc::new(MockCheck {
            name: "comp".to_string(),
            healthy: true,
        }));

        svc.run_checks().await.unwrap();
        let report = svc.cached_full_report();
        assert!(report.is_some());
        assert_eq!(report.unwrap().overall_status, "healthy");
    }

    #[tokio::test]
    async fn test_full_report_tracks_state() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.register_check(Arc::new(MockCheck {
            name: "comp".to_string(),
            healthy: true,
        }));

        // Run checks twice
        svc.run_checks().await.unwrap();
        svc.run_checks().await.unwrap();

        let report = svc.generate_full_report().await.unwrap();
        assert_eq!(report.total_checks_run, 3); // 2 from run_checks + 1 from generate_full_report
    }

    #[test]
    fn test_with_registries_stores_dependencies() {
        let config = nanobot_config::Config::default();
        let tmp = tempfile::tempdir().unwrap();
        let sessions = SessionManager::new(tmp.path().to_path_buf()).unwrap();
        sessions.get_or_create("session-1", None);

        let mut providers = ProviderRegistry::new();
        providers.register("mock", MockProvider);
        providers.set_default("mock");

        let tools = nanobot_tools::ToolRegistry::new();
        nanobot_tools::builtins::register_all(&tools);

        let service = HeartbeatService::with_registries(config, providers, tools, sessions.clone());

        let attached_providers = service.provider_registry().unwrap();
        let attached_tools = service.tool_registry().unwrap();
        let attached_sessions = service.session_manager().unwrap();

        assert_eq!(
            attached_providers.provider_names(),
            vec!["mock".to_string()]
        );
        assert!(!attached_tools.tool_names().is_empty());
        assert_eq!(attached_sessions.session_count(), 1);
    }

    struct MockProvider;

    #[async_trait::async_trait]
    impl nanobot_providers::LlmProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        async fn complete(
            &self,
            _request: nanobot_providers::CompletionRequest,
        ) -> anyhow::Result<nanobot_providers::CompletionResponse> {
            Ok(nanobot_providers::CompletionResponse {
                content: Some("ok".to_string()),
                tool_calls: None,
                usage: None,
                finish_reason: Some("stop".to_string()),
            })
        }

        async fn complete_stream(
            &self,
            request: nanobot_providers::CompletionRequest,
        ) -> anyhow::Result<nanobot_providers::base::BoxStream> {
            use nanobot_providers::base::CompletionChunk;

            let response = self.complete(request).await?;
            let chunk = CompletionChunk {
                delta: response.content,
                tool_call_deltas: None,
                usage: response.usage,
                done: true,
            };
            Ok(Box::pin(futures::stream::once(async move { Ok(chunk) })))
        }

        fn supports_model(&self, _model: &str) -> bool {
            true
        }
    }
}
