//! Full pipeline integration tests — end-to-end message flow verification.
//!
//! Verifies the complete assembly: Bus → ChannelManager → AgentLoop → Provider
//! with deterministic mock components. No LLM calls are made.
//!
//! Key principle: tests are fully deterministic. Mock responses are fixed.
//! We verify message flow paths and state changes, not output quality.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use nanobot_agent::AgentLoop;
use nanobot_bus::events::{AgentEvent, InboundMessage};
use nanobot_bus::MessageBus;
use nanobot_channels::base::{BaseChannel, SendResult};
use nanobot_channels::registry::ChannelRegistry;
use nanobot_channels::ChannelManager;
use nanobot_config::Config;
use nanobot_core::{MessageType, Platform};
use nanobot_heartbeat::types::CheckStatus;
use nanobot_heartbeat::{HealthCheck, HealthCheckResult};
use nanobot_providers::{CompletionRequest, CompletionResponse, LlmProvider};
use nanobot_session::SessionManager;
use nanobot_tools::ToolRegistry;

// ===========================================================================
// Mock LLM Provider — returns fixed deterministic responses
// ===========================================================================

/// A mock provider that returns a canned response.
/// Optionally returns a tool_call on the first invocation and a text
/// response on the second.
struct MockProvider {
    /// Fixed text response to return.
    response: String,
    /// If set, the first call returns this tool call instead.
    first_tool_call: Option<(String, String, String)>, // (tool_name, args_json, id)
    /// How many times complete() was called.
    call_count: std::sync::atomic::AtomicUsize,
}

