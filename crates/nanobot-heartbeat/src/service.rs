//! Heartbeat service — periodic health checks, auto-restart, and state persistence.
//!
//! Runs on a configurable interval and checks:
//! 1. Agent process (provider reachable)
//! 2. Channel connections (bus has subscribers)
//! 3. Provider health (lightweight completion test)
//!
//! When failures are detected, emits restart requests via the bus.
//! State is persisted to `heartbeat_state.json` for observability.

use crate::types::*;
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use nanobot_bus::events::AgentEvent;
use nanobot_bus::MessageBus;
use nanobot_config::Config;
use nanobot_core::{Message, MessageRole};
use nanobot_providers::ProviderRegistry;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// The heartbeat service.
pub struct HeartbeatService {
    config: Arc<Config>,
    provider_registry: Arc<ProviderRegistry>,
    interval: Duration,
    running: Arc<RwLock<bool>>,
    bus: Option<Arc<MessageBus>>,
    state_path: PathBuf,
    state: parking_lot::Mutex<HeartbeatState>,
}

impl HeartbeatService {
    /// Create a new heartbeat service with default state directory.
    pub fn new(config: Config) -> Self {
        let interval_secs = config.heartbeat.interval_secs.max(60);
        let data_dir = nanobot_config::paths::get_data_dir()
            .unwrap_or_else(|_| std::env::temp_dir());
        let state_path = data_dir.join("heartbeat_state.json");
        let state = load_state(&state_path).unwrap_or_default();

        Self {
            config: Arc::new(config),
            provider_registry: Arc::new(ProviderRegistry::new()),
            interval: Duration::from_secs(interval_secs),
            running: Arc::new(RwLock::new(false)),
            bus: None,
            state_path,
            state: parking_lot::Mutex::new(state),
        }
    }

    /// Create with a custom data directory.
    pub fn with_data_dir(config: Config, data_dir: PathBuf) -> Self {
        let interval_secs = config.heartbeat.interval_secs.max(60);
        let state_path = data_dir.join("heartbeat_state.json");
        let state = load_state(&state_path).unwrap_or_default();

        Self {
            config: Arc::new(config),
            provider_registry: Arc::new(ProviderRegistry::new()),
            interval: Duration::from_secs(interval_secs),
            running: Arc::new(RwLock::new(false)),
            bus: None,
            state_path,
            state: parking_lot::Mutex::new(state),
        }
    }

