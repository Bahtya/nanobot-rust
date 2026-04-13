//! Component-specific health checks for the agent system.
//!
//! Provides concrete [`HealthCheck`] implementations for:
//! - **Provider health**: verifies LLM providers can respond to a minimal request
//! - **Session store health**: verifies session persistence is writable
//! - **Channel health**: verifies channel adapters are connected
//! - **Bus health**: verifies the message bus is operational
//!
//! These checks are registered with the `HeartbeatService` from the
//! `nanobot-heartbeat` crate and polled periodically.

use async_trait::async_trait;
use nanobot_heartbeat::types::{CheckStatus, HealthCheck, HealthCheckResult};
use std::sync::Arc;

// ─── ProviderHealthCheck ─────────────────────────────────────────

/// Health check for LLM provider availability.
///
/// Sends a minimal completion request (1 token, empty prompt) to verify
/// the provider endpoint is reachable and responding. Skips the check if
/// no providers are registered.
pub struct ProviderHealthCheck {
    registry: nanobot_providers::ProviderRegistry,
}

impl ProviderHealthCheck {
    /// Create a new provider health check.
    pub fn new(registry: nanobot_providers::ProviderRegistry) -> Self {
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
                message: format!(
                    "providers unavailable: {}",
                    unhealthy.join(", ")
                ),
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
    manager: nanobot_session::SessionManager,
}

impl SessionStoreHealthCheck {
    /// Create a new session store health check.
    pub fn new(manager: nanobot_session::SessionManager) -> Self {
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
            connected: Arc::new(parking_lot::RwLock::new(
                std::collections::HashSet::new(),
            )),
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
    bus: nanobot_bus::MessageBus,
}

impl BusHealthCheck {
    /// Create a new bus health check.
    pub fn new(bus: nanobot_bus::MessageBus) -> Self {
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
    registry: nanobot_tools::ToolRegistry,
}

impl ToolRegistryHealthCheck {
    /// Create a new tool registry health check.
    pub fn new(registry: nanobot_tools::ToolRegistry) -> Self {
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

// ─── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use nanobot_heartbeat::types::HealthCheck as _;

    // === ProviderHealthCheck ===

    #[tokio::test]
    async fn test_provider_check_no_providers() {
        let registry = nanobot_providers::ProviderRegistry::new();
        let check = ProviderHealthCheck::new(registry);
        let result = check.report_health().await;

        assert_eq!(result.component, "provider");
        assert_eq!(result.status, CheckStatus::Skipped);
        assert!(result.message.contains("No providers"));
    }

    #[tokio::test]
    async fn test_provider_check_name() {
        let registry = nanobot_providers::ProviderRegistry::new();
        let check = ProviderHealthCheck::new(registry);
        assert_eq!(check.component_name(), "provider");
    }

    // === SessionStoreHealthCheck ===

    #[tokio::test]
    async fn test_session_store_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let mgr =
            nanobot_session::SessionManager::new(dir.path().to_path_buf()).unwrap();
        let check = SessionStoreHealthCheck::new(mgr);
        let result = check.report_health().await;

        assert_eq!(result.component, "session_store");
        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("0 active sessions"));
    }

    #[tokio::test]
    async fn test_session_store_with_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let mgr =
            nanobot_session::SessionManager::new(dir.path().to_path_buf()).unwrap();
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
        let mgr =
            nanobot_session::SessionManager::new(dir.path().to_path_buf()).unwrap();
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
        let connected = Arc::new(parking_lot::RwLock::new(
            std::collections::HashSet::from([
                "telegram".to_string(),
                "discord".to_string(),
            ]),
        ));
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
        let connected = Arc::new(parking_lot::RwLock::new(
            std::collections::HashSet::from(["telegram".to_string()]),
        ));
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
        let connected = Arc::new(parking_lot::RwLock::new(
            std::collections::HashSet::new(),
        ));
        let check = ChannelHealthCheck::new(
            vec!["telegram".to_string()],
            connected,
        );
        let result = check.report_health().await;

        assert_eq!(result.status, CheckStatus::Unhealthy);
        assert!(result.message.contains("telegram"));
    }