impl MockProvider {
    fn new(response: &str) -> Self {
        Self {
            response: response.to_string(),
            first_tool_call: None,
            call_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// On the first call, return a tool_call; on the second, return text.
    fn with_tool_call(mut self, tool_name: &str, args: &str) -> Self {
        self.first_tool_call = Some((
            tool_name.to_string(),
            args.to_string(),
            "call_mock_1".to_string(),
        ));
        self
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn supports_model(&self, _model: &str) -> bool {
        true
    }

    async fn complete(&self, _request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
        let count = self.call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        // First call: tool call (if configured)
        if count == 0 {
            if let Some(ref tc) = self.first_tool_call {
                return Ok(CompletionResponse {
                    content: Some(String::new()),
                    tool_calls: Some(vec![nanobot_core::ToolCall {
                        id: tc.2.clone(),
                        call_type: "function".to_string(),
                        function: nanobot_core::FunctionCall {
                            name: tc.0.clone(),
                            arguments: tc.1.clone(),
                        },
                    }]),
                    usage: Some(nanobot_core::Usage {
                        prompt_tokens: Some(10),
                        completion_tokens: Some(5),
                        total_tokens: Some(15),
                    }),
                    finish_reason: Some("tool_calls".to_string()),
                });
            }
        }

        // Regular text response
        Ok(CompletionResponse {
            content: Some(self.response.clone()),
            tool_calls: None,
            usage: Some(nanobot_core::Usage {
                prompt_tokens: Some(10),
                completion_tokens: Some(20),
                total_tokens: Some(30),
            }),
            finish_reason: Some("stop".to_string()),
        })
    }

    async fn complete_stream(
        &self,
        request: CompletionRequest,
    ) -> anyhow::Result<nanobot_providers::base::BoxStream> {
        // Delegate to complete() and wrap as a single-chunk stream.
        use nanobot_providers::base::{CompletionChunk, ToolCallDelta};
        let resp = self.complete(request).await?;
        let tool_call_deltas = resp.tool_calls.as_ref().map(|tcs| {
            tcs.iter()
                .enumerate()
                .map(|(i, tc)| ToolCallDelta {
                    index: i,
                    id: Some(tc.id.clone()),
                    function_name: Some(tc.function.name.clone()),
                    function_arguments: Some(tc.function.arguments.clone()),
                })
                .collect::<Vec<_>>()
        });
        let chunk = CompletionChunk {
            delta: resp.content.clone(),
            tool_call_deltas,
            usage: resp.usage.clone(),
            done: true,
        };
        Ok(Box::pin(futures::stream::once(async move { Ok(chunk) })))
    }
}

// ===========================================================================
// Mock Channel — records outbound calls
// ===========================================================================

type RecordedCall = (String, String, Option<String>);

struct MockChannel {
    platform: Platform,
    connected: bool,
    handler: Option<tokio::sync::mpsc::Sender<InboundMessage>>,
    sent_messages: Arc<std::sync::Mutex<Vec<RecordedCall>>>,
}

impl MockChannel {
    fn new(platform: Platform, sent: Arc<std::sync::Mutex<Vec<RecordedCall>>>) -> Self {
        Self {
            platform,
            connected: false,
            handler: None,
            sent_messages: sent,
        }
    }
}

#[async_trait]
impl BaseChannel for MockChannel {
    fn name(&self) -> &str {
        self.platform.as_str()
    }
    fn platform(&self) -> Platform {
        self.platform.clone()
    }
    fn is_connected(&self) -> bool {
        self.connected
    }
    async fn connect(&mut self) -> anyhow::Result<bool> {
        self.connected = true;
        Ok(true)
    }
    async fn disconnect(&mut self) -> anyhow::Result<()> {
        self.connected = false;
        Ok(())
    }
    async fn send_message(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
    ) -> anyhow::Result<SendResult> {
        self.sent_messages.lock().unwrap().push((
            chat_id.to_string(),
            content.to_string(),
            reply_to.map(|s| s.to_string()),
        ));
        Ok(SendResult {
            success: true,
            message_id: Some("mock-msg-1".to_string()),
            error: None,
            retryable: false,
        })
    }
    async fn send_typing(&self, _chat_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
    async fn send_image(
        &self,
        chat_id: &str,
        image_url: &str,
        caption: Option<&str>,
    ) -> anyhow::Result<SendResult> {
        self.sent_messages.lock().unwrap().push((
            chat_id.to_string(),
            format!("[image:{} caption:{}]", image_url, caption.unwrap_or("")),
            None,
        ));
        Ok(SendResult {
            success: true,
            message_id: Some("mock-img-1".to_string()),
            error: None,
            retryable: false,
        })
    }
    fn set_message_handler(&mut self, handler: tokio::sync::mpsc::Sender<InboundMessage>) {
        self.handler = Some(handler);
    }
}

// ===========================================================================
// Test helpers
// ===========================================================================

fn make_test_config() -> Config {
    Config::default()
}

fn make_inbound(content: &str, chat_id: &str) -> InboundMessage {
    InboundMessage {
        channel: Platform::Telegram,
        sender_id: "test_user".to_string(),
        chat_id: chat_id.to_string(),
        content: content.to_string(),
        media: vec![],
        metadata: HashMap::new(),
        source: None,
        message_type: MessageType::Text,
        message_id: Some("msg_1".to_string()),
        reply_to: None,
        timestamp: chrono::Local::now(),
    }
}

/// Collect ALL events within a timeout, stopping early when the stop predicate
/// is satisfied. Returns all events received (not filtered).
async fn collect_events_until(
    rx: &mut tokio::sync::broadcast::Receiver<AgentEvent>,
    stop: impl Fn(&AgentEvent) -> bool,
    timeout_ms: u64,
) -> Vec<AgentEvent> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut collected = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(event)) => {
                let should_stop = stop(&event);
                collected.push(event);
                if should_stop {
                    break;
                }
            }
            _ => break,
        }
    }
    collected
}

// ===========================================================================
// Tests
// ===========================================================================

// ===========================================================================
// Tests
// ===========================================================================

