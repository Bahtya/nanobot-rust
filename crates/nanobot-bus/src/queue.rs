//! Async message bus using tokio channels.
//!
//! The MessageBus provides inbound/outbound queues that decouple
//! channel adapters from agent processing, matching the Python
//! `bus/queue.py` MessageBus pattern.

use crate::events::{AgentEvent, InboundMessage, OutboundMessage, StreamChunk};
use nanobot_core::BUS_CHANNEL_CAPACITY;
use tokio::sync::{broadcast, mpsc};

/// The central message bus for nanobot.
///
/// Connects channel adapters (producers of InboundMessages) with
/// the agent loop (consumer of InboundMessages, producer of OutboundMessages).
#[derive(Debug, Clone)]
pub struct MessageBus {
    /// Channel adapters publish inbound messages here.
    inbound_tx: mpsc::Sender<InboundMessage>,

    /// The agent loop consumes inbound messages from here.
    inbound_rx: std::sync::Arc<tokio::sync::RwLock<Option<mpsc::Receiver<InboundMessage>>>>,

    /// The agent loop publishes outbound messages here.
    outbound_tx: mpsc::Sender<OutboundMessage>,

    /// Channel adapters consume outbound messages from here.
    outbound_rx: std::sync::Arc<tokio::sync::RwLock<Option<mpsc::Receiver<OutboundMessage>>>>,

    /// Broadcast channel for agent lifecycle events.
    event_tx: broadcast::Sender<AgentEvent>,

    /// Streaming chunks for real-time output.
    stream_tx: broadcast::Sender<StreamChunk>,
}

impl MessageBus {
    /// Create a new MessageBus with default channel capacities.
    pub fn new() -> Self {
        Self::with_capacity(BUS_CHANNEL_CAPACITY)
    }

    /// Create a new MessageBus with a specific channel capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        let (inbound_tx, inbound_rx) = mpsc::channel(capacity);
        let (outbound_tx, outbound_rx) = mpsc::channel(capacity);
        let (event_tx, _) = broadcast::channel(256);
        let (stream_tx, _) = broadcast::channel(256);

        Self {
            inbound_tx,
            inbound_rx: std::sync::Arc::new(tokio::sync::RwLock::new(Some(inbound_rx))),
            outbound_tx,
            outbound_rx: std::sync::Arc::new(tokio::sync::RwLock::new(Some(outbound_rx))),
            event_tx,
            stream_tx,
        }
    }

    /// Publish an inbound message from a channel adapter.
    pub async fn publish_inbound(
        &self,
        msg: InboundMessage,
    ) -> Result<(), mpsc::error::SendError<InboundMessage>> {
        self.inbound_tx.send(msg).await
    }

    /// Take ownership of the inbound receiver (can only be called once).
    pub async fn consume_inbound(&self) -> Option<mpsc::Receiver<InboundMessage>> {
        self.inbound_rx.write().await.take()
    }

    /// Publish an outbound message from the agent.
    pub async fn publish_outbound(
        &self,
        msg: OutboundMessage,
    ) -> Result<(), mpsc::error::SendError<OutboundMessage>> {
        self.outbound_tx.send(msg).await
    }

    /// Take ownership of the outbound receiver (can only be called once).
    pub async fn consume_outbound(&self) -> Option<mpsc::Receiver<OutboundMessage>> {
        self.outbound_rx.write().await.take()
    }

    /// Emit an agent lifecycle event.
    pub fn emit_event(&self, event: AgentEvent) {
        // Ignore send errors (no listeners is fine).
        let _ = self.event_tx.send(event);
    }

    /// Subscribe to agent lifecycle events.
    pub fn subscribe_events(&self) -> broadcast::Receiver<AgentEvent> {
        self.event_tx.subscribe()
    }

    /// Publish a streaming chunk.
    pub fn publish_stream_chunk(&self, chunk: StreamChunk) {
        let _ = self.stream_tx.send(chunk);
    }

    /// Subscribe to streaming chunks.
    pub fn subscribe_stream(&self) -> broadcast::Receiver<StreamChunk> {
        self.stream_tx.subscribe()
    }

    /// Get a clone of the stream sender (for wiring streaming through components).
    pub fn subscribe_stream_tx(&self) -> broadcast::Sender<StreamChunk> {
        self.stream_tx.clone()
    }

    /// Get a reference to the inbound sender (for channel adapters).
    pub fn inbound_sender(&self) -> mpsc::Sender<InboundMessage> {
        self.inbound_tx.clone()
    }

    /// Get a reference to the outbound sender (for agent).
    pub fn outbound_sender(&self) -> mpsc::Sender<OutboundMessage> {
        self.outbound_tx.clone()
    }
}

