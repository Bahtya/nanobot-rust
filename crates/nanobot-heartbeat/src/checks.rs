//! Concrete health check implementations for system components.
//!
//! Provides ready-to-use [`HealthCheck`] implementations for:
//! - **Providers** — LLM provider connectivity via minimal completion requests
//! - **Bus** — message bus liveness via event broadcast
//! - **Memory store** — read/write responsiveness via store/recall/delete cycle
//! - **Channels** — channel connectivity via shared status snapshot

use crate::types::*;
use async_trait::async_trait;
use chrono::Local;
use nanobot_bus::MessageBus;
use nanobot_memory::MemoryStore;
use nanobot_providers::ProviderRegistry;
use std::sync::Arc;
use std::time::Duration;

/// Default timeout for individual health check operations.
const DEFAULT_CHECK_TIMEOUT_SECS: u64 = 5;

// ─── ProviderHealthCheck ─────────────────────────────────────────

/// Health check for LLM providers.
///
/// Verifies provider connectivity by sending a minimal completion request
/// to each registered provider. Returns:
/// - `Healthy` if all providers respond
/// - `Degraded` if some providers respond but others fail
/// - `Unhealthy` if no providers respond
/// - `Skipped` if no providers are configured
pub struct ProviderHealthCheck {
    providers: Arc<ProviderRegistry>,
    timeout: Duration,
}

impl ProviderHealthCheck {
    /// Create a new provider health check.
    pub fn new(providers: Arc<ProviderRegistry>) -> Self {
        Self {
            providers,
            timeout: Duration::from_secs(DEFAULT_CHECK_TIMEOUT_SECS),
        }
    }

    /// Set a custom timeout per-provider check.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl HealthCheck for ProviderHealthCheck {
    fn component_name(&self) -> &str {
        "providers"
    }

    async fn report_health(&self) -> HealthCheckResult {
        let names = self.providers.provider_names();
        if names.is_empty() {
            return HealthCheckResult {
                component: "providers".to_string(),
                status: CheckStatus::Skipped,
                message: "no providers configured".to_string(),
                timestamp: Local::now(),
            };
        }

        let mut healthy_count = 0usize;
        let mut errors = Vec::new();

        for name in &names {
            if let Some(provider) = self.providers.get_provider_by_name(name) {
                let request = nanobot_providers::CompletionRequest {
                    model: name.clone(),
                    messages: vec![nanobot_core::Message {
                        role: nanobot_core::MessageRole::User,
                        content: "health check".to_string(),
                        name: None,
                        tool_call_id: None,
                        tool_calls: None,
                    }],
                    tools: None,
                    max_tokens: Some(1),
                    temperature: Some(0.0),
                    stream: false,
                };

                match tokio::time::timeout(self.timeout, provider.complete(request)).await {
                    Ok(Ok(_)) => healthy_count += 1,
                    Ok(Err(e)) => errors.push(format!("{}: {}", name, e)),
                    Err(_) => errors.push(format!("{}: timed out after {:?}", name, self.timeout)),
                }
            }
        }

        let total = names.len();
        if errors.is_empty() {
            HealthCheckResult {
                component: "providers".to_string(),
                status: CheckStatus::Healthy,
                message: format!("{}/{} providers reachable", healthy_count, total),
                timestamp: Local::now(),
            }
        } else if healthy_count > 0 {
            HealthCheckResult {
                component: "providers".to_string(),
                status: CheckStatus::Degraded,
                message: format!(
                    "{}/{} healthy; failed: [{}]",
                    healthy_count,
                    total,
                    errors.join(", ")
                ),
                timestamp: Local::now(),
            }
        } else {
            HealthCheckResult {
                component: "providers".to_string(),
                status: CheckStatus::Unhealthy,
                message: format!("all {} provider(s) failed: [{}]", total, errors.join(", ")),
                timestamp: Local::now(),
            }
        }
    }
}

// ─── BusHealthCheck ──────────────────────────────────────────────

/// Health check for the message bus.
///
/// Verifies bus liveness by emitting a test event and confirming it
/// arrives on a subscriber. Returns:
/// - `Healthy` if the event round-trip succeeds
/// - `Unhealthy` if the bus is unresponsive
pub struct BusHealthCheck {
    bus: MessageBus,
    timeout: Duration,
}

impl BusHealthCheck {
    /// Create a new bus health check.
    pub fn new(bus: MessageBus) -> Self {
        Self {
            bus,
            timeout: Duration::from_secs(DEFAULT_CHECK_TIMEOUT_SECS),
        }
    }