/// Test 1: Complete message flow — mock channel → bus → agent → bus → channel.
///
/// Sends an inbound message through the bus. The agent loop processes it
/// using a mock provider that returns a fixed response. The outbound message
/// is routed back to the mock channel. We verify the reply content.
#[tokio::test]
async fn test_full_message_pipeline() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let config = make_test_config();
    let bus = MessageBus::new();

    // Session manager
    let session_manager =
        SessionManager::new(tmp_dir.path().to_path_buf()).unwrap();

    // Provider registry with mock
    let mut provider_registry = nanobot_providers::ProviderRegistry::new();
    provider_registry.register("mock", MockProvider::new("The answer is 42."));
    provider_registry.set_default("mock");

    // Tool registry (empty)
    let tool_registry = ToolRegistry::new();

    // Agent loop
    let agent_loop = AgentLoop::new(
        config,
        bus.clone(),
        session_manager.clone(),
        provider_registry,
        tool_registry,
    );

    // Channel manager with mock telegram
    let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sent_clone = sent.clone();
    let platform = Platform::Telegram;
    let mut registry = ChannelRegistry::new();
    registry.register("telegram", move || {
        Box::new(MockChannel::new(platform.clone(), sent_clone.clone()))
    });
    let cm = Arc::new(ChannelManager::new(registry, bus.clone()));
    cm.start_channel("telegram").await.unwrap();

    // Subscribe to events
    let mut event_rx = bus.subscribe_events();

    // Spawn agent loop (consumes inbound)
    let agent_handle = tokio::spawn(async move {
        if let Err(e) = agent_loop.run().await {
            eprintln!("Agent loop error: {e}");
        }
    });

    // Spawn outbound consumer (routes outbound to channels)
    let outbound_cm = cm.clone();
    let outbound_handle = tokio::spawn(async move {
        outbound_cm.run_outbound_consumer().await;
    });

    // Give tasks a moment to start and wire up receivers
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Send an inbound message
    let inbound = make_inbound("What is the answer?", "chat_42");
    bus.publish_inbound(inbound).await.unwrap();

    // Collect events until Completed
    let all_events = collect_events_until(&mut event_rx, |e| {
        matches!(e, AgentEvent::Completed { .. })
    }, 3000).await;

    let has_completed = all_events.iter().any(|e| matches!(e, AgentEvent::Completed { .. }));
    assert!(has_completed, "Expected a Completed event");

    // Verify the outbound message reached the mock channel
    let recorded = sent.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1, "Expected 1 outbound message, got {:?}", recorded);
    assert_eq!(recorded[0].0, "chat_42");
    assert_eq!(recorded[0].1, "The answer is 42.");
    assert_eq!(recorded[0].2, Some("msg_1".to_string())); // reply_to

    // Verify events: Started → Completed
    let has_started = all_events.iter().any(|e| {
        matches!(e, AgentEvent::Started { session_key } if session_key.contains("chat_42"))
    });
    assert!(has_started, "Expected a Started event for chat_42");

    // Cleanup
    outbound_handle.abort();
    agent_handle.abort();
    cm.stop_all().await;
}

