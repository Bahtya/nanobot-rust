//! Integration tests verifying the full gateway message routing pipeline.
//!
//! Tests the end-to-end flow:
//! - **Outbound**: bus → ChannelManager → correct channel adapter
//! - **Inbound**: channel adapter → handler → bus
//! - **Lifecycle**: start/stop channels, stop_all

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nanobot_bus::events::{InboundMessage, OutboundMessage};
use nanobot_bus::MessageBus;
use nanobot_channels::base::{BaseChannel, SendResult};
use nanobot_channels::registry::ChannelRegistry;
use nanobot_channels::ChannelManager;
use nanobot_core::{MessageType, Platform};

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
        Box::new(MockChannel::new(platform_for_closure.clone(), sent_clone.clone()))
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
    assert!(manager.running_channel_names().contains(&"telegram".to_string()));

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
            media: vec![],
            metadata: HashMap::new(),
        };
        bus.publish_outbound(msg).await.unwrap();
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let recorded = sent.lock().unwrap().clone();
    assert_eq!(recorded.len(), 5);
    for i in 0..5 {
        assert_eq!(recorded[i].0, format!("channel_{i}"));
        assert_eq!(recorded[i].1, format!("Message {i}"));
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
        Box::new(MockChannel::new(Platform::Telegram, telegram_sent_clone.clone()))
    });
    registry.register("discord", move || {
        Box::new(MockChannel::new(Platform::Discord, discord_sent_clone.clone()))
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
        Box::new(MockChannel::new(Platform::Telegram, telegram_sent_clone.clone()))
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