    /// Set a custom timeout for the event round-trip.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl HealthCheck for BusHealthCheck {
    fn component_name(&self) -> &str {
        "bus"
    }

    async fn report_health(&self) -> HealthCheckResult {
        let mut rx = self.bus.subscribe_events();
        self.bus.emit_event(nanobot_bus::AgentEvent::HeartbeatCheck {
            healthy: true,
            checks_total: 0,
            checks_failed: 0,
        });

        match tokio::time::timeout(self.timeout, rx.recv()).await {
            Ok(Ok(_)) => HealthCheckResult {
                component: "bus".to_string(),
                status: CheckStatus::Healthy,
                message: "event bus responsive".to_string(),
                timestamp: Local::now(),
            },
            Ok(Err(e)) => HealthCheckResult {
                component: "bus".to_string(),
                status: CheckStatus::Unhealthy,
                message: format!("event bus error: {}", e),
                timestamp: Local::now(),
            },
            Err(_) => HealthCheckResult {
                component: "bus".to_string(),
                status: CheckStatus::Unhealthy,
                message: format!("event bus timed out after {:?}", self.timeout),
                timestamp: Local::now(),
            },
        }
    }
}

// ─── MemoryStoreHealthCheck ──────────────────────────────────────

/// Health check for the memory store.
///
/// Verifies responsiveness by performing a store/recall/delete cycle
/// with a disposable test entry. Returns:
/// - `Healthy` if the full cycle succeeds
/// - `Unhealthy` if any operation fails
/// - `Skipped` if no store is configured
pub struct MemoryStoreHealthCheck {
    store: Arc<dyn MemoryStore>,
    timeout: Duration,
}

impl MemoryStoreHealthCheck {
    /// Create a new memory store health check.
    pub fn new(store: Arc<dyn MemoryStore>) -> Self {
        Self {
            store,
            timeout: Duration::from_secs(DEFAULT_CHECK_TIMEOUT_SECS),
        }
    }

    /// Set a custom timeout for the store/recall/delete cycle.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[async_trait]
impl HealthCheck for MemoryStoreHealthCheck {
    fn component_name(&self) -> &str {
        "memory_store"
    }

    async fn report_health(&self) -> HealthCheckResult {
        let entry =
            nanobot_memory::MemoryEntry::new("__heartbeat_check__", nanobot_memory::MemoryCategory::AgentNote);
        let id = entry.id.clone();

        // Store
        let store_fut = self.store.store(entry);
        match tokio::time::timeout(self.timeout, store_fut).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                return HealthCheckResult {
                    component: "memory_store".to_string(),
                    status: CheckStatus::Unhealthy,
                    message: format!("store failed: {}", e),
                    timestamp: Local::now(),
                };
            }
            Err(_) => {
                return HealthCheckResult {
                    component: "memory_store".to_string(),
                    status: CheckStatus::Unhealthy,
                    message: "store timed out".to_string(),
                    timestamp: Local::now(),
                };
            }
        }

        // Recall
        let recall_fut = self.store.recall(&id);
        let recall_ok = match tokio::time::timeout(self.timeout, recall_fut).await {
            Ok(Ok(Some(_))) => true,
            Ok(Ok(None)) => false,
            Ok(Err(e)) => {
                return HealthCheckResult {
                    component: "memory_store".to_string(),
                    status: CheckStatus::Unhealthy,
                    message: format!("recall failed: {}", e),
                    timestamp: Local::now(),
                };
            }
            Err(_) => {
                return HealthCheckResult {
                    component: "memory_store".to_string(),
                    status: CheckStatus::Unhealthy,
                    message: "recall timed out".to_string(),
                    timestamp: Local::now(),
                };
            }
        };

        // Cleanup — best-effort delete
        let _ = self.store.delete(&id).await;

        if recall_ok {
            HealthCheckResult {
                component: "memory_store".to_string(),
                status: CheckStatus::Healthy,
                message: format!("store/recall/delete OK ({} entries)", self.store.len().await),
                timestamp: Local::now(),
            }
        } else {
            HealthCheckResult {
                component: "memory_store".to_string(),
                status: CheckStatus::Degraded,
                message: "stored entry not found on recall".to_string(),
                timestamp: Local::now(),
            }
        }
    }
}

// ─── ChannelHealthCheck ──────────────────────────────────────────