/// Test 2: Multi-turn conversation — verify session persistence across messages.
///
/// Sends two messages in sequence from the same chat_id.
/// Verifies that the session is persisted and reused (the session key
/// remains the same across turns).
#[tokio::test]
async fn test_multi_turn_conversation() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let config = make_test_config();
    let bus = MessageBus::new();

    let session_manager =
        SessionManager::new(tmp_dir.path().to_path_buf()).unwrap();

    let mut provider_registry = nanobot_providers::ProviderRegistry::new();
    provider_registry.register("mock", MockProvider::new("Response."));
    provider_registry.set_default("mock");

    let tool_registry = ToolRegistry::new();

    let agent_loop = AgentLoop::new(
        config,
        bus.clone(),
        session_manager.clone(),
        provider_registry,
        tool_registry,
    );

    let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sent_clone = sent.clone();
    let platform = Platform::Telegram;
    let mut registry = ChannelRegistry::new();
    registry.register("telegram", move || {
        Box::new(MockChannel::new(platform.clone(), sent_clone.clone()))
    });
    let cm = Arc::new(ChannelManager::new(registry, bus.clone()));
    cm.start_channel("telegram").await.unwrap();

    let mut event_rx = bus.subscribe_events();

    let al = agent_loop;
    let agent_handle = tokio::spawn(async move {
        if let Err(e) = al.run().await {
            eprintln!("Agent loop error: {e}");
        }
    });

    let outbound_cm = cm.clone();
    let outbound_handle = tokio::spawn(async move {
        outbound_cm.run_outbound_consumer().await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Turn 1
    let msg1 = make_inbound("Hello", "chat_multi");
    bus.publish_inbound(msg1).await.unwrap();
    let events1 = collect_events_until(&mut event_rx, |e| {
        matches!(e, AgentEvent::Completed { .. })
    }, 3000).await;
    assert!(events1.iter().any(|e| matches!(e, AgentEvent::Completed { .. })), "Turn 1 should complete");

    // Turn 2
    let msg2 = make_inbound("Follow up", "chat_multi");
    bus.publish_inbound(msg2).await.unwrap();
    let events2 = collect_events_until(&mut event_rx, |e| {
        matches!(e, AgentEvent::Completed { .. })
    }, 3000).await;
    assert!(events2.iter().any(|e| matches!(e, AgentEvent::Completed { .. })), "Turn 2 should complete");

    // Both replies should be recorded
    let recorded = sent.lock().unwrap().clone();
    assert_eq!(recorded.len(), 2, "Expected 2 outbound messages");
    assert_eq!(recorded[0].0, "chat_multi");
    assert_eq!(recorded[1].0, "chat_multi");

    // Session should exist and have entries
    let keys = session_manager.active_session_keys();
    assert_eq!(keys.len(), 1, "Should have exactly 1 session");
    assert!(keys[0].contains("chat_multi"), "Session key should contain chat_multi");

    outbound_handle.abort();
    agent_handle.abort();
    cm.stop_all().await;
}

/// Test 3: Tool call flow — mock provider returns a tool_call, registry
/// dispatches, result feeds back, final text response is sent.
///
/// The mock provider's first call returns a tool_call for a "shell" tool.
/// The tool registry has no real tools, so the tool error is fed back.
/// The second call returns a text response. We verify the final outbound
/// reaches the channel.
#[tokio::test]
async fn test_tool_call_flow() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let config = make_test_config();
    let bus = MessageBus::new();

    let session_manager =
        SessionManager::new(tmp_dir.path().to_path_buf()).unwrap();

    // Mock provider: first call returns tool_call, second returns text
    let mut provider_registry = nanobot_providers::ProviderRegistry::new();
    provider_registry.register(
        "mock",
        MockProvider::new("Tool executed successfully.")
            .with_tool_call("shell", "{\"command\":\"echo hello\"}"),
    );
    provider_registry.set_default("mock");

    // Tool registry with a mock shell tool
    let tool_registry = ToolRegistry::new();
    // The built-in shell tool won't be registered (register_all not called),
    // so the tool call will fail gracefully, and the agent will still produce
    // a final response on the second iteration.

    let agent_loop = AgentLoop::new(
        config,
        bus.clone(),
        session_manager,
        provider_registry,
        tool_registry,
    );

    let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sent_clone = sent.clone();
    let platform = Platform::Telegram;
    let mut registry = ChannelRegistry::new();
    registry.register("telegram", move || {
        Box::new(MockChannel::new(platform.clone(), sent_clone.clone()))
    });
    let cm = Arc::new(ChannelManager::new(registry, bus.clone()));
    cm.start_channel("telegram").await.unwrap();

    let mut event_rx = bus.subscribe_events();

    let al = agent_loop;
    let agent_handle = tokio::spawn(async move {
        if let Err(e) = al.run().await {
            eprintln!("Agent loop error: {e}");
        }
    });

    let outbound_cm = cm.clone();
    let outbound_handle = tokio::spawn(async move {
        outbound_cm.run_outbound_consumer().await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Send message that triggers tool call flow
    let msg = make_inbound("Run echo hello", "chat_tool");
    bus.publish_inbound(msg).await.unwrap();

    // Wait for completion, collecting all events
    let all_events = collect_events_until(&mut event_rx, |e| {
        matches!(e, AgentEvent::Completed { .. })
    }, 5000).await;
    assert!(all_events.iter().any(|e| matches!(e, AgentEvent::Completed { .. })),
        "Expected completion after tool call flow");

    // Verify final outbound message
    let recorded = sent.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1, "Expected 1 final outbound, got {:?}", recorded);
    assert_eq!(recorded[0].0, "chat_tool");
    // The mock provider returns "Tool executed successfully." on the 2nd call
    assert_eq!(recorded[0].1, "Tool executed successfully.");

    // Verify we got a ToolCall event
    let has_tool_call = all_events.iter().any(|e| {
        matches!(e, AgentEvent::ToolCall { tool_name, .. } if tool_name == "shell")
    });
    assert!(has_tool_call, "Expected a ToolCall event for 'shell'");

    outbound_handle.abort();
    agent_handle.abort();
    cm.stop_all().await;
}

/// Test 4: Cron fires an event on the bus.
///
/// Creates a CronService with a bus, adds a job, and calls tick()
/// to fire it. Verifies the CronFired event is emitted.
#[tokio::test]
async fn test_cron_fires_event() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let bus = MessageBus::new();
    let mut event_rx = bus.subscribe_events();

    let cron_service = nanobot_cron::CronService::with_bus(
        tmp_dir.path().to_path_buf(),
        bus,
    )
    .unwrap();

    // Add a one-shot "at" job that fires immediately (past timestamp)
    let schedule = nanobot_cron::CronSchedule {
        kind: nanobot_cron::ScheduleKind::At,
        at_ms: Some(1), // 1 ms after epoch — already in the past
        every_ms: None,
        expr: None,
        tz: None,
    };
    let payload = nanobot_cron::CronPayload {
        message: "Run daily backup".to_string(),
        channel: None,
        chat_id: None,
        deliver: false,
    };
    let job = cron_service.add_job(schedule, payload, Some("backup_job".to_string())).unwrap();

    // Tick to fire the job
    let fired = cron_service.tick();
    assert_eq!(fired.len(), 1, "Expected 1 job to fire");
    assert_eq!(fired[0].id, job.id);

    // Verify event on bus
    let events = collect_events_until(&mut event_rx, |e| {
        matches!(e, AgentEvent::CronFired { .. })
    }, 1000).await;

    let cron_event = events.iter().find(|e| matches!(e, AgentEvent::CronFired { .. }));
    assert!(cron_event.is_some(), "Expected CronFired event on bus");
    match cron_event.unwrap() {
        AgentEvent::CronFired { job_id, job_name, message } => {
            assert_eq!(*job_id, job.id);
            assert_eq!(job_name.as_deref(), Some("backup_job"));
            assert_eq!(message, "Run daily backup");
        }
        _ => panic!("Wrong event type"),
    }
}