impl Default for MessageBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanobot_core::{MessageType, Platform};

    #[tokio::test]
    async fn test_bus_roundtrip() {
        let bus = MessageBus::new();

        // Take receivers
        let mut inbound_rx = bus.consume_inbound().await.unwrap();
        let _outbound_rx = bus.consume_outbound().await.unwrap();

        // Publish inbound
        let msg = InboundMessage {
            channel: Platform::Local,
            sender_id: "user1".to_string(),
            chat_id: "chat1".to_string(),
            content: "Hello".to_string(),
            media: vec![],
            metadata: Default::default(),
            source: None,
            message_type: MessageType::Text,
            message_id: None,
            reply_to: None,
            timestamp: chrono::Local::now(),
        };
        bus.publish_inbound(msg.clone()).await.unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_secs(1), inbound_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.content, "Hello");
    }

    #[tokio::test]
    async fn test_consume_inbound_once() {
        let bus = MessageBus::new();
        let _rx = bus.consume_inbound().await.unwrap();
        let second = bus.consume_inbound().await;
        assert!(second.is_none());
    }

    #[tokio::test]
    async fn test_bus_outbound_roundtrip() {
        let bus = MessageBus::new();
        let mut outbound_rx = bus.consume_outbound().await.unwrap();

        let msg = OutboundMessage {
            channel: Platform::Local,
            chat_id: "chat1".to_string(),
            content: "Response".to_string(),
            reply_to: None,
            media: vec![],
            metadata: Default::default(),
        };
        bus.publish_outbound(msg).await.unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_secs(1), outbound_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.content, "Response");
    }

    #[tokio::test]
    async fn test_bus_consume_outbound_once() {
        let bus = MessageBus::new();
        let _rx = bus.consume_outbound().await.unwrap();
        let second = bus.consume_outbound().await;
        assert!(second.is_none());
    }

    #[tokio::test]
    async fn test_bus_events_broadcast() {
        let bus = MessageBus::new();
        let mut events_rx = bus.subscribe_events();

        bus.emit_event(AgentEvent::Started {
            session_key: "test".to_string(),
        });
        bus.emit_event(AgentEvent::Completed {
            session_key: "test".to_string(),
            iterations: 2,
            tool_calls: 1,
        });

        let event1 = tokio::time::timeout(std::time::Duration::from_secs(1), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event1, AgentEvent::Started { .. }));

        let event2 = tokio::time::timeout(std::time::Duration::from_secs(1), events_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event2, AgentEvent::Completed { .. }));
    }

    #[tokio::test]
    async fn test_bus_stream_chunks() {
        let bus = MessageBus::new();
        let mut stream_rx = bus.subscribe_stream();

        bus.publish_stream_chunk(StreamChunk {
            session_key: "test".to_string(),
            content: "Hello ".to_string(),
            done: false,
        });
        bus.publish_stream_chunk(StreamChunk {
            session_key: "test".to_string(),
            content: "World".to_string(),
            done: true,
        });

        let chunk1 = tokio::time::timeout(std::time::Duration::from_secs(1), stream_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(chunk1.content, "Hello ");
        assert!(!chunk1.done);

        let chunk2 = tokio::time::timeout(std::time::Duration::from_secs(1), stream_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(chunk2.content, "World");
        assert!(chunk2.done);
    }

    #[tokio::test]
    async fn test_bus_with_capacity() {
        let bus = MessageBus::with_capacity(1);
        let mut inbound_rx = bus.consume_inbound().await.unwrap();

        let msg = InboundMessage {
            channel: Platform::Local,
            sender_id: "u".to_string(),
            chat_id: "c".to_string(),
            content: "hi".to_string(),
            media: vec![],
            metadata: Default::default(),
            source: None,
            message_type: MessageType::Text,
            message_id: None,
            reply_to: None,
            timestamp: chrono::Local::now(),
        };

        // Capacity 1: first send should succeed
        bus.publish_inbound(msg.clone()).await.unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_secs(1), inbound_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.content, "hi");
    }

    #[tokio::test]
    async fn test_bus_inbound_sender_clone() {
        let bus = MessageBus::new();
        let mut rx = bus.consume_inbound().await.unwrap();

        let sender = bus.inbound_sender();
        let msg = InboundMessage {
            channel: Platform::Local,
            sender_id: "u".to_string(),
            chat_id: "c".to_string(),
            content: "from sender".to_string(),
            media: vec![],
            metadata: Default::default(),
            source: None,
            message_type: MessageType::Text,
            message_id: None,
            reply_to: None,
            timestamp: chrono::Local::now(),
        };
        sender.send(msg).await.unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.content, "from sender");
    }

    #[tokio::test]
    async fn test_bus_outbound_sender_clone() {
        let bus = MessageBus::new();
        let mut rx = bus.consume_outbound().await.unwrap();

        let sender = bus.outbound_sender();
        let msg = OutboundMessage {
            channel: Platform::Local,
            chat_id: "c".to_string(),
            content: "outbound".to_string(),
            reply_to: None,
            media: vec![],
            metadata: Default::default(),
        };
        sender.send(msg).await.unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.content, "outbound");
    }

    #[test]
    fn test_bus_default() {
        let bus = MessageBus::default();
        // Should be usable
        assert!(bus.inbound_sender().capacity() > 0);
    }

    #[tokio::test]
    async fn test_bus_stream_tx_clone() {
        let bus = MessageBus::new();
        let mut rx = bus.subscribe_stream();

        let tx = bus.subscribe_stream_tx();
        tx.send(StreamChunk {
            session_key: "k".to_string(),
            content: "chunk".to_string(),
            done: false,
        })
        .unwrap();

        let chunk = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(chunk.content, "chunk");
    }
}
