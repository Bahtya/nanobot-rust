//! Integration tests verifying the full gateway message routing pipeline.
//!
//! Tests the end-to-end flow:
//! - **Outbound**: bus → ChannelManager → correct channel adapter
//! - **Inbound**: channel adapter → handler → bus
//! - **Lifecycle**: start/stop channels, stop_all

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kestrel_bus::events::{InboundMessage, OutboundMessage};
use kestrel_bus::MessageBus;
use kestrel_channels::base::{BaseChannel, SendResult};
use kestrel_channels::registry::ChannelRegistry;
use kestrel_channels::ChannelManager;
use kestrel_core::{MessageType, Platform};

// ---------------------------------------------------------------------------
// MockChannel — records outbound calls, optionally emits inbound messages
// ---------------------------------------------------------------------------

/// A recorded outbound call: (chat_id, content, reply_to).
type RecordedCall = (String, String, Option<String>);

/// A mock channel that records all `send_message` / `send_image` calls
/// in a shared vector so the test can assert them.
struct MockChannel {
    platform: Platform,
    connected: bool,
    handler: Option<tokio::sync::mpsc::Sender<InboundMessage>>,
    sent_messages: Arc<Mutex<Vec<RecordedCall>>>,
}

impl MockChannel {
    fn new(platform: Platform, sent_messages: Arc<Mutex<Vec<RecordedCall>>>) -> Self {
        Self {
            platform,
            connected: false,
            handler: None,
            sent_messages,
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

    async fn send_typing(&self, _chat_id: &str, _trace_id: Option<&str>) -> anyhow::Result<()> {
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

// ---------------------------------------------------------------------------
// Helper: build a ChannelManager with a registered mock channel
// ---------------------------------------------------------------------------

fn build_manager_with_mock(
    platform: Platform,
    bus: &MessageBus,
) -> (ChannelManager, Arc<Mutex<Vec<RecordedCall>>>) {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sent_clone = sent.clone();
    let platform_name = platform.as_str().to_string();
    let platform_for_closure = platform.clone();

    let mut registry = ChannelRegistry::new();
    registry.register(&platform_name, move || {
        Box::new(MockChannel::new(
            platform_for_closure.clone(),
            sent_clone.clone(),
        ))
    });

    let manager = ChannelManager::new(registry, bus.clone());
    (manager, sent)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_outbound_routing_single_message() {
    let bus = MessageBus::new();
    let (manager, sent) = build_manager_with_mock(Platform::Telegram, &bus);

    // Start the mock telegram channel
    manager.start_channel("telegram").await.unwrap();
    assert!(manager
        .running_channel_names()
        .contains(&"telegram".to_string()));

    // Spawn outbound consumer in background
    let cm = Arc::new(manager);
    let cm_clone = cm.clone();
    let consumer_handle = tokio::spawn(async move {
        cm_clone.run_outbound_consumer().await;
    });

    // Publish an outbound message targeting Telegram
    let msg = OutboundMessage {
        channel: Platform::Telegram,
        chat_id: "12345".to_string(),
        content: "Hello from gateway!".to_string(),
        reply_to: Some("99".to_string()),
        trace_id: None,
        media: vec![],
        metadata: HashMap::new(),
    };
    bus.publish_outbound(msg).await.unwrap();

    // Give the consumer a moment to process
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Verify the message was routed to the mock channel
    let recorded = sent.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, "12345");
    assert_eq!(recorded[0].1, "Hello from gateway!");
    assert_eq!(recorded[0].2, Some("99".to_string()));

    consumer_handle.abort();
    cm.stop_all().await;
}

#[tokio::test]
async fn test_outbound_routing_multiple_messages() {
    let bus = MessageBus::new();
    let (manager, sent) = build_manager_with_mock(Platform::Discord, &bus);

    manager.start_channel("discord").await.unwrap();

    let cm = Arc::new(manager);
    let cm_clone = cm.clone();
    let consumer_handle = tokio::spawn(async move {
        cm_clone.run_outbound_consumer().await;
    });

    // Publish multiple messages
    for i in 0..5 {
        let msg = OutboundMessage {
            channel: Platform::Discord,
            chat_id: format!("channel_{i}"),
            content: format!("Message {i}"),
            reply_to: None,
            trace_id: None,
            media: vec![],
            metadata: HashMap::new(),
        };
        bus.publish_outbound(msg).await.unwrap();
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let recorded = sent.lock().unwrap().clone();
    assert_eq!(recorded.len(), 5);
    for (i, entry) in recorded.iter().enumerate() {
        assert_eq!(entry.0, format!("channel_{i}"));
        assert_eq!(entry.1, format!("Message {i}"));
    }

    consumer_handle.abort();
    cm.stop_all().await;
}

#[tokio::test]
async fn test_outbound_routing_unknown_platform() {
    let bus = MessageBus::new();
    let (manager, sent) = build_manager_with_mock(Platform::Telegram, &bus);

    manager.start_channel("telegram").await.unwrap();

    let cm = Arc::new(manager);
    let cm_clone = cm.clone();
    let consumer_handle = tokio::spawn(async move {
        cm_clone.run_outbound_consumer().await;
    });

    // Publish a message targeting Discord, but only Telegram is running
    let msg = OutboundMessage {
        channel: Platform::Discord,
        chat_id: "99999".to_string(),
        content: "Should not reach anyone".to_string(),
        reply_to: None,
        trace_id: None,
        media: vec![],
        metadata: HashMap::new(),
    };
    bus.publish_outbound(msg).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Nothing should be recorded — no Discord channel is running
    let recorded = sent.lock().unwrap().clone();
    assert!(recorded.is_empty());

    consumer_handle.abort();
    cm.stop_all().await;
}

#[tokio::test]
async fn test_inbound_from_channel_to_bus() {
    let bus = MessageBus::new();
    let mut registry = ChannelRegistry::new();

    // Register a mock telegram channel
    let registry = {
        registry.register("telegram", || {
            Box::new(MockChannel::new(
                Platform::Telegram,
                Arc::new(Mutex::new(Vec::new())),
            ))
        });
        registry
    };

    let manager = ChannelManager::new(registry, bus.clone());
    manager.start_channel("telegram").await.unwrap();

    // Take the inbound receiver from the bus (simulates the agent loop)
    let mut inbound_rx = bus.consume_inbound().await.unwrap();

    // Now simulate an inbound message arriving through the bus
    // (In real life the channel adapter would call bus.publish_inbound via the handler)
    let handler = bus.inbound_sender();
    let inbound = InboundMessage {
        channel: Platform::Telegram,
        sender_id: "user_42".to_string(),
        chat_id: "chat_42".to_string(),
        content: "Hello agent!".to_string(),
        media: vec![],
        metadata: HashMap::new(),
        source: None,
        message_type: MessageType::Text,
        message_id: Some("msg_100".to_string()),
        trace_id: None,
        reply_to: None,
        timestamp: chrono::Local::now(),
    };
    handler.send(inbound).await.unwrap();

    // The agent loop side should receive it
    let received = tokio::time::timeout(std::time::Duration::from_secs(1), inbound_rx.recv())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(received.channel, Platform::Telegram);
    assert_eq!(received.sender_id, "user_42");
    assert_eq!(received.chat_id, "chat_42");
    assert_eq!(received.content, "Hello agent!");
    assert_eq!(received.message_id.as_deref(), Some("msg_100"));

    manager.stop_all().await;
}

#[tokio::test]
async fn test_start_stop_lifecycle() {
    let bus = MessageBus::new();
    let (manager, _sent) = build_manager_with_mock(Platform::Telegram, &bus);

    // Initially empty
    assert!(manager.running_channel_names().is_empty());

    // Start
    manager.start_channel("telegram").await.unwrap();
    assert_eq!(manager.running_channel_names(), vec!["telegram"]);

    // Stop
    manager.stop_channel("telegram").await.unwrap();
    assert!(manager.running_channel_names().is_empty());

    // Can start again
    manager.start_channel("telegram").await.unwrap();
    assert_eq!(manager.running_channel_names(), vec!["telegram"]);

    manager.stop_all().await;
    assert!(manager.running_channel_names().is_empty());
}

#[tokio::test]
async fn test_stop_all_multiple_channels() {
    let bus = MessageBus::new();

    let telegram_sent = Arc::new(Mutex::new(Vec::new()));
    let discord_sent = Arc::new(Mutex::new(Vec::new()));
    let telegram_sent_clone = telegram_sent.clone();
    let discord_sent_clone = discord_sent.clone();

    let mut registry = ChannelRegistry::new();
    registry.register("telegram", move || {
        Box::new(MockChannel::new(
            Platform::Telegram,
            telegram_sent_clone.clone(),
        ))
    });
    registry.register("discord", move || {
        Box::new(MockChannel::new(
            Platform::Discord,
            discord_sent_clone.clone(),
        ))
    });

    let manager = ChannelManager::new(registry, bus);

    // Start both channels
    manager.start_channel("telegram").await.unwrap();
    manager.start_channel("discord").await.unwrap();

    let names = manager.running_channel_names();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"telegram".to_string()));
    assert!(names.contains(&"discord".to_string()));

    // Stop all at once
    manager.stop_all().await;
    assert!(manager.running_channel_names().is_empty());
}

#[tokio::test]
async fn test_full_roundtrip_inbound_to_outbound() {
    // End-to-end: inbound message arrives → agent publishes outbound reply →
    // outbound is routed to the correct channel
    let bus = MessageBus::new();

    let telegram_sent = Arc::new(Mutex::new(Vec::new()));
    let telegram_sent_clone = telegram_sent.clone();

    let mut registry = ChannelRegistry::new();
    registry.register("telegram", move || {
        Box::new(MockChannel::new(
            Platform::Telegram,
            telegram_sent_clone.clone(),
        ))
    });

    let manager = ChannelManager::new(registry, bus.clone());
    manager.start_channel("telegram").await.unwrap();

    // Take both receivers (simulates agent loop wiring)
    let mut inbound_rx = bus.consume_inbound().await.unwrap();

    // Spawn outbound consumer
    let cm = Arc::new(manager);
    let cm_clone = cm.clone();
    let consumer_handle = tokio::spawn(async move {
        cm_clone.run_outbound_consumer().await;
    });

    // 1. Simulate inbound message from Telegram
    let handler = bus.inbound_sender();
    let inbound = InboundMessage {
        channel: Platform::Telegram,
        sender_id: "user_42".to_string(),
        chat_id: "chat_42".to_string(),
        content: "What is 2+2?".to_string(),
        media: vec![],
        metadata: HashMap::new(),
        source: None,
        message_type: MessageType::Text,
        message_id: Some("msg_in_1".to_string()),
        trace_id: None,
        reply_to: None,
        timestamp: chrono::Local::now(),
    };
    handler.send(inbound).await.unwrap();

    // 2. "Agent" receives the inbound message
    let received = tokio::time::timeout(std::time::Duration::from_secs(1), inbound_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received.content, "What is 2+2?");
    assert_eq!(received.chat_id, "chat_42");

    // 3. "Agent" publishes an outbound reply
    let reply = OutboundMessage {
        channel: Platform::Telegram,
        chat_id: received.chat_id.clone(),
        content: "2+2 = 4".to_string(),
        reply_to: received.message_id.clone(),
        trace_id: None,
        media: vec![],
        metadata: HashMap::new(),
    };
    bus.publish_outbound(reply).await.unwrap();

    // 4. Verify the reply reached the mock channel
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let recorded = telegram_sent.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, "chat_42");
    assert_eq!(recorded[0].1, "2+2 = 4");
    assert_eq!(recorded[0].2, Some("msg_in_1".to_string()));

    consumer_handle.abort();
    cm.stop_all().await;
}

// ---------------------------------------------------------------------------
// Mock agent — async task that echoes inbound → outbound
// ---------------------------------------------------------------------------

/// Spawns a mock agent task that consumes inbound messages and publishes
/// echo replies as outbound messages. Returns a shutdown handle.
fn spawn_mock_agent(
    bus: MessageBus,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::watch::Sender<bool>,
) {
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let handle = tokio::spawn(async move {
        let mut inbound_rx = match bus.consume_inbound().await {
            Some(rx) => rx,
            None => return,
        };

        loop {
            tokio::select! {
                msg = inbound_rx.recv() => {
                    match msg {
                        Some(inbound) => {
                            // Simulate agent processing: echo the message back
                            let reply = OutboundMessage {
                                channel: inbound.channel.clone(),
                                chat_id: inbound.chat_id.clone(),
                                content: format!("Echo: {}", inbound.content),
                                reply_to: inbound.message_id.clone(),
                                trace_id: None,
                                media: vec![],
                                metadata: HashMap::new(),
                            };
                            if let Err(e) = bus.publish_outbound(reply).await {
                                eprintln!("Mock agent publish error: {e}");
                            }
                        }
                        None => break, // Channel closed
                    }
                }
                _ = shutdown_rx.changed() => {
                    break; // Shutdown signal
                }
            }
        }
    });

    (handle, shutdown_tx)
}

// ---------------------------------------------------------------------------
// Full gateway integration tests with mock agent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_gateway_mock_agent_single_message() {
    // Full flow: mock channel → bus → mock agent → bus → mock channel
    let bus = MessageBus::new();

    let telegram_sent = Arc::new(Mutex::new(Vec::new()));
    let telegram_sent_clone = telegram_sent.clone();

    let mut registry = ChannelRegistry::new();
    registry.register("telegram", move || {
        Box::new(MockChannel::new(
            Platform::Telegram,
            telegram_sent_clone.clone(),
        ))
    });

    let manager = ChannelManager::new(registry, bus.clone());
    manager.start_channel("telegram").await.unwrap();

    // Start mock agent (consumes inbound, publishes outbound echoes)
    let (agent_handle, agent_shutdown) = spawn_mock_agent(bus.clone());

    // Start outbound consumer (routes outbound to channel adapters)
    let cm = Arc::new(manager);
    let cm_clone = cm.clone();
    let consumer_handle = tokio::spawn(async move {
        cm_clone.run_outbound_consumer().await;
    });

    // Send an inbound message from Telegram
    let handler = bus.inbound_sender();
    let inbound = InboundMessage {
        channel: Platform::Telegram,
        sender_id: "user_42".to_string(),
        chat_id: "chat_100".to_string(),
        content: "Hello gateway!".to_string(),
        media: vec![],
        metadata: HashMap::new(),
        source: None,
        message_type: MessageType::Text,
        message_id: Some("msg_in_1".to_string()),
        trace_id: None,
        reply_to: None,
        timestamp: chrono::Local::now(),
    };
    handler.send(inbound).await.unwrap();

    // Wait for the message to flow through the pipeline
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Verify the reply reached the mock channel
    let recorded = telegram_sent.lock().unwrap().clone();
    assert_eq!(
        recorded.len(),
        1,
        "Expected exactly 1 message, got {:?}",
        recorded
    );
    assert_eq!(recorded[0].0, "chat_100");
    assert_eq!(recorded[0].1, "Echo: Hello gateway!");
    assert_eq!(recorded[0].2, Some("msg_in_1".to_string()));

    // Cleanup
    let _ = agent_shutdown.send(true);
    consumer_handle.abort();
    agent_handle.await.unwrap();
    cm.stop_all().await;
}

#[tokio::test]
async fn test_gateway_mock_agent_multi_platform() {
    // Test routing across two platforms simultaneously
    let bus = MessageBus::new();

    let telegram_sent = Arc::new(Mutex::new(Vec::new()));
    let discord_sent = Arc::new(Mutex::new(Vec::new()));
    let tg_clone = telegram_sent.clone();
    let dc_clone = discord_sent.clone();

    let mut registry = ChannelRegistry::new();
    registry.register("telegram", move || {
        Box::new(MockChannel::new(Platform::Telegram, tg_clone.clone()))
    });
    registry.register("discord", move || {
        Box::new(MockChannel::new(Platform::Discord, dc_clone.clone()))
    });

    let manager = ChannelManager::new(registry, bus.clone());
    manager.start_channel("telegram").await.unwrap();
    manager.start_channel("discord").await.unwrap();

    // Start mock agent
    let (agent_handle, agent_shutdown) = spawn_mock_agent(bus.clone());

    // Start outbound consumer
    let cm = Arc::new(manager);
    let cm_clone = cm.clone();
    let consumer_handle = tokio::spawn(async move {
        cm_clone.run_outbound_consumer().await;
    });

    // Send messages to both platforms
    let handler = bus.inbound_sender();

    let tg_msg = InboundMessage {
        channel: Platform::Telegram,
        sender_id: "tg_user".to_string(),
        chat_id: "tg_chat_1".to_string(),
        content: "From Telegram".to_string(),
        media: vec![],
        metadata: HashMap::new(),
        source: None,
        message_type: MessageType::Text,
        message_id: Some("tg_msg_1".to_string()),
        trace_id: None,
        reply_to: None,
        timestamp: chrono::Local::now(),
    };

    let dc_msg = InboundMessage {
        channel: Platform::Discord,
        sender_id: "dc_user".to_string(),
        chat_id: "dc_channel_1".to_string(),
        content: "From Discord".to_string(),
        media: vec![],
        metadata: HashMap::new(),
        source: None,
        message_type: MessageType::Text,
        message_id: Some("dc_msg_1".to_string()),
        trace_id: None,
        reply_to: None,
        timestamp: chrono::Local::now(),
    };

    handler.send(tg_msg).await.unwrap();
    handler.send(dc_msg).await.unwrap();

    // Wait for processing
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // Verify Telegram received its reply
    let tg_recorded = telegram_sent.lock().unwrap().clone();
    assert_eq!(
        tg_recorded.len(),
        1,
        "Telegram should have 1 message, got {:?}",
        tg_recorded
    );
    assert_eq!(tg_recorded[0].0, "tg_chat_1");
    assert_eq!(tg_recorded[0].1, "Echo: From Telegram");

    // Verify Discord received its reply
    let dc_recorded = discord_sent.lock().unwrap().clone();
    assert_eq!(
        dc_recorded.len(),
        1,
        "Discord should have 1 message, got {:?}",
        dc_recorded
    );
    assert_eq!(dc_recorded[0].0, "dc_channel_1");
    assert_eq!(dc_recorded[0].1, "Echo: From Discord");

    // Cleanup
    let _ = agent_shutdown.send(true);
    consumer_handle.abort();
    agent_handle.await.unwrap();
    cm.stop_all().await;
}

#[tokio::test]
async fn test_gateway_mock_agent_burst_messages() {
    // Test multiple messages from the same chat session in quick succession
    let bus = MessageBus::new();

    let telegram_sent = Arc::new(Mutex::new(Vec::new()));
    let telegram_sent_clone = telegram_sent.clone();

    let mut registry = ChannelRegistry::new();
    registry.register("telegram", move || {
        Box::new(MockChannel::new(
            Platform::Telegram,
            telegram_sent_clone.clone(),
        ))
    });

    let manager = ChannelManager::new(registry, bus.clone());
    manager.start_channel("telegram").await.unwrap();

    // Start mock agent
    let (agent_handle, agent_shutdown) = spawn_mock_agent(bus.clone());

    // Start outbound consumer
    let cm = Arc::new(manager);
    let cm_clone = cm.clone();
    let consumer_handle = tokio::spawn(async move {
        cm_clone.run_outbound_consumer().await;
    });

    // Send a burst of 10 messages
    let handler = bus.inbound_sender();
    for i in 0..10 {
        let inbound = InboundMessage {
            channel: Platform::Telegram,
            sender_id: "user_burst".to_string(),
            chat_id: "chat_burst".to_string(),
            content: format!("Message {i}"),
            media: vec![],
            metadata: HashMap::new(),
            source: None,
            message_type: MessageType::Text,
            message_id: Some(format!("msg_burst_{i}")),
            trace_id: None,
            reply_to: None,
            timestamp: chrono::Local::now(),
        };
        handler.send(inbound).await.unwrap();
    }

    // Wait for all messages to be processed
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // All 10 echoes should have been routed back
    let recorded = telegram_sent.lock().unwrap().clone();
    assert_eq!(
        recorded.len(),
        10,
        "Expected 10 messages, got {}",
        recorded.len()
    );

    // Verify ordering and content
    for (i, call) in recorded.iter().enumerate() {
        assert_eq!(call.0, "chat_burst", "Message {i} has wrong chat_id");
        assert_eq!(
            call.1,
            format!("Echo: Message {i}"),
            "Message {i} has wrong content"
        );
        assert_eq!(
            call.2,
            Some(format!("msg_burst_{i}")),
            "Message {i} has wrong reply_to"
        );
    }

    // Cleanup
    let _ = agent_shutdown.send(true);
    consumer_handle.abort();
    agent_handle.await.unwrap();
    cm.stop_all().await;
}

#[tokio::test]
async fn test_gateway_mock_agent_cross_platform_isolation() {
    // Ensure messages targeted at one platform don't leak to another
    let bus = MessageBus::new();

    let telegram_sent = Arc::new(Mutex::new(Vec::new()));
    let discord_sent = Arc::new(Mutex::new(Vec::new()));
    let tg_clone = telegram_sent.clone();
    let dc_clone = discord_sent.clone();

    let mut registry = ChannelRegistry::new();
    registry.register("telegram", move || {
        Box::new(MockChannel::new(Platform::Telegram, tg_clone.clone()))
    });
    registry.register("discord", move || {
        Box::new(MockChannel::new(Platform::Discord, dc_clone.clone()))
    });

    let manager = ChannelManager::new(registry, bus.clone());
    manager.start_channel("telegram").await.unwrap();
    manager.start_channel("discord").await.unwrap();

    // Start mock agent
    let (agent_handle, agent_shutdown) = spawn_mock_agent(bus.clone());

    // Start outbound consumer
    let cm = Arc::new(manager);
    let cm_clone = cm.clone();
    let consumer_handle = tokio::spawn(async move {
        cm_clone.run_outbound_consumer().await;
    });

    // Only send Telegram messages — Discord should receive nothing
    let handler = bus.inbound_sender();
    for i in 0..3 {
        let inbound = InboundMessage {
            channel: Platform::Telegram,
            sender_id: "tg_only_user".to_string(),
            chat_id: "tg_only_chat".to_string(),
            content: format!("TG only {i}"),
            media: vec![],
            metadata: HashMap::new(),
            source: None,
            message_type: MessageType::Text,
            message_id: Some(format!("tg_only_{i}")),
            trace_id: None,
            reply_to: None,
            timestamp: chrono::Local::now(),
        };
        handler.send(inbound).await.unwrap();
    }

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // Telegram should have 3 messages
    let tg_recorded = telegram_sent.lock().unwrap().clone();
    assert_eq!(tg_recorded.len(), 3);

    // Discord should have zero
    let dc_recorded = discord_sent.lock().unwrap().clone();
    assert!(
        dc_recorded.is_empty(),
        "Discord should have 0 messages, got {:?}",
        dc_recorded
    );

    // Cleanup
    let _ = agent_shutdown.send(true);
    consumer_handle.abort();
    agent_handle.await.unwrap();
    cm.stop_all().await;
}

#[tokio::test]
async fn test_gateway_graceful_shutdown() {
    // Test that stop_all gracefully shuts down while messages are flowing
    let bus = MessageBus::new();

    let telegram_sent = Arc::new(Mutex::new(Vec::new()));
    let telegram_sent_clone = telegram_sent.clone();

    let mut registry = ChannelRegistry::new();
    registry.register("telegram", move || {
        Box::new(MockChannel::new(
            Platform::Telegram,
            telegram_sent_clone.clone(),
        ))
    });

    let manager = ChannelManager::new(registry, bus.clone());
    manager.start_channel("telegram").await.unwrap();

    // Start mock agent
    let (agent_handle, agent_shutdown) = spawn_mock_agent(bus.clone());

    // Start outbound consumer
    let cm = Arc::new(manager);
    let cm_clone = cm.clone();
    let consumer_handle = tokio::spawn(async move {
        cm_clone.run_outbound_consumer().await;
    });

    // Send a message and wait for it
    let handler = bus.inbound_sender();
    let inbound = InboundMessage {
        channel: Platform::Telegram,
        sender_id: "user_shutdown".to_string(),
        chat_id: "chat_shutdown".to_string(),
        content: "Before shutdown".to_string(),
        media: vec![],
        metadata: HashMap::new(),
        source: None,
        message_type: MessageType::Text,
        message_id: Some("msg_shutdown_1".to_string()),
        trace_id: None,
        reply_to: None,
        timestamp: chrono::Local::now(),
    };
    handler.send(inbound).await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Verify message was delivered
    let recorded = telegram_sent.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1);

    // Graceful shutdown
    let _ = agent_shutdown.send(true);
    consumer_handle.abort();
    agent_handle.await.unwrap();
    cm.stop_all().await;

    // After shutdown, no channels should be running
    assert!(cm.running_channel_names().is_empty());
}