/// Test 5: Heartbeat health check — registers a failing check, verifies
/// unhealthy snapshot and event emission.
#[tokio::test]
async fn test_heartbeat_health_check() {
    let config = make_test_config();
    let bus = MessageBus::new();
    let mut event_rx = bus.subscribe_events();

    let mut heartbeat = nanobot_heartbeat::HeartbeatService::new(config);
    heartbeat.set_bus(bus);

    // Register a failing health check
    struct FailingCheck;
    #[async_trait]
    impl HealthCheck for FailingCheck {
        fn component_name(&self) -> &str {
            "test_component"
        }
        async fn report_health(&self) -> HealthCheckResult {
            HealthCheckResult {
                component: "test_component".to_string(),
                status: CheckStatus::Unhealthy,
                message: "Simulated failure".to_string(),
                timestamp: chrono::Local::now(),
            }
        }
    }

    heartbeat.register_check(Arc::new(FailingCheck));

    // Run checks
    let snapshot = heartbeat.run_checks().await.unwrap();

    assert!(!snapshot.healthy, "Snapshot should be unhealthy");
    assert_eq!(snapshot.checks.len(), 1);
    assert_eq!(snapshot.checks[0].component, "test_component");
    assert!(matches!(snapshot.checks[0].status, CheckStatus::Unhealthy));

    // Verify event emitted
    let events = collect_events_until(&mut event_rx, |e| {
        matches!(e, AgentEvent::HeartbeatCheck { .. })
    }, 1000).await;

    let hb_event = events.iter().find(|e| matches!(e, AgentEvent::HeartbeatCheck { .. }));
    assert!(hb_event.is_some(), "Expected HeartbeatCheck event");
    match hb_event.unwrap() {
        AgentEvent::HeartbeatCheck { healthy, checks_total, checks_failed } => {
            assert!(!healthy);
            assert_eq!(*checks_total, 1);
            assert_eq!(*checks_failed, 1);
        }
        _ => panic!("Wrong event type"),
    }
}