    /// Create with full registries and bus for real health checks.
    pub fn with_registries(
        config: Config,
        provider_registry: ProviderRegistry,
        bus: MessageBus,
        data_dir: PathBuf,
    ) -> Self {
        let interval_secs = config.heartbeat.interval_secs.max(60);
        let state_path = data_dir.join("heartbeat_state.json");
        let state = load_state(&state_path).unwrap_or_default();

        Self {
            config: Arc::new(config),
            provider_registry: Arc::new(provider_registry),
            interval: Duration::from_secs(interval_secs),
            running: Arc::new(RwLock::new(false)),
            bus: Some(Arc::new(bus)),
            state_path,
            state: parking_lot::Mutex::new(state),
        }
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
            "Heartbeat service started (interval: {}s)",
            self.interval.as_secs()
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
                    if snapshot.healthy {
                        debug!(
                            "Heartbeat: all {} checks healthy",
                            snapshot.checks.len()
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
        let mut checks = Vec::new();
        let now = Local::now();

        // Check 1: Provider / agent process health
        checks.push(self.check_provider(now).await);

        // Check 2: Bus connectivity
        checks.push(self.check_bus(now));

        // Check 3: Session store health
        checks.push(self.check_session_store(now));

        let snapshot = HealthSnapshot::from_checks(checks);

        // Update persistent state
        {
            let mut state = self.state.lock();
            state.total_checks += 1;
            state.total_failures += snapshot.failed_count();
            state.last_snapshot = Some(snapshot.clone());
        }

        // Emit event via bus
        if let Some(bus) = &self.bus {
            bus.emit_event(AgentEvent::HeartbeatCheck {
                healthy: snapshot.healthy,
                checks_total: snapshot.checks.len(),
                checks_failed: snapshot.failed_count(),
            });

            // Auto-restart failed components
            for check in &snapshot.checks {
                if check.status == CheckStatus::Unhealthy {
                    bus.emit_event(AgentEvent::RestartRequested {
                        component: check.component.clone(),
                        reason: check.message.clone(),
                    });
                    self.state.lock().restarts_requested += 1;
                }
            }
        }

        let _ = self.persist_state();
        Ok(snapshot)
    }

    /// Check provider health — attempt a lightweight LLM completion.
    async fn check_provider(&self, now: DateTime<Local>) -> HealthCheckResult {
        let model = &self.config.agent.model;
        let provider = match self.provider_registry.get_provider(model) {
            Some(p) => p,
            None => {
                return HealthCheckResult {
                    component: "provider".to_string(),
                    status: CheckStatus::Skipped,
                    message: format!("No provider for model '{}'", model),
                    timestamp: now,
                };
            }
        };

        let request = nanobot_providers::CompletionRequest {
            model: model.clone(),
            messages: vec![Message {
                role: MessageRole::User,
                content: "ping".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            tools: None,
            max_tokens: Some(5),
            temperature: Some(0.0),
            stream: false,
        };

        match provider.complete(request).await {
            Ok(_) => HealthCheckResult {
                component: "provider".to_string(),
                status: CheckStatus::Healthy,
                message: format!("Provider '{}' responding", provider.name()),
                timestamp: now,
            },
            Err(e) => HealthCheckResult {
                component: "provider".to_string(),
                status: CheckStatus::Unhealthy,
                message: format!("Provider '{}' error: {}", provider.name(), e),
                timestamp: now,
            },
        }
    }

    /// Check bus connectivity.
    fn check_bus(&self, now: DateTime<Local>) -> HealthCheckResult {
        match &self.bus {
            Some(_) => HealthCheckResult {
                component: "bus".to_string(),
                status: CheckStatus::Healthy,
                message: "Bus connected".to_string(),
                timestamp: now,
            },
            None => HealthCheckResult {
                component: "bus".to_string(),
                status: CheckStatus::Skipped,
                message: "Bus not configured".to_string(),
                timestamp: now,
            },
        }
    }

    /// Check session store health by verifying the data directory exists.
    fn check_session_store(&self, now: DateTime<Local>) -> HealthCheckResult {
        let data_dir = self.state_path.parent();
        match data_dir {
            Some(dir) if dir.exists() => HealthCheckResult {
                component: "session_store".to_string(),
                status: CheckStatus::Healthy,
                message: "Data directory accessible".to_string(),
                timestamp: now,
            },
            _ => HealthCheckResult {
                component: "session_store".to_string(),
                status: CheckStatus::Unhealthy,
                message: "Data directory not found".to_string(),
                timestamp: now,
            },
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

    /// Persist state to disk.
    fn persist_state(&self) -> Result<()> {
        let state = self.state.lock();
        let json = serde_json::to_string_pretty(&*state)
            .with_context(|| "Failed to serialize heartbeat state")?;
        std::fs::write(&self.state_path, json)
            .with_context(|| format!("Failed to write {}", self.state_path.display()))?;
        Ok(())
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
    use nanobot_config::Config;
    use std::time::Duration;

    fn make_service(dir: &std::path::Path) -> HeartbeatService {
        let config = Config::default();
        HeartbeatService::with_data_dir(config, dir.to_path_buf())
    }

    fn make_service_with_bus(dir: &std::path::Path) -> HeartbeatService {
        let config = Config::default();
        let bus = MessageBus::new();
        let providers = ProviderRegistry::new();
        HeartbeatService::with_registries(config, providers, bus, dir.to_path_buf())
    }

    // === Construction ===

    #[test]
    fn test_construction() {
        let config = Config::default();
        let svc = HeartbeatService::new(config);
        assert!(svc.interval() >= Duration::from_secs(60));
    }

    #[test]
    fn test_construction_with_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        let svc = HeartbeatService::with_data_dir(config, dir.path().to_path_buf());
        assert!(svc.interval() >= Duration::from_secs(60));
    }

    #[test]
    fn test_construction_with_bus() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        let bus = MessageBus::new();
        let providers = ProviderRegistry::new();
        let svc = HeartbeatService::with_registries(config, providers, bus, dir.path().to_path_buf());
        assert!(svc.interval() >= Duration::from_secs(60));
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

    #[tokio::test]
    async fn test_stop_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        svc.stop().await;
        svc.stop().await;
        assert!(!svc.is_running().await);
    }

    // === Interval config ===

    #[test]
    fn test_interval_minimum_60s() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.heartbeat.interval_secs = 10;
        let svc = HeartbeatService::with_data_dir(config, dir.path().to_path_buf());
        assert!(svc.interval() >= Duration::from_secs(60));
    }

    #[test]
    fn test_interval_uses_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.heartbeat.interval_secs = 300;
        let svc = HeartbeatService::with_data_dir(config, dir.path().to_path_buf());
        assert_eq!(svc.interval(), Duration::from_secs(300));
    }

    #[test]
    fn test_default_disabled() {
        let config = Config::default();
        assert!(!config.heartbeat.enabled);
    }

    // === Health checks ===

    #[tokio::test]
    async fn test_run_checks_no_provider() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let snapshot = svc.run_checks().await.unwrap();
        // No provider → Skipped, no bus → Skipped, session_store → Healthy
        assert!(snapshot.healthy); // All skipped or healthy
        assert_eq!(snapshot.checks.len(), 3);
        // Provider should be skipped
        let provider_check = snapshot.checks.iter().find(|c| c.component == "provider").unwrap();
        assert_eq!(provider_check.status, CheckStatus::Skipped);
    }

    #[tokio::test]
    async fn test_run_checks_with_bus() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service_with_bus(dir.path());
        let snapshot = svc.run_checks().await.unwrap();
        assert!(snapshot.healthy);
        // Bus should be healthy
        let bus_check = snapshot.checks.iter().find(|c| c.component == "bus").unwrap();
        assert_eq!(bus_check.status, CheckStatus::Healthy);
    }

    #[tokio::test]
    async fn test_run_checks_session_store() {
        let dir = tempfile::tempdir().unwrap();
        let svc = make_service(dir.path());
        let snapshot = svc.run_checks().await.unwrap();
        let store_check = snapshot
            .checks
            .iter()
            .find(|c| c.component == "session_store")
            .unwrap();
        assert_eq!(store_check.status, CheckStatus::Healthy);
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

    // === Bus events ===

    #[tokio::test]
    async fn test_heartbeat_event_emitted() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        let bus = MessageBus::new();
        let mut events_rx = bus.subscribe_events();
        let providers = ProviderRegistry::new();
        let svc = HeartbeatService::with_registries(config, providers, bus, dir.path().to_path_buf());

        svc.run_checks().await.unwrap();

        // Should receive a HeartbeatCheck event
        let event = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, AgentEvent::HeartbeatCheck { healthy: true, .. }));
    }

    #[tokio::test]
    async fn test_no_restart_event_when_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        let bus = MessageBus::new();
        let providers = ProviderRegistry::new();
        let svc = HeartbeatService::with_registries(config, providers, bus, dir.path().to_path_buf());

        svc.run_checks().await.unwrap();
        assert_eq!(svc.state().restarts_requested, 0);
    }

    // === State persistence ===

    #[test]
    fn test_state_persists_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
        let svc = HeartbeatService::with_data_dir(config, dir.path().to_path_buf());

        // Manually update state and persist
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
            svc.run_checks().await.unwrap();
        }

        assert!(state_path.exists());
        let loaded: HeartbeatState =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        assert_eq!(loaded.total_checks, 1);
        assert!(loaded.last_snapshot.is_some());
    }

    #[tokio::test]
    async fn test_state_persists_start_stop() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("heartbeat_state.json");

        {
            let svc = make_service(dir.path());
            // Simulate start (manually set state)
            svc.state.lock().started_at = Some(Local::now());
            svc.persist_state().unwrap();
        }

        let loaded: HeartbeatState =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        assert!(loaded.started_at.is_some());
    }

    #[test]
    fn test_state_file_is_heartbeat_state_json() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::default();
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

        for _ in 0..5 {
            svc.run_checks().await.unwrap();
        }

        let state = svc.state();
        assert_eq!(state.total_checks, 5);
    }
}