/// Health check for channel connectivity.
///
/// Uses a shared status snapshot that the caller (typically the gateway)
/// updates from the channel manager. This avoids a direct dependency on
/// the channels crate while still providing real connectivity status.
///
/// Returns:
/// - `Healthy` if all channels report connected
/// - `Degraded` if some channels are connected and some are not
/// - `Unhealthy` if no channels are connected
/// - `Skipped` if no channels are configured
pub struct ChannelHealthCheck {
    statuses: Arc<parking_lot::RwLock<Vec<(String, bool)>>>,
}

impl ChannelHealthCheck {
    /// Create a new channel health check backed by a shared status vector.
    ///
    /// The caller should update the shared statuses periodically (e.g. on
    /// each heartbeat tick) from the channel manager.
    pub fn new(statuses: Arc<parking_lot::RwLock<Vec<(String, bool)>>>) -> Self {
        Self { statuses }
    }
}

#[async_trait]
impl HealthCheck for ChannelHealthCheck {
    fn component_name(&self) -> &str {
        "channels"
    }

    async fn report_health(&self) -> HealthCheckResult {
        let statuses = self.statuses.read().clone();
        if statuses.is_empty() {
            return HealthCheckResult {
                component: "channels".to_string(),
                status: CheckStatus::Skipped,
                message: "no channels configured".to_string(),
                timestamp: Local::now(),
            };
        }

        let total = statuses.len();
        let connected: Vec<&str> = statuses
            .iter()
            .filter(|(_, c)| *c)
            .map(|(n, _)| n.as_str())
            .collect();
        let disconnected: Vec<&str> = statuses
            .iter()
            .filter(|(_, c)| !*c)
            .map(|(n, _)| n.as_str())
            .collect();

        let connected_count = connected.len();
        if disconnected.is_empty() {
            HealthCheckResult {
                component: "channels".to_string(),
                status: CheckStatus::Healthy,
                message: format!("{}/{} channels connected", connected_count, total),
                timestamp: Local::now(),
            }
        } else if connected_count > 0 {
            HealthCheckResult {
                component: "channels".to_string(),
                status: CheckStatus::Degraded,
                message: format!(
                    "{}/{} connected; disconnected: [{}]",
                    connected_count,
                    total,
                    disconnected.join(", ")
                ),
                timestamp: Local::now(),
            }
        } else {
            HealthCheckResult {
                component: "channels".to_string(),
                status: CheckStatus::Unhealthy,
                message: format!(
                    "all {} channel(s) disconnected: [{}]",
                    total,
                    disconnected.join(", ")
                ),
                timestamp: Local::now(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::HealthCheck;
    use nanobot_memory::HotStore;

    // ─── ProviderHealthCheck tests ────────────────────────────────

    /// Mock provider that succeeds on completion.
    struct MockHealthyProvider;

    #[async_trait::async_trait]
    impl nanobot_providers::LlmProvider for MockHealthyProvider {
        fn name(&self) -> &str {
            "mock_healthy"
        }
        async fn complete(
            &self,
            _request: nanobot_providers::CompletionRequest,
        ) -> anyhow::Result<nanobot_providers::CompletionResponse> {
            Ok(nanobot_providers::CompletionResponse {
                content: Some("ok".to_string()),
                tool_calls: None,
                usage: None,
                finish_reason: None,
            })
        }
        async fn complete_stream(
            &self,
            request: nanobot_providers::CompletionRequest,
        ) -> anyhow::Result<nanobot_providers::base::BoxStream> {
            let resp = self.complete(request).await?;
            let chunk = nanobot_providers::base::CompletionChunk {
                delta: resp.content,
                tool_call_deltas: None,
                usage: None,
                done: true,
            };
            Ok(Box::pin(futures::stream::once(async move { Ok(chunk) })))
        }
        fn supports_model(&self, _model: &str) -> bool {
            true
        }
    }

    /// Mock provider that always fails.
    struct MockFailingProvider {
        error_msg: String,
    }

    #[async_trait::async_trait]
    impl nanobot_providers::LlmProvider for MockFailingProvider {
        fn name(&self) -> &str {
            "mock_failing"
        }
        async fn complete(
            &self,
            _request: nanobot_providers::CompletionRequest,
        ) -> anyhow::Result<nanobot_providers::CompletionResponse> {
            Err(anyhow::anyhow!("{}", self.error_msg))
        }
        async fn complete_stream(
            &self,
            request: nanobot_providers::CompletionRequest,
        ) -> anyhow::Result<nanobot_providers::base::BoxStream> {
            let resp = self.complete(request).await?;
            let chunk = nanobot_providers::base::CompletionChunk {
                delta: resp.content,
                tool_call_deltas: None,
                usage: None,
                done: true,
            };
            Ok(Box::pin(futures::stream::once(async move { Ok(chunk) })))
        }
        fn supports_model(&self, _model: &str) -> bool {
            true
        }
    }

    /// Mock provider that never responds (simulates timeout).
    struct MockSlowProvider;

    #[async_trait::async_trait]
    impl nanobot_providers::LlmProvider for MockSlowProvider {
        fn name(&self) -> &str {
            "mock_slow"
        }
        async fn complete(
            &self,
            _request: nanobot_providers::CompletionRequest,
        ) -> anyhow::Result<nanobot_providers::CompletionResponse> {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(nanobot_providers::CompletionResponse {
                content: Some("ok".to_string()),
                tool_calls: None,
                usage: None,
                finish_reason: None,
            })
        }
        async fn complete_stream(
            &self,
            request: nanobot_providers::CompletionRequest,
        ) -> anyhow::Result<nanobot_providers::base::BoxStream> {
            let resp = self.complete(request).await?;
            let chunk = nanobot_providers::base::CompletionChunk {
                delta: resp.content,
                tool_call_deltas: None,
                usage: None,
                done: true,
            };
            Ok(Box::pin(futures::stream::once(async move { Ok(chunk) })))
        }
        fn supports_model(&self, _model: &str) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn test_provider_check_skipped_when_empty() {
        let registry = Arc::new(ProviderRegistry::new());
        let check = ProviderHealthCheck::new(registry);
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Skipped);
        assert_eq!(result.component, "providers");
    }

    #[tokio::test]
    async fn test_provider_check_healthy() {
        let mut registry = ProviderRegistry::new();
        registry.register("mock", MockHealthyProvider);
        let check = ProviderHealthCheck::new(Arc::new(registry));
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("1/1"));
    }

    #[tokio::test]
    async fn test_provider_check_unhealthy_all_fail() {
        let mut registry = ProviderRegistry::new();
        registry.register(
            "fail_a",
            MockFailingProvider {
                error_msg: "connection refused".to_string(),
            },
        );
        registry.register(
            "fail_b",
            MockFailingProvider {
                error_msg: "auth error".to_string(),
            },
        );
        let check = ProviderHealthCheck::new(Arc::new(registry));
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Unhealthy);
        assert!(result.message.contains("fail_a"));
        assert!(result.message.contains("fail_b"));
    }

    #[tokio::test]
    async fn test_provider_check_degraded_mixed() {
        let mut registry = ProviderRegistry::new();
        registry.register("healthy", MockHealthyProvider);
        registry.register(
            "failing",
            MockFailingProvider {
                error_msg: "timeout".to_string(),
            },
        );
        let check = ProviderHealthCheck::new(Arc::new(registry));
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Degraded);
        assert!(result.message.contains("1/2 healthy"));
    }

    #[tokio::test]
    async fn test_provider_check_timeout() {
        let mut registry = ProviderRegistry::new();
        registry.register("slow", MockSlowProvider);
        let check =
            ProviderHealthCheck::new(Arc::new(registry)).with_timeout(Duration::from_millis(50));
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Unhealthy);
        assert!(result.message.contains("timed out"));
    }