/// Test 5b: Heartbeat with healthy and unhealthy checks.
#[tokio::test]
async fn test_heartbeat_mixed_checks() {
    let config = make_test_config();
    let bus = MessageBus::new();
    let mut event_rx = bus.subscribe_events();

    let mut heartbeat = nanobot_heartbeat::HeartbeatService::new(config);
    heartbeat.set_bus(bus);

    struct HealthyCheck;
    #[async_trait]
    impl HealthCheck for HealthyCheck {
        fn component_name(&self) -> &str { "healthy_svc" }
        async fn report_health(&self) -> HealthCheckResult {
            HealthCheckResult {
                component: "healthy_svc".to_string(),
                status: CheckStatus::Healthy,
                message: "OK".to_string(),
                timestamp: chrono::Local::now(),
            }
        }
    }

    struct UnhealthyCheck;
    #[async_trait]
    impl HealthCheck for UnhealthyCheck {
        fn component_name(&self) -> &str { "broken_svc" }
        async fn report_health(&self) -> HealthCheckResult {
            HealthCheckResult {
                component: "broken_svc".to_string(),
                status: CheckStatus::Unhealthy,
                message: "Connection refused".to_string(),
                timestamp: chrono::Local::now(),
            }
        }
    }

    heartbeat.register_check(Arc::new(HealthyCheck));
    heartbeat.register_check(Arc::new(UnhealthyCheck));

    let snapshot = heartbeat.run_checks().await.unwrap();

    assert!(!snapshot.healthy, "One unhealthy → overall unhealthy");
    assert_eq!(snapshot.checks.len(), 2);

    let events = collect_events_until(&mut event_rx, |e| {
        matches!(e, AgentEvent::HeartbeatCheck { .. })
    }, 1000).await;

    let hb_event = events.iter().find(|e| matches!(e, AgentEvent::HeartbeatCheck { .. }));
    assert!(hb_event.is_some());
    if let AgentEvent::HeartbeatCheck { checks_total, checks_failed, .. } = hb_event.unwrap() {
        assert_eq!(*checks_total, 2);
        assert_eq!(*checks_failed, 1);
    }
}

