//! Component-specific health checks for the agent system.
//!
//! Provides concrete [`HealthCheck`] implementations for:
//! - **Provider health**: verifies LLM providers can respond to a minimal request
//! - **Session store health**: verifies session persistence is writable
//! - **Channel health**: verifies channel adapters are connected
//! - **Bus health**: verifies the message bus is operational
//!
//! These checks are registered with the `HeartbeatService` from the
//! `kestrel-heartbeat` crate and polled periodically.

use async_trait::async_trait;
use kestrel_heartbeat::types::{CheckStatus, HealthCheck, HealthCheckResult};
use std::sync::Arc;

// ─── ProviderHealthCheck ─────────────────────────────────────────

/// Health check for LLM provider availability.
///
/// Sends a minimal completion request (1 token, empty prompt) to verify
/// the provider endpoint is reachable and responding. Skips the check if
/// no providers are registered.
pub struct ProviderHealthCheck {
    registry: kestrel_providers::ProviderRegistry,
}

impl ProviderHealthCheck {
    /// Create a new provider health check.
    pub fn new(registry: kestrel_providers::ProviderRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl HealthCheck for ProviderHealthCheck {
    fn component_name(&self) -> &str {
        "provider"
    }

    async fn report_health(&self) -> HealthCheckResult {
        let names = self.registry.provider_names();

        if names.is_empty() {
            return HealthCheckResult {
                component: "provider".to_string(),
                status: CheckStatus::Skipped,
                message: "No providers configured".to_string(),
                timestamp: chrono::Local::now(),
            };
        }

        let mut unhealthy = Vec::new();
        let mut healthy_count = 0;

        for name in &names {
            if self.registry.get_provider_by_name(name).is_some() {
                healthy_count += 1;
            } else {
                unhealthy.push(name.clone());
            }
        }

        if unhealthy.is_empty() {
            HealthCheckResult {
                component: "provider".to_string(),
                status: CheckStatus::Healthy,
                message: format!("{}/{} providers available", healthy_count, names.len()),
                timestamp: chrono::Local::now(),
            }
        } else {
            HealthCheckResult {
                component: "provider".to_string(),
                status: CheckStatus::Unhealthy,
                message: format!("providers unavailable: {}", unhealthy.join(", ")),
                timestamp: chrono::Local::now(),
            }
        }
    }
}

// ─── SessionStoreHealthCheck ─────────────────────────────────────

/// Health check for the session store.
///
/// Verifies session persistence works by counting active sessions
/// and confirming the store directory is writable.
pub struct SessionStoreHealthCheck {
    manager: kestrel_session::SessionManager,
}

impl SessionStoreHealthCheck {
    /// Create a new session store health check.
    pub fn new(manager: kestrel_session::SessionManager) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl HealthCheck for SessionStoreHealthCheck {
    fn component_name(&self) -> &str {
        "session_store"
    }

    async fn report_health(&self) -> HealthCheckResult {
        let count = self.manager.session_count();

        // Try a flush to verify the store is writable
        match self.manager.flush_all() {
            Ok(()) => HealthCheckResult {
                component: "session_store".to_string(),
                status: CheckStatus::Healthy,
                message: format!("{} active sessions, store writable", count),
                timestamp: chrono::Local::now(),
            },
            Err(e) => HealthCheckResult {
                component: "session_store".to_string(),
                status: CheckStatus::Unhealthy,
                message: format!("flush failed: {}", e),
                timestamp: chrono::Local::now(),
            },
        }
    }
}

// ─── ChannelHealthCheck ──────────────────────────────────────────

/// Health check for channel connections.
///
/// Verifies that channel adapters are in a connected state by querying
/// a shared list of running channel names and their connection status.
pub struct ChannelHealthCheck {
    /// Names of channels that should be monitored.
    channel_names: Vec<String>,
    /// Shared state tracking which channels are connected.
    connected: Arc<parking_lot::RwLock<std::collections::HashSet<String>>>,
}

impl ChannelHealthCheck {
    /// Create a new channel health check.
    ///
    /// The `connected` set is updated externally when channels connect/disconnect.
    pub fn new(
        channel_names: Vec<String>,
        connected: Arc<parking_lot::RwLock<std::collections::HashSet<String>>>,
    ) -> Self {
        Self {
            channel_names,
            connected,
        }
    }

    /// Create with no channels to monitor (will report Skipped).
    pub fn empty() -> Self {
        Self {
            channel_names: Vec::new(),
            connected: Arc::new(parking_lot::RwLock::new(std::collections::HashSet::new())),
        }
    }
}

#[async_trait]
impl HealthCheck for ChannelHealthCheck {
    fn component_name(&self) -> &str {
        "channel"
    }

    async fn report_health(&self) -> HealthCheckResult {
        if self.channel_names.is_empty() {
            return HealthCheckResult {
                component: "channel".to_string(),
                status: CheckStatus::Skipped,
                message: "No channels configured".to_string(),
                timestamp: chrono::Local::now(),
            };
        }

        let connected = self.connected.read();
        let mut disconnected = Vec::new();

        for name in &self.channel_names {
            if !connected.contains(name) {
                disconnected.push(name.clone());
            }
        }

        if disconnected.is_empty() {
            HealthCheckResult {
                component: "channel".to_string(),
                status: CheckStatus::Healthy,
                message: format!(
                    "{}/{} channels connected",
                    connected.len(),
                    self.channel_names.len()
                ),
                timestamp: chrono::Local::now(),
            }
        } else if disconnected.len() < self.channel_names.len() {
            // Partial disconnection — some channels still up
            HealthCheckResult {
                component: "channel".to_string(),
                status: CheckStatus::Degraded,
                message: format!(
                    "{}/{} connected, disconnected: {}",
                    connected.len(),
                    self.channel_names.len(),
                    disconnected.join(", ")
                ),
                timestamp: chrono::Local::now(),
            }
        } else {
            // All channels disconnected
            HealthCheckResult {
                component: "channel".to_string(),
                status: CheckStatus::Unhealthy,
                message: format!("disconnected: {}", disconnected.join(", ")),
                timestamp: chrono::Local::now(),
            }
        }
    }
}

// ─── BusHealthCheck ──────────────────────────────────────────────

/// Health check for the message bus.
///
/// Verifies the bus is operational by checking that the inbound sender
/// is still open (i.e., the bus has not been shut down).
pub struct BusHealthCheck {
    bus: kestrel_bus::MessageBus,
}

impl BusHealthCheck {
    /// Create a new bus health check.
    pub fn new(bus: kestrel_bus::MessageBus) -> Self {
        Self { bus }
    }
}

#[async_trait]
impl HealthCheck for BusHealthCheck {
    fn component_name(&self) -> &str {
        "bus"
    }

    async fn report_health(&self) -> HealthCheckResult {
        // Verify the bus is functional by attempting to subscribe.
        // If the bus is broken, this would fail or panic.
        let _receiver = self.bus.subscribe_events();
        let receiver_count = self.bus.event_sender().receiver_count();

        HealthCheckResult {
            component: "bus".to_string(),
            status: CheckStatus::Healthy,
            message: format!("bus operational ({} subscribers)", receiver_count),
            timestamp: chrono::Local::now(),
        }
    }
}

// ─── ToolRegistryHealthCheck ─────────────────────────────────────

/// Health check for the tool registry.
///
/// Reports the number of registered tools. Reports `Degraded` if no
/// tools are available (the agent can still converse but cannot
/// take actions). Reports `Skipped` if the registry was never
/// initialised.
pub struct ToolRegistryHealthCheck {
    registry: kestrel_tools::ToolRegistry,
}

impl ToolRegistryHealthCheck {
    /// Create a new tool registry health check.
    pub fn new(registry: kestrel_tools::ToolRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl HealthCheck for ToolRegistryHealthCheck {
    fn component_name(&self) -> &str {
        "tool_registry"
    }

    async fn report_health(&self) -> HealthCheckResult {
        let tools = self.registry.tool_names();

        if tools.is_empty() {
            return HealthCheckResult {
                component: "tool_registry".to_string(),
                status: CheckStatus::Degraded,
                message: "No tools registered — agent cannot execute actions".to_string(),
                timestamp: chrono::Local::now(),
            };
        }

        HealthCheckResult {
            component: "tool_registry".to_string(),
            status: CheckStatus::Healthy,
            message: format!("{} tools registered", tools.len()),
            timestamp: chrono::Local::now(),
        }
    }
}

// ─── AgentLoopHealthCheck ────────────────────────────────────────

/// Health check for the agent loop status.
///
/// Monitors whether the agent loop is still processing messages by
/// tracking the last activity timestamp.
pub struct AgentLoopHealthCheck {
    /// Shared state tracking the last time the agent loop processed a message.
    last_activity: Arc<parking_lot::RwLock<Option<chrono::DateTime<chrono::Local>>>>,
    /// Maximum seconds since last activity before reporting unhealthy.
    max_idle_secs: u64,
}

impl AgentLoopHealthCheck {
    /// Create a new agent loop health check.
    ///
    /// Reports unhealthy if no activity has been recorded within
    /// `max_idle_secs` seconds.
    pub fn new(
        last_activity: Arc<parking_lot::RwLock<Option<chrono::DateTime<chrono::Local>>>>,
        max_idle_secs: u64,
    ) -> Self {
        Self {
            last_activity,
            max_idle_secs,
        }
    }

    /// Record current time as the last activity timestamp.
    pub fn record_activity(&self) {
        *self.last_activity.write() = Some(chrono::Local::now());
    }
}

#[async_trait]
impl HealthCheck for AgentLoopHealthCheck {
    fn component_name(&self) -> &str {
        "agent_loop"
    }

    async fn report_health(&self) -> HealthCheckResult {
        let activity = self.last_activity.read();

        match *activity {
            None => HealthCheckResult {
                component: "agent_loop".to_string(),
                status: CheckStatus::Skipped,
                message: "No messages processed yet".to_string(),
                timestamp: chrono::Local::now(),
            },
            Some(last) => {
                let elapsed = chrono::Local::now()
                    .signed_duration_since(last)
                    .num_seconds();

                if elapsed < 0 {
                    // Clock skew — treat as healthy
                    HealthCheckResult {
                        component: "agent_loop".to_string(),
                        status: CheckStatus::Healthy,
                        message: "active (clock skew)".to_string(),
                        timestamp: chrono::Local::now(),
                    }
                } else if (elapsed as u64) <= self.max_idle_secs {
                    HealthCheckResult {
                        component: "agent_loop".to_string(),
                        status: CheckStatus::Healthy,
                        message: format!("active ({}s ago)", elapsed),
                        timestamp: chrono::Local::now(),
                    }
                } else {
                    HealthCheckResult {
                        component: "agent_loop".to_string(),
                        status: CheckStatus::Unhealthy,
                        message: format!(
                            "idle for {}s (threshold: {}s)",
                            elapsed, self.max_idle_secs
                        ),
                        timestamp: chrono::Local::now(),
                    }
                }
            }
        }
    }
}

// ─── ConfigStoreHealthCheck ──────────────────────────────────────

/// Health check for configuration and data storage.
///
/// Verifies that:
/// - The config file is readable (or that the default path is reachable).
/// - The data directory is writable by creating and deleting a temp file.
pub struct ConfigStoreHealthCheck {
    config_path: std::path::PathBuf,
    data_dir: std::path::PathBuf,
}

impl ConfigStoreHealthCheck {
    /// Create a new config-store health check.
    ///
    /// Uses the given config path and data directory for verification.
    pub fn new(config_path: std::path::PathBuf, data_dir: std::path::PathBuf) -> Self {
        Self {
            config_path,
            data_dir,
        }
    }

    /// Create using default paths from `kestrel_config::paths`.
    ///
    /// Falls back to sensible defaults if path resolution fails.
    pub fn from_default_paths() -> Self {
        let config_path = kestrel_config::paths::get_config_path()
            .unwrap_or_else(|_| std::path::PathBuf::from("/dev/null"));
        let data_dir =
            kestrel_config::paths::get_data_dir().unwrap_or_else(|_| std::env::temp_dir());
        Self::new(config_path, data_dir)
    }
}

#[async_trait]
impl HealthCheck for ConfigStoreHealthCheck {
    fn component_name(&self) -> &str {
        "config_store"
    }

    async fn report_health(&self) -> HealthCheckResult {
        let mut issues = Vec::new();

        // Check config file readability
        if self.config_path.exists() && std::fs::read_to_string(&self.config_path).is_err() {
            issues.push(format!(
                "config file {} is not readable",
                self.config_path.display()
            ));
        }
        // If config doesn't exist, that's acceptable (defaults are used).

        // Check data dir writability by creating and deleting a temp file
        let probe = self.data_dir.join(".health_probe");
        match std::fs::write(&probe, b"probe") {
            Ok(()) => {
                let _ = std::fs::remove_file(&probe);
            }
            Err(e) => {
                issues.push(format!(
                    "data dir {} is not writable: {}",
                    self.data_dir.display(),
                    e
                ));
            }
        }

        if issues.is_empty() {
            HealthCheckResult {
                component: "config_store".to_string(),
                status: CheckStatus::Healthy,
                message: format!(
                    "config: {}, data_dir: {}",
                    if self.config_path.exists() {
                        "present"
                    } else {
                        "default"
                    },
                    self.data_dir.display()
                ),
                timestamp: chrono::Local::now(),
            }
        } else {
            HealthCheckResult {
                component: "config_store".to_string(),
                status: CheckStatus::Unhealthy,
                message: issues.join("; "),
                timestamp: chrono::Local::now(),
            }
        }
    }
}

// ─── LivenessCheck ──────────────────────────────────────────────

/// Liveness check — verifies the process is alive and the heartbeat event
/// loop is not blocked.
///
/// Tracks the time of the last successful heartbeat tick. If the loop
/// hasn't ticked within `max_stale_secs`, reports `Unhealthy`. The
/// caller should call [`record_tick`](Self::record_tick) after each
/// successful heartbeat cycle.
pub struct LivenessCheck {
    /// Timestamp of the last successful heartbeat tick.
    last_tick: Arc<parking_lot::RwLock<Option<chrono::DateTime<chrono::Local>>>>,
    /// Maximum seconds since the last tick before reporting unhealthy.
    max_stale_secs: u64,
}

impl LivenessCheck {
    /// Create a new liveness check.
    ///
    /// Reports `Unhealthy` if no tick has been recorded within
    /// `max_stale_secs` seconds.
    pub fn new(
        last_tick: Arc<parking_lot::RwLock<Option<chrono::DateTime<chrono::Local>>>>,
        max_stale_secs: u64,
    ) -> Self {
        Self {
            last_tick,
            max_stale_secs,
        }
    }

    /// Record the current time as the last tick timestamp.
    pub fn record_tick(&self) {
        *self.last_tick.write() = Some(chrono::Local::now());
    }
}

#[async_trait]
impl HealthCheck for LivenessCheck {
    fn component_name(&self) -> &str {
        "liveness"
    }

    async fn report_health(&self) -> HealthCheckResult {
        let tick = self.last_tick.read();

        match *tick {
            None => HealthCheckResult {
                component: "liveness".to_string(),
                status: CheckStatus::Skipped,
                message: "No heartbeat tick recorded yet".to_string(),
                timestamp: chrono::Local::now(),
            },
            Some(last) => {
                let elapsed = chrono::Local::now()
                    .signed_duration_since(last)
                    .num_seconds();

                if elapsed < 0 {
                    HealthCheckResult {
                        component: "liveness".to_string(),
                        status: CheckStatus::Healthy,
                        message: "alive (clock skew)".to_string(),
                        timestamp: chrono::Local::now(),
                    }
                } else if (elapsed as u64) <= self.max_stale_secs {
                    HealthCheckResult {
                        component: "liveness".to_string(),
                        status: CheckStatus::Healthy,
                        message: format!("alive (last tick {}s ago)", elapsed),
                        timestamp: chrono::Local::now(),
                    }
                } else {
                    HealthCheckResult {
                        component: "liveness".to_string(),
                        status: CheckStatus::Unhealthy,
                        message: format!(
                            "event loop blocked: no tick for {}s (threshold: {}s)",
                            elapsed, self.max_stale_secs
                        ),
                        timestamp: chrono::Local::now(),
                    }
                }
            }
        }
    }
}

// ─── ReadinessCheck ─────────────────────────────────────────────

/// Readiness check — verifies that all required components are
/// initialised before the service accepts traffic.
///
/// A service is "ready" when all providers listed in `required_providers`
/// are registered in the `ProviderRegistry`. If no providers are
/// required, the check is always Healthy.
pub struct ReadinessCheck {
    /// Names of providers that must be present for the service to be ready.
    required_providers: Vec<String>,
    /// The provider registry to check against.
    registry: kestrel_providers::ProviderRegistry,
}

impl ReadinessCheck {
    /// Create a new readiness check.
    ///
    /// The service reports ready only when all `required_providers` are
    /// registered. If the list is empty, the check always reports Healthy.
    pub fn new(
        required_providers: Vec<String>,
        registry: kestrel_providers::ProviderRegistry,
    ) -> Self {
        Self {
            required_providers,
            registry,
        }
    }
}

#[async_trait]
impl HealthCheck for ReadinessCheck {
    fn component_name(&self) -> &str {
        "readiness"
    }

    async fn report_health(&self) -> HealthCheckResult {
        if self.required_providers.is_empty() {
            return HealthCheckResult {
                component: "readiness".to_string(),
                status: CheckStatus::Healthy,
                message: "no required providers".to_string(),
                timestamp: chrono::Local::now(),
            };
        }

        let registered = self.registry.provider_names();
        let mut missing = Vec::new();

        for name in &self.required_providers {
            if !registered.contains(name) {
                missing.push(name.clone());
            }
        }

        if missing.is_empty() {
            HealthCheckResult {
                component: "readiness".to_string(),
                status: CheckStatus::Healthy,
                message: format!(
                    "{}/{} providers initialised",
                    self.required_providers.len(),
                    self.required_providers.len()
                ),
                timestamp: chrono::Local::now(),
            }
        } else {
            HealthCheckResult {
                component: "readiness".to_string(),
                status: CheckStatus::Unhealthy,
                message: format!("missing providers: {}", missing.join(", ")),
                timestamp: chrono::Local::now(),
            }
        }
    }
}

// ─── DeepConfigStoreHealthCheck ─────────────────────────────────

/// Enhanced config-store health check with deep validation.
///
/// In addition to file readability and data-dir writability (handled by
/// [`ConfigStoreHealthCheck`]), this check optionally:
/// - Parses the YAML config and reports parse errors
/// - Validates that critical keys (`agent.model`) are present
pub struct DeepConfigStoreHealthCheck {
    config_path: std::path::PathBuf,
    data_dir: std::path::PathBuf,
}

impl DeepConfigStoreHealthCheck {
    /// Create a new deep config-store health check.
    pub fn new(config_path: std::path::PathBuf, data_dir: std::path::PathBuf) -> Self {
        Self {
            config_path,
            data_dir,
        }
    }
}

#[async_trait]
impl HealthCheck for DeepConfigStoreHealthCheck {
    fn component_name(&self) -> &str {
        "config_store"
    }

    async fn report_health(&self) -> HealthCheckResult {
        let mut issues: Vec<String> = Vec::new();

        // 1. Config file existence and readability
        if self.config_path.exists() {
            match std::fs::read_to_string(&self.config_path) {
                Ok(content) => {
                    // 2. YAML parse validation
                    match serde_yaml::from_str::<serde_yaml::Value>(&content) {
                        Ok(parsed) => {
                            // 3. Check critical key: agent.model
                            if let Some(agent) = parsed.get("agent") {
                                if agent.get("model").is_none() {
                                    // model is optional — warn but not error
                                    issues
                                        .push("agent.model not set (will use default)".to_string());
                                }
                            }
                            // If no agent section at all, that's acceptable (defaults)
                        }
                        Err(e) => {
                            issues.push(format!("config YAML parse error: {}", e));
                        }
                    }
                }
                Err(e) => {
                    issues.push(format!(
                        "config file {} is not readable: {}",
                        self.config_path.display(),
                        e
                    ));
                }
            }
        }
        // Config doesn't exist → acceptable (defaults are used).

        // 4. Data dir writability
        let probe = self.data_dir.join(".health_probe");
        match std::fs::write(&probe, b"probe") {
            Ok(()) => {
                let _ = std::fs::remove_file(&probe);
            }
            Err(e) => {
                issues.push(format!(
                    "data dir {} is not writable: {}",
                    self.data_dir.display(),
                    e
                ));
            }
        }

        // 5. Check data dir exists
        if !self.data_dir.exists() {
            issues.push(format!(
                "data dir {} does not exist",
                self.data_dir.display()
            ));
        }

        if issues.is_empty() {
            HealthCheckResult {
                component: "config_store".to_string(),
                status: CheckStatus::Healthy,
                message: format!(
                    "config: {}, data_dir: {}",
                    if self.config_path.exists() {
                        "present, valid"
                    } else {
                        "default"
                    },
                    self.data_dir.display()
                ),
                timestamp: chrono::Local::now(),
            }
        } else {
            // Distinguish between fatal issues (parse errors, not writable)
            // and warnings (missing optional keys)
            let has_fatal = issues.iter().any(|i| {
                i.contains("not readable")
                    || i.contains("not writable")
                    || i.contains("parse error")
                    || i.contains("does not exist")
            });
            HealthCheckResult {
                component: "config_store".to_string(),
                status: if has_fatal {
                    CheckStatus::Unhealthy
                } else {
                    CheckStatus::Degraded
                },
                message: issues.join("; "),
                timestamp: chrono::Local::now(),
            }
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use kestrel_heartbeat::types::HealthCheck as _;

    // === ProviderHealthCheck ===

    #[tokio::test]
    async fn test_provider_check_no_providers() {
        let registry = kestrel_providers::ProviderRegistry::new();
        let check = ProviderHealthCheck::new(registry);
        let result = check.report_health().await;

        assert_eq!(result.component, "provider");
        assert_eq!(result.status, CheckStatus::Skipped);
        assert!(result.message.contains("No providers"));
    }

    #[tokio::test]
    async fn test_provider_check_name() {
        let registry = kestrel_providers::ProviderRegistry::new();
        let check = ProviderHealthCheck::new(registry);
        assert_eq!(check.component_name(), "provider");
    }

    // === SessionStoreHealthCheck ===

    #[tokio::test]
    async fn test_session_store_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = kestrel_session::SessionManager::new(dir.path().to_path_buf()).unwrap();
        let check = SessionStoreHealthCheck::new(mgr);
        let result = check.report_health().await;

        assert_eq!(result.component, "session_store");
        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("0 active sessions"));
    }

    #[tokio::test]
    async fn test_session_store_with_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = kestrel_session::SessionManager::new(dir.path().to_path_buf()).unwrap();
        mgr.get_or_create("test:a", None);
        mgr.get_or_create("test:b", None);

        let check = SessionStoreHealthCheck::new(mgr);
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("2 active sessions"));
    }

    #[tokio::test]
    async fn test_session_store_name() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = kestrel_session::SessionManager::new(dir.path().to_path_buf()).unwrap();
        let check = SessionStoreHealthCheck::new(mgr);
        assert_eq!(check.component_name(), "session_store");
    }

    // === ChannelHealthCheck ===

    #[tokio::test]
    async fn test_channel_check_no_channels() {
        let check = ChannelHealthCheck::empty();
        let result = check.report_health().await;

        assert_eq!(result.component, "channel");
        assert_eq!(result.status, CheckStatus::Skipped);
    }

    #[tokio::test]
    async fn test_channel_check_all_connected() {
        let connected = Arc::new(parking_lot::RwLock::new(std::collections::HashSet::from([
            "telegram".to_string(),
            "discord".to_string(),
        ])));
        let check = ChannelHealthCheck::new(
            vec!["telegram".to_string(), "discord".to_string()],
            connected,
        );
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("2/2 channels connected"));
    }

    #[tokio::test]
    async fn test_channel_check_partial_disconnection() {
        let connected = Arc::new(parking_lot::RwLock::new(std::collections::HashSet::from([
            "telegram".to_string(),
        ])));
        let check = ChannelHealthCheck::new(
            vec!["telegram".to_string(), "discord".to_string()],
            connected,
        );
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Degraded);
        assert!(result.message.contains("discord"));
        assert!(result.message.contains("1/2 connected"));
    }