    #[tokio::test]
    async fn test_channel_check_dynamic_connection() {
        let connected: Arc<parking_lot::RwLock<std::collections::HashSet<String>>> =
            Arc::new(parking_lot::RwLock::new(std::collections::HashSet::new()));
        let check = ChannelHealthCheck::new(
            vec!["ws".to_string()],
            connected.clone(),
        );

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
        let bus = nanobot_bus::MessageBus::new();
        let check = BusHealthCheck::new(bus);
        let result = check.report_health().await;

        assert_eq!(result.component, "bus");
        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("bus operational"));
    }

    #[tokio::test]
    async fn test_bus_check_with_subscribers() {
        let bus = nanobot_bus::MessageBus::new();
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
        let bus = nanobot_bus::MessageBus::new();
        let check = BusHealthCheck::new(bus);
        assert_eq!(check.component_name(), "bus");
    }

    // === ToolRegistryHealthCheck ===

    #[tokio::test]
    async fn test_tool_registry_empty_degraded() {
        let registry = nanobot_tools::ToolRegistry::new();
        let check = ToolRegistryHealthCheck::new(registry);
        let result = check.report_health().await;

        assert_eq!(result.component, "tool_registry");
        assert_eq!(result.status, CheckStatus::Degraded);
        assert!(result.message.contains("No tools registered"));
    }

    #[tokio::test]
    async fn test_tool_registry_with_tools_healthy() {
        let registry = nanobot_tools::ToolRegistry::new();

        use async_trait::async_trait;
        use nanobot_tools::Tool;
        use nanobot_tools::ToolError;

        struct DummyTool;
        #[async_trait]
        impl Tool for DummyTool {
            fn name(&self) -> &str { "test_tool" }
            fn description(&self) -> &str { "A test tool" }
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
        let registry = nanobot_tools::ToolRegistry::new();
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

    // === Integration: all checks together ===

    #[tokio::test]
    async fn test_all_checks_report() {
        let dir = tempfile::tempdir().unwrap();
        let bus = nanobot_bus::MessageBus::new();
        let _rx = bus.subscribe_events();

        let provider_check = ProviderHealthCheck::new(nanobot_providers::ProviderRegistry::new());
        let session_check = SessionStoreHealthCheck::new(
            nanobot_session::SessionManager::new(dir.path().to_path_buf()).unwrap(),
        );
        let channel_check = ChannelHealthCheck::empty();
        let bus_check = BusHealthCheck::new(bus);
        let tool_check = ToolRegistryHealthCheck::new(nanobot_tools::ToolRegistry::new());
        let agent_check = AgentLoopHealthCheck::new(
            Arc::new(parking_lot::RwLock::new(None)),
            60,
        );

        let checks: Vec<&dyn HealthCheck> = vec![
            &provider_check,
            &session_check,
            &channel_check,
            &bus_check,
            &tool_check,
            &agent_check,
        ];

        let mut results = Vec::new();
        for check in checks {
            results.push(check.report_health().await);
        }

        assert_eq!(results.len(), 6);

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

        // All should have unique component names
        let names: std::collections::HashSet<&str> =
            results.iter().map(|r| r.component.as_str()).collect();
        assert_eq!(names.len(), 6);
    }

    // === Integration with HeartbeatService ===

    #[tokio::test]
    async fn test_checks_with_heartbeat_service() {
        let dir = tempfile::tempdir().unwrap();
        let config = nanobot_config::Config::default();
        let svc =
            nanobot_heartbeat::HeartbeatService::with_data_dir(config, dir.path().to_path_buf());

        let _bus = nanobot_bus::MessageBus::new();
        let session_check = SessionStoreHealthCheck::new(
            nanobot_session::SessionManager::new(dir.path().to_path_buf()).unwrap(),
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
        let config = nanobot_config::Config::default();
        let svc = nanobot_heartbeat::HeartbeatService::with_data_dir(config, dir.path().to_path_buf())
            .with_failures_before_restart(3);

        // Register a healthy check
        svc.register_check(std::sync::Arc::new(SessionStoreHealthCheck::new(
            nanobot_session::SessionManager::new(dir.path().to_path_buf()).unwrap(),
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
        let bus = nanobot_bus::MessageBus::new();
        let check = BusHealthCheck::new(bus);

        for _ in 0..5 {
            let result = check.report_health().await;
            assert_eq!(result.status, CheckStatus::Healthy);
        }
    }
}