/// Test 6: Verify bus event ordering — Started before Completed.
#[tokio::test]
async fn test_event_ordering_started_before_completed() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let config = make_test_config();
    let bus = MessageBus::new();

    let session_manager =
        SessionManager::new(tmp_dir.path().to_path_buf()).unwrap();

    let mut provider_registry = nanobot_providers::ProviderRegistry::new();
    provider_registry.register("mock", MockProvider::new("Hi."));
    provider_registry.set_default("mock");

    let tool_registry = ToolRegistry::new();

    let agent_loop = AgentLoop::new(
        config,
        bus.clone(),
        session_manager,
        provider_registry,
        tool_registry,
    );

    let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sent_clone = sent.clone();
    let platform = Platform::Telegram;
    let mut registry = ChannelRegistry::new();
    registry.register("telegram", move || {
        Box::new(MockChannel::new(platform.clone(), sent_clone.clone()))
    });
    let cm = Arc::new(ChannelManager::new(registry, bus.clone()));
    cm.start_channel("telegram").await.unwrap();

    let mut event_rx = bus.subscribe_events();

    let al = agent_loop;
    let agent_handle = tokio::spawn(async move {
        if let Err(e) = al.run().await {
            eprintln!("Agent loop error: {e}");
        }
    });

    let outbound_cm = cm.clone();
    let outbound_handle = tokio::spawn(async move {
        outbound_cm.run_outbound_consumer().await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let msg = make_inbound("Hi", "chat_order");
    bus.publish_inbound(msg).await.unwrap();

    // Collect events until Completed
    let events = collect_events_until(&mut event_rx, |e| {
        matches!(e, AgentEvent::Completed { .. })
    }, 3000).await;

    // Find positions
    let started_pos = events.iter().position(|e| matches!(e, AgentEvent::Started { .. }));
    let completed_pos = events.iter().position(|e| matches!(e, AgentEvent::Completed { .. }));

    assert!(started_pos.is_some(), "Should have a Started event");
    assert!(completed_pos.is_some(), "Should have a Completed event");
    assert!(
        started_pos.unwrap() < completed_pos.unwrap(),
        "Started should come before Completed"
    );

    outbound_handle.abort();
    agent_handle.abort();
    cm.stop_all().await;
}

/// Test 7: Multiple platforms routing — Telegram and Discord simultaneously.
#[tokio::test]
async fn test_multi_platform_routing() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let config = make_test_config();
    let bus = MessageBus::new();

    let session_manager =
        SessionManager::new(tmp_dir.path().to_path_buf()).unwrap();

    let mut provider_registry = nanobot_providers::ProviderRegistry::new();
    provider_registry.register("mock", MockProvider::new("Multi-platform reply."));
    provider_registry.set_default("mock");

    let tool_registry = ToolRegistry::new();

    let agent_loop = AgentLoop::new(
        config,
        bus.clone(),
        session_manager,
        provider_registry,
        tool_registry,
    );

    // Two mock channels
    let tg_sent = Arc::new(std::sync::Mutex::new(Vec::new()));
    let dc_sent = Arc::new(std::sync::Mutex::new(Vec::new()));
    let tg_clone = tg_sent.clone();
    let dc_clone = dc_sent.clone();
    let tg_platform = Platform::Telegram;
    let dc_platform = Platform::Discord;

    let mut registry = ChannelRegistry::new();
    registry.register("telegram", move || {
        Box::new(MockChannel::new(tg_platform.clone(), tg_clone.clone()))
    });
    registry.register("discord", move || {
        Box::new(MockChannel::new(dc_platform.clone(), dc_clone.clone()))
    });

    let cm = Arc::new(ChannelManager::new(registry, bus.clone()));
    cm.start_channel("telegram").await.unwrap();
    cm.start_channel("discord").await.unwrap();

    let mut event_rx = bus.subscribe_events();

    let al = agent_loop;
    let agent_handle = tokio::spawn(async move {
        if let Err(e) = al.run().await {
            eprintln!("Agent loop error: {e}");
        }
    });

    let outbound_cm = cm.clone();
    let outbound_handle = tokio::spawn(async move {
        outbound_cm.run_outbound_consumer().await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Send to Telegram
    let tg_msg = make_inbound("Hello TG", "tg_chat");
    bus.publish_inbound(tg_msg).await.unwrap();

    // Send to Discord
    let dc_msg = InboundMessage {
        channel: Platform::Discord,
        sender_id: "dc_user".to_string(),
        chat_id: "dc_channel".to_string(),
        content: "Hello DC".to_string(),
        media: vec![],
        metadata: HashMap::new(),
        source: None,
        message_type: MessageType::Text,
        message_id: Some("dc_msg_1".to_string()),
        reply_to: None,
        timestamp: chrono::Local::now(),
    };
    bus.publish_inbound(dc_msg).await.unwrap();

    // Wait for both completions — collect until we see 2 Completed events
    let all_events = collect_events_until(&mut event_rx, |e| {
        // Stop after seeing the second Completed event — use a counter workaround
        matches!(e, AgentEvent::Completed { .. })
    }, 5000).await;
    let completed: Vec<_> = all_events.iter()
        .filter(|e| matches!(e, AgentEvent::Completed { .. }))
        .collect();
    // If we only got one, wait for the second
    let final_events = if completed.len() < 2 {
        let more = collect_events_until(&mut event_rx, |e| {
            matches!(e, AgentEvent::Completed { .. })
        }, 5000).await;
        let mut combined = all_events;
        combined.extend(more);
        combined
    } else {
        all_events
    };
    let completed_count = final_events.iter()
        .filter(|e| matches!(e, AgentEvent::Completed { .. }))
        .count();
    assert_eq!(completed_count, 2, "Expected 2 Completed events");

    // Verify routing: TG message → TG channel, DC message → DC channel
    let tg_recorded = tg_sent.lock().unwrap().clone();
    assert_eq!(tg_recorded.len(), 1);
    assert_eq!(tg_recorded[0].0, "tg_chat");

    let dc_recorded = dc_sent.lock().unwrap().clone();
    assert_eq!(dc_recorded.len(), 1);
    assert_eq!(dc_recorded[0].0, "dc_channel");

    outbound_handle.abort();
    agent_handle.abort();
    cm.stop_all().await;
}