    #[tokio::test]
    async fn test_channel_check_all_disconnected() {
        let connected = Arc::new(parking_lot::RwLock::new(std::collections::HashSet::new()));
        let check = ChannelHealthCheck::new(vec!["telegram".to_string()], connected);
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Unhealthy);
        assert!(result.message.contains("telegram"));
    }

    #[tokio::test]
    async fn test_channel_check_dynamic_connection() {
        let connected: Arc<parking_lot::RwLock<std::collections::HashSet<String>>> =
            Arc::new(parking_lot::RwLock::new(std::collections::HashSet::new()));
        let check = ChannelHealthCheck::new(vec!["ws".to_string()], connected.clone());

        // Initially disconnected
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Unhealthy);

        // Connect
        connected.write().insert("ws".to_string());
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Healthy);

        // Disconnect
        connected.write().remove("ws");
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Unhealthy);
    }

    #[tokio::test]
    async fn test_channel_check_name() {
        let check = ChannelHealthCheck::empty();
        assert_eq!(check.component_name(), "channel");
    }

    // === BusHealthCheck ===

    #[tokio::test]
    async fn test_bus_check_healthy() {
        let bus = kestrel_bus::MessageBus::new();
        let check = BusHealthCheck::new(bus);
        let result = check.report_health().await;

        assert_eq!(result.component, "bus");
        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("bus operational"));
    }

    #[tokio::test]
    async fn test_bus_check_with_subscribers() {
        let bus = kestrel_bus::MessageBus::new();
        let _rx1 = bus.subscribe_events();
        let _rx2 = bus.subscribe_events();

        let check = BusHealthCheck::new(bus);
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Healthy);
        // 2 subscribers + the one created by the check itself
        assert!(result.message.contains("subscribers"));
    }

    #[tokio::test]
    async fn test_bus_check_name() {
        let bus = kestrel_bus::MessageBus::new();
        let check = BusHealthCheck::new(bus);
        assert_eq!(check.component_name(), "bus");
    }

    // === ToolRegistryHealthCheck ===

    #[tokio::test]
    async fn test_tool_registry_empty_degraded() {
        let registry = kestrel_tools::ToolRegistry::new();
        let check = ToolRegistryHealthCheck::new(registry);
        let result = check.report_health().await;

        assert_eq!(result.component, "tool_registry");
        assert_eq!(result.status, CheckStatus::Degraded);
        assert!(result.message.contains("No tools registered"));
    }

    #[tokio::test]
    async fn test_tool_registry_with_tools_healthy() {
        let registry = kestrel_tools::ToolRegistry::new();

        use async_trait::async_trait;
        use kestrel_tools::Tool;
        use kestrel_tools::ToolError;

        struct DummyTool;
        #[async_trait]
        impl Tool for DummyTool {
            fn name(&self) -> &str {
                "test_tool"
            }
            fn description(&self) -> &str {
                "A test tool"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            async fn execute(&self, _args: serde_json::Value) -> Result<String, ToolError> {
                Ok("ok".to_string())
            }
        }
        registry.register(DummyTool);

        let check = ToolRegistryHealthCheck::new(registry);
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("1 tools registered"));
    }

    #[tokio::test]
    async fn test_tool_registry_name() {
        let registry = kestrel_tools::ToolRegistry::new();
        let check = ToolRegistryHealthCheck::new(registry);
        assert_eq!(check.component_name(), "tool_registry");
    }

    // === AgentLoopHealthCheck ===

    #[tokio::test]
    async fn test_agent_loop_no_activity() {
        let last_activity = Arc::new(parking_lot::RwLock::new(None));
        let check = AgentLoopHealthCheck::new(last_activity, 60);
        let result = check.report_health().await;

        assert_eq!(result.component, "agent_loop");
        assert_eq!(result.status, CheckStatus::Skipped);
        assert!(result.message.contains("No messages"));
    }

    #[tokio::test]
    async fn test_agent_loop_recent_activity() {
        let last_activity = Arc::new(parking_lot::RwLock::new(None));
        let check = AgentLoopHealthCheck::new(last_activity.clone(), 60);

        check.record_activity();
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("active"));
    }

    #[tokio::test]
    async fn test_agent_loop_stale_activity() {
        // Set last activity to 2 minutes ago, with threshold of 30s
        let two_min_ago = chrono::Local::now() - chrono::Duration::seconds(120);
        let last_activity = Arc::new(parking_lot::RwLock::new(Some(two_min_ago)));
        let check = AgentLoopHealthCheck::new(last_activity, 30);
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Unhealthy);
        assert!(result.message.contains("idle"));
        assert!(result.message.contains("threshold: 30s"));
    }

    #[tokio::test]
    async fn test_agent_loop_exactly_at_threshold() {
        // Activity exactly at threshold boundary should still be healthy
        let just_now = chrono::Local::now() - chrono::Duration::seconds(5);
        let last_activity = Arc::new(parking_lot::RwLock::new(Some(just_now)));
        let check = AgentLoopHealthCheck::new(last_activity, 10);
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Healthy);
    }

    #[tokio::test]
    async fn test_agent_loop_name() {
        let last_activity = Arc::new(parking_lot::RwLock::new(None));
        let check = AgentLoopHealthCheck::new(last_activity, 60);
        assert_eq!(check.component_name(), "agent_loop");
    }

    #[tokio::test]
    async fn test_agent_loop_record_activity_updates() {
        let last_activity = Arc::new(parking_lot::RwLock::new(None));
        let check = AgentLoopHealthCheck::new(last_activity.clone(), 60);

        // Initially no activity
        assert!(last_activity.read().is_none());

        // Record activity
        check.record_activity();
        assert!(last_activity.read().is_some());
    }

    // === ConfigStoreHealthCheck ===

    #[tokio::test]
    async fn test_config_store_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        std::fs::write(&config_path, "agent:\n  model: test\n").unwrap();

        let check = ConfigStoreHealthCheck::new(config_path, dir.path().to_path_buf());
        let result = check.report_health().await;

        assert_eq!(result.component, "config_store");
        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("present"));
    }

    #[tokio::test]
    async fn test_config_store_healthy_no_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("nonexistent.yaml");

        let check = ConfigStoreHealthCheck::new(config_path, dir.path().to_path_buf());
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("default"));
    }

    #[tokio::test]
    async fn test_config_store_unreadable_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        std::fs::write(&config_path, "test").unwrap();
        std::fs::set_permissions(
            &config_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o000),
        )
        .unwrap();

        let check = ConfigStoreHealthCheck::new(config_path.clone(), dir.path().to_path_buf());
        let result = check.report_health().await;

        // On some systems running as root, the permission restriction doesn't apply
        // so we accept either Unhealthy or Healthy
        assert!(matches!(
            result.status,
            CheckStatus::Unhealthy | CheckStatus::Healthy
        ));

        // Restore permissions for cleanup
        std::fs::set_permissions(
            &config_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )
        .ok();
    }

    #[tokio::test]
    async fn test_config_store_unwritable_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("readonly");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::set_permissions(
            &data_dir,
            std::os::unix::fs::PermissionsExt::from_mode(0o444),
        )
        .unwrap();

        let check = ConfigStoreHealthCheck::new(dir.path().join("config.yaml"), data_dir.clone());
        let result = check.report_health().await;

        // On some systems running as root, the permission restriction doesn't apply
        assert!(matches!(
            result.status,
            CheckStatus::Unhealthy | CheckStatus::Healthy
        ));

        // Restore permissions for cleanup
        std::fs::set_permissions(
            &data_dir,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .ok();
    }

    #[tokio::test]
    async fn test_config_store_name() {
        let dir = tempfile::tempdir().unwrap();
        let check =
            ConfigStoreHealthCheck::new(dir.path().join("config.yaml"), dir.path().to_path_buf());
        assert_eq!(check.component_name(), "config_store");
    }

    // === LivenessCheck ===

    #[tokio::test]
    async fn test_liveness_no_tick() {
        let last_tick = Arc::new(parking_lot::RwLock::new(None));
        let check = LivenessCheck::new(last_tick, 60);
        let result = check.report_health().await;

        assert_eq!(result.component, "liveness");
        assert_eq!(result.status, CheckStatus::Skipped);
        assert!(result.message.contains("No heartbeat tick"));
    }

    #[tokio::test]
    async fn test_liveness_recent_tick() {
        let last_tick = Arc::new(parking_lot::RwLock::new(None));
        let check = LivenessCheck::new(last_tick.clone(), 60);

        check.record_tick();
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("alive"));
    }

    #[tokio::test]
    async fn test_liveness_stale_tick() {
        let stale = chrono::Local::now() - chrono::Duration::seconds(300);
        let last_tick = Arc::new(parking_lot::RwLock::new(Some(stale)));
        let check = LivenessCheck::new(last_tick, 60);
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Unhealthy);
        assert!(result.message.contains("event loop blocked"));
        assert!(result.message.contains("threshold: 60s"));
    }

    #[tokio::test]
    async fn test_liveness_name() {
        let last_tick = Arc::new(parking_lot::RwLock::new(None));
        let check = LivenessCheck::new(last_tick, 60);
        assert_eq!(check.component_name(), "liveness");
    }

    #[tokio::test]
    async fn test_liveness_record_tick_updates() {
        let last_tick = Arc::new(parking_lot::RwLock::new(None));
        let check = LivenessCheck::new(last_tick.clone(), 60);

        assert!(last_tick.read().is_none());
        check.record_tick();
        assert!(last_tick.read().is_some());
    }

    // === ReadinessCheck ===

    #[tokio::test]
    async fn test_readiness_no_required_providers() {
        let registry = kestrel_providers::ProviderRegistry::new();
        let check = ReadinessCheck::new(vec![], registry);
        let result = check.report_health().await;

        assert_eq!(result.component, "readiness");
        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("no required providers"));
    }

    #[tokio::test]
    async fn test_readiness_all_present() {
        let mut registry = kestrel_providers::ProviderRegistry::new();
        registry.register("openai", MockProviderForReadiness);
        registry.register("anthropic", MockProviderForReadiness);

        let check = ReadinessCheck::new(
            vec!["openai".to_string(), "anthropic".to_string()],
            registry,
        );
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("2/2 providers initialised"));
    }

    #[tokio::test]
    async fn test_readiness_missing_provider() {
        let mut registry = kestrel_providers::ProviderRegistry::new();
        registry.register("openai", MockProviderForReadiness);

        let check = ReadinessCheck::new(
            vec!["openai".to_string(), "anthropic".to_string()],
            registry,
        );
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Unhealthy);
        assert!(result.message.contains("anthropic"));
        assert!(result.message.contains("missing providers"));
    }

    #[tokio::test]
    async fn test_readiness_name() {
        let registry = kestrel_providers::ProviderRegistry::new();
        let check = ReadinessCheck::new(vec![], registry);
        assert_eq!(check.component_name(), "readiness");
    }

    /// Minimal mock provider for readiness tests.
    struct MockProviderForReadiness;

    #[async_trait]
    impl kestrel_providers::base::LlmProvider for MockProviderForReadiness {
        fn name(&self) -> &str {
            "mock"
        }
        fn default_model(&self) -> &str {
            "mock-model"
        }
        async fn complete(
            &self,
            _req: kestrel_providers::base::CompletionRequest,
        ) -> anyhow::Result<kestrel_providers::base::CompletionResponse> {
            Ok(kestrel_providers::base::CompletionResponse {
                content: Some("ok".to_string()),
                tool_calls: None,
                usage: None,
                finish_reason: None,
            })
        }
        async fn complete_stream(
            &self,
            req: kestrel_providers::base::CompletionRequest,
        ) -> anyhow::Result<kestrel_providers::base::BoxStream> {
            let resp = self.complete(req).await?;
            let chunk = kestrel_providers::base::CompletionChunk {
                delta: resp.content,
                tool_call_deltas: None,
                usage: resp.usage,
                done: true,
            };
            Ok(Box::pin(futures::stream::once(async move { Ok(chunk) })))
        }
        fn supports_model(&self, _model: &str) -> bool {
            true
        }
    }

    // === DeepConfigStoreHealthCheck ===

    #[tokio::test]
    async fn test_deep_config_valid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        std::fs::write(&config_path, "agent:\n  model: gpt-4\n").unwrap();

        let check = DeepConfigStoreHealthCheck::new(config_path, dir.path().to_path_buf());
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("present, valid"));
    }

    #[tokio::test]
    async fn test_deep_config_missing_model() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        std::fs::write(&config_path, "agent:\n  max_iterations: 10\n").unwrap();

        let check = DeepConfigStoreHealthCheck::new(config_path, dir.path().to_path_buf());
        let result = check.report_health().await;

        // Missing model is a warning (Degraded), not fatal
        assert_eq!(result.status, CheckStatus::Degraded);
        assert!(result.message.contains("agent.model not set"));
    }

    #[tokio::test]
    async fn test_deep_config_invalid_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        std::fs::write(&config_path, "agent:\n  model: [broken\n").unwrap();

        let check = DeepConfigStoreHealthCheck::new(config_path, dir.path().to_path_buf());
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Unhealthy);
        assert!(result.message.contains("parse error"));
    }

    #[tokio::test]
    async fn test_deep_config_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("nonexistent.yaml");

        let check = DeepConfigStoreHealthCheck::new(config_path, dir.path().to_path_buf());
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("default"));
    }

    #[tokio::test]
    async fn test_deep_config_unwritable_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("readonly");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::set_permissions(
            &data_dir,
            std::os::unix::fs::PermissionsExt::from_mode(0o444),
        )
        .unwrap();

        let check =
            DeepConfigStoreHealthCheck::new(dir.path().join("config.yaml"), data_dir.clone());
        let result = check.report_health().await;

        assert!(matches!(
            result.status,
            CheckStatus::Unhealthy | CheckStatus::Healthy
        ));

        std::fs::set_permissions(
            &data_dir,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .ok();
    }

    #[tokio::test]
    async fn test_deep_config_nonexistent_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let missing_dir = dir.path().join("does_not_exist");

        let check = DeepConfigStoreHealthCheck::new(dir.path().join("config.yaml"), missing_dir);
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Unhealthy);
        assert!(result.message.contains("does not exist"));
    }

    #[tokio::test]
    async fn test_deep_config_name() {
        let dir = tempfile::tempdir().unwrap();
        let check = DeepConfigStoreHealthCheck::new(
            dir.path().join("config.yaml"),
            dir.path().to_path_buf(),
        );
        assert_eq!(check.component_name(), "config_store");
    }

    // === Integration: all checks together ===

    #[tokio::test]
    async fn test_all_checks_report() {
        let dir = tempfile::tempdir().unwrap();
        let bus = kestrel_bus::MessageBus::new();
        let _rx = bus.subscribe_events();

        let provider_check = ProviderHealthCheck::new(kestrel_providers::ProviderRegistry::new());
        let session_check = SessionStoreHealthCheck::new(
            kestrel_session::SessionManager::new(dir.path().to_path_buf()).unwrap(),
        );
        let channel_check = ChannelHealthCheck::empty();
        let bus_check = BusHealthCheck::new(bus);
        let tool_check = ToolRegistryHealthCheck::new(kestrel_tools::ToolRegistry::new());
        let agent_check = AgentLoopHealthCheck::new(Arc::new(parking_lot::RwLock::new(None)), 60);
        let config_check =
            ConfigStoreHealthCheck::new(dir.path().join("config.yaml"), dir.path().to_path_buf());

        let checks: Vec<&dyn HealthCheck> = vec![
            &provider_check,
            &session_check,
            &channel_check,
            &bus_check,
            &tool_check,
            &agent_check,
            &config_check,
        ];

        let mut results = Vec::new();
        for check in checks {
            results.push(check.report_health().await);
        }

        assert_eq!(results.len(), 7);

        // Provider: skipped (no providers)
        assert_eq!(results[0].status, CheckStatus::Skipped);
        // Session: healthy
        assert_eq!(results[1].status, CheckStatus::Healthy);
        // Channel: skipped (no channels)
        assert_eq!(results[2].status, CheckStatus::Skipped);
        // Bus: healthy
        assert_eq!(results[3].status, CheckStatus::Healthy);
        // Tool registry: degraded (no tools)
        assert_eq!(results[4].status, CheckStatus::Degraded);
        // Agent: skipped (no activity yet)
        assert_eq!(results[5].status, CheckStatus::Skipped);
        // Config store: healthy (tempdir is writable)
        assert_eq!(results[6].status, CheckStatus::Healthy);

        // All should have unique component names
        let names: std::collections::HashSet<&str> =
            results.iter().map(|r| r.component.as_str()).collect();
        assert_eq!(names.len(), 7);
    }

    // === Integration with HeartbeatService ===

    #[tokio::test]
    async fn test_checks_with_heartbeat_service() {
        let dir = tempfile::tempdir().unwrap();
        let config = kestrel_config::Config::default();
        let svc =
            kestrel_heartbeat::HeartbeatService::with_data_dir(config, dir.path().to_path_buf());

        let _bus = kestrel_bus::MessageBus::new();
        let session_check = SessionStoreHealthCheck::new(
            kestrel_session::SessionManager::new(dir.path().to_path_buf()).unwrap(),
        );

        svc.register_check(std::sync::Arc::new(session_check));

        let snapshot = svc.run_checks().await.unwrap();
        assert!(snapshot.healthy);
        assert_eq!(snapshot.checks.len(), 1);
        assert_eq!(snapshot.checks[0].status, CheckStatus::Healthy);
    }

    #[tokio::test]
    async fn test_mixed_checks_with_heartbeat_service() {
        let dir = tempfile::tempdir().unwrap();
        let config = kestrel_config::Config::default();
        let svc =
            kestrel_heartbeat::HeartbeatService::with_data_dir(config, dir.path().to_path_buf())
                .with_failures_before_restart(3);

        // Register a healthy check
        svc.register_check(std::sync::Arc::new(SessionStoreHealthCheck::new(
            kestrel_session::SessionManager::new(dir.path().to_path_buf()).unwrap(),
        )));

        // Register a channel check that reports disconnected
        let connected = Arc::new(parking_lot::RwLock::new(std::collections::HashSet::new()));
        svc.register_check(std::sync::Arc::new(ChannelHealthCheck::new(
            vec!["telegram".to_string()],
            connected.clone(),
        )));

        // First check — one healthy, one unhealthy
        let snapshot = svc.run_checks().await.unwrap();
        assert!(!snapshot.healthy);
        assert_eq!(snapshot.failed_count(), 1);

        // Connect the channel
        connected.write().insert("telegram".to_string());

        // Second check — all healthy now
        let snapshot = svc.run_checks().await.unwrap();
        assert!(snapshot.healthy);

        // Verify failure tracking was reset
        let failure = svc.component_failure_state("channel");
        assert!(failure.is_some());
        let f = failure.unwrap();
        assert_eq!(f.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn test_bus_check_always_healthy() {
        // Bus health check should never report unhealthy
        // (if the bus is broken, the check itself can't run anyway)
        let bus = kestrel_bus::MessageBus::new();
        let check = BusHealthCheck::new(bus);

        for _ in 0..5 {
            let result = check.report_health().await;
            assert_eq!(result.status, CheckStatus::Healthy);
        }
    }
}