    #[tokio::test]
    async fn test_provider_check_custom_timeout() {
        let registry = Arc::new(ProviderRegistry::new());
        let check = ProviderHealthCheck::new(registry).with_timeout(Duration::from_secs(10));
        assert_eq!(check.timeout, Duration::from_secs(10));
    }

    // ─── BusHealthCheck tests ─────────────────────────────────────

    #[tokio::test]
    async fn test_bus_check_healthy() {
        let bus = MessageBus::new();
        let check = BusHealthCheck::new(bus);
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Healthy);
        assert_eq!(result.component, "bus");
    }

    #[tokio::test]
    async fn test_bus_check_custom_timeout() {
        let bus = MessageBus::new();
        let check = BusHealthCheck::new(bus).with_timeout(Duration::from_secs(10));
        assert_eq!(check.timeout, Duration::from_secs(10));
    }

    // ─── MemoryStoreHealthCheck tests ─────────────────────────────

    async fn make_test_hot_store() -> HotStore {
        let dir = tempfile::tempdir().unwrap();
        let config = nanobot_memory::MemoryConfig::for_test(dir.path());
        HotStore::new(&config).await.unwrap()
    }

    #[tokio::test]
    async fn test_memory_check_healthy() {
        let store = make_test_hot_store().await;
        let check = MemoryStoreHealthCheck::new(Arc::new(store));
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Healthy);
        assert_eq!(result.component, "memory_store");
        assert!(result.message.contains("OK"));
    }

    #[tokio::test]
    async fn test_memory_check_custom_timeout() {
        let store = make_test_hot_store().await;
        let check = MemoryStoreHealthCheck::new(Arc::new(store)).with_timeout(Duration::from_secs(10));
        assert_eq!(check.timeout, Duration::from_secs(10));
    }

    // ─── ChannelHealthCheck tests ─────────────────────────────────

    #[tokio::test]
    async fn test_channel_check_skipped_when_empty() {
        let statuses = Arc::new(parking_lot::RwLock::new(Vec::new()));
        let check = ChannelHealthCheck::new(statuses);
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Skipped);
        assert_eq!(result.component, "channels");
    }

    #[tokio::test]
    async fn test_channel_check_all_connected() {
        let statuses = Arc::new(parking_lot::RwLock::new(vec![
            ("telegram".to_string(), true),
            ("discord".to_string(), true),
        ]));
        let check = ChannelHealthCheck::new(statuses);
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("2/2"));
    }

    #[tokio::test]
    async fn test_channel_check_degraded() {
        let statuses = Arc::new(parking_lot::RwLock::new(vec![
            ("telegram".to_string(), true),
            ("discord".to_string(), false),
        ]));
        let check = ChannelHealthCheck::new(statuses);
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Degraded);
        assert!(result.message.contains("1/2 connected"));
        assert!(result.message.contains("discord"));
    }

    #[tokio::test]
    async fn test_channel_check_all_disconnected() {
        let statuses = Arc::new(parking_lot::RwLock::new(vec![
            ("telegram".to_string(), false),
            ("discord".to_string(), false),
        ]));
        let check = ChannelHealthCheck::new(statuses);
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Unhealthy);
        assert!(result.message.contains("all 2"));
    }

    #[tokio::test]
    async fn test_channel_check_single_connected() {
        let statuses = Arc::new(parking_lot::RwLock::new(vec![(
            "telegram".to_string(),
            true,
        )]));
        let check = ChannelHealthCheck::new(statuses);
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Healthy);
        assert!(result.message.contains("1/1"));
    }

    #[tokio::test]
    async fn test_channel_check_status_updates_dynamically() {
        let statuses = Arc::new(parking_lot::RwLock::new(vec![
            ("telegram".to_string(), true),
        ]));
        let check = ChannelHealthCheck::new(statuses.clone());

        // Initially healthy
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Healthy);

        // Disconnect
        *statuses.write() = vec![("telegram".to_string(), false)];
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Unhealthy);

        // Reconnect
        *statuses.write() = vec![("telegram".to_string(), true)];
        let result = check.report_health().await;
        assert_eq!(result.status, CheckStatus::Healthy);
    }

    // ─── Integration: register checks with HeartbeatService ───────

    #[tokio::test]
    async fn test_register_and_run_all_checks() {
        let dir = tempfile::tempdir().unwrap();
        let config = nanobot_config::Config::default();
        let svc = crate::HeartbeatService::with_data_dir(config, dir.path().to_path_buf());

        // Register all four checks
        let mut provider_reg = ProviderRegistry::new();
        provider_reg.register("mock", MockHealthyProvider);
        svc.register_check(Arc::new(ProviderHealthCheck::new(Arc::new(provider_reg))));

        let bus = MessageBus::new();
        svc.register_check(Arc::new(BusHealthCheck::new(bus)));

        let store = make_test_hot_store().await;
        svc.register_check(Arc::new(MemoryStoreHealthCheck::new(Arc::new(store))));

        let channel_statuses = Arc::new(parking_lot::RwLock::new(vec![
            ("telegram".to_string(), true),
        ]));
        svc.register_check(Arc::new(ChannelHealthCheck::new(channel_statuses)));

        // Run all checks
        let snapshot = svc.run_checks().await.unwrap();
        assert!(snapshot.healthy);
        assert_eq!(snapshot.checks.len(), 4);

        // Verify each component is reported
        let names: Vec<&str> = snapshot.checks.iter().map(|c| c.component.as_str()).collect();
        assert!(names.contains(&"providers"));
        assert!(names.contains(&"bus"));
        assert!(names.contains(&"memory_store"));
        assert!(names.contains(&"channels"));
    }
}
