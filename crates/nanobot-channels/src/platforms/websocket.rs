//! WebSocket channel adapter — server for browser-based chat clients.
//!
//! Runs a WebSocket server that browsers connect to. Each connected client
//! becomes an independent chat session. Messages use OpenAI-compatible
//! `{role, content}` JSON format.
//!
//! ## Message format
//!
//! **Inbound (browser → server):**
//! ```json
//! {"role": "user", "content": "Hello"}
//! ```
//!
//! **Outbound (server → browser):**
//! ```json
//! {"role": "assistant", "content": "Response text"}
//! ```
//!
//! **Typing indicator (server → browser):**
//! ```json
//! {"type": "typing"}
//! ```
//!
//! **Image (server → browser):**
//! ```json
//! {"type": "image", "url": "https://...", "caption": "..."}
//! ```

use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use nanobot_bus::events::InboundMessage;
use nanobot_core::{MessageType, Platform, SessionSource};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::base::{BaseChannel, SendResult};

// ---------------------------------------------------------------------------
// Default listen address
// ---------------------------------------------------------------------------

/// Default WebSocket listen address.
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:8090";

// ---------------------------------------------------------------------------
// Inbound / outbound JSON helpers
// ---------------------------------------------------------------------------

/// Inbound message from a browser client.
#[derive(Debug, serde::Deserialize)]
struct WsInboundMessage {
    #[serde(default = "default_user_role")]
    role: String,
    #[serde(default)]
    content: String,
}

fn default_user_role() -> String {
    "user".to_string()
}

/// Outbound text message to a browser client.
#[derive(Debug, serde::Serialize)]
struct WsOutboundMessage<'a> {
    role: &'a str,
    content: &'a str,
}

/// Outbound typing indicator.
#[derive(Debug, serde::Serialize)]
struct WsTypingIndicator {
    r#type: &'static str,
}

/// Outbound image message.
#[derive(Debug, serde::Serialize)]
struct WsImageMessage<'a> {
    r#type: &'static str,
    url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    caption: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// WebSocketChannel
// ---------------------------------------------------------------------------

/// WebSocket channel adapter — a server that browser chat clients connect to.
///
/// Each connected browser tab is tracked as a separate client with a unique
/// UUID-based `client_id`. The `client_id` is used as `chat_id` in the
/// message bus, so each browser session gets its own agent conversation.
pub struct WebSocketChannel {
    /// Address to listen on (e.g. "127.0.0.1:8090").
    listen_addr: String,
    /// Whether the server is running.
    connected: bool,
    /// Handler to forward inbound messages to the bus.
    message_handler: Option<mpsc::Sender<InboundMessage>>,
    /// Running flag for graceful shutdown.
    running: Arc<AtomicBool>,
    /// Connected clients: client_id → outbound sender.
    clients: Arc<DashMap<String, mpsc::UnboundedSender<String>>>,
}

impl WebSocketChannel {
    /// Create a new WebSocket channel with the default listen address.
    pub fn new() -> Self {
        Self {
            listen_addr: DEFAULT_LISTEN_ADDR.to_string(),
            connected: false,
            message_handler: None,
            running: Arc::new(AtomicBool::new(false)),
            clients: Arc::new(DashMap::new()),
        }
    }

    /// Create with a custom listen address (for testing or custom config).
    pub fn with_addr(addr: String) -> Self {
        Self {
            listen_addr: addr,
            connected: false,
            message_handler: None,
            running: Arc::new(AtomicBool::new(false)),
            clients: Arc::new(DashMap::new()),
        }
    }

    /// Number of currently connected clients.
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Run the accept loop — binds a TCP listener and accepts WebSocket
    /// connections until `running` is cleared.
    async fn run_accept_loop(
        listener: TcpListener,
        handler: mpsc::Sender<InboundMessage>,
        running: Arc<AtomicBool>,
        clients: Arc<DashMap<String, mpsc::UnboundedSender<String>>>,
    ) {
        use futures::StreamExt;

        info!(
            "WebSocket server listening on {}",
            listener.local_addr().expect("listener was just bound")
        );

        while running.load(Ordering::Relaxed) {
            // Accept with timeout so we can check the running flag.
            let accept = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                listener.accept(),
            )
            .await;

            let (stream, addr) = match accept {
                Ok(Ok((s, a))) => (s, a),
                Ok(Err(e)) => {
                    error!("WebSocket accept error: {}", e);
                    continue;
                }
                Err(_) => continue, // timeout — check running flag
            };

            let ws_stream = match tokio_tungstenite::accept_async(stream).await {
                Ok(ws) => ws,
                Err(e) => {
                    warn!("WebSocket handshake failed from {}: {}", addr, e);
                    continue;
                }
            };

            let client_id = uuid::Uuid::new_v4().to_string();
            info!("WebSocket client connected: {} from {}", client_id, addr);

            let (client_tx, client_rx) = mpsc::unbounded_channel::<String>();
            clients.insert(client_id.clone(), client_tx);

            let (sink, stream) = ws_stream.split();

            // Spawn tasks for this client.
            let read_handler = handler.clone();
            let read_client_id = client_id.clone();
            let read_clients = clients.clone();
            let read_running = running.clone();

            let write_client_id = client_id.clone();
            let write_clients = clients.clone();
            let write_running = running.clone();

            tokio::spawn(async move {
                Self::write_loop(
                    sink,
                    client_rx,
                    write_client_id,
                    write_clients,
                    write_running,
                )
                .await;
            });

            tokio::spawn(async move {
                Self::read_loop(
                    stream,
                    read_client_id,
                    read_handler,
                    read_clients,
                    read_running,
                )
                .await;
            });
        }

        info!("WebSocket accept loop stopped");
    }

    /// Read loop for a single client — parses inbound messages and forwards
    /// them to the message bus handler.
    async fn read_loop(
        stream: futures::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        >,
        client_id: String,
        handler: mpsc::Sender<InboundMessage>,
        clients: Arc<DashMap<String, mpsc::UnboundedSender<String>>>,
        running: Arc<AtomicBool>,
    ) {
        use futures::StreamExt;
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let mut stream = stream;

        while let Some(msg_result) = stream.next().await {
            if !running.load(Ordering::Relaxed) {
                break;
            }

            let msg = match msg_result {
                Ok(WsMessage::Text(text)) => text,
                Ok(WsMessage::Close(_)) => {
                    debug!("WebSocket client {} closed connection", client_id);
                    break;
                }
                Ok(WsMessage::Ping(_data)) => {
                    // tungstenite auto-replies with pong
                    continue;
                }
                Ok(_) => continue,
                Err(e) => {
                    warn!("WebSocket read error for {}: {}", client_id, e);
                    break;
                }
            };

            // Parse the inbound message.
            let ws_msg: WsInboundMessage = match serde_json::from_str(&msg) {
                Ok(m) => m,
                Err(e) => {
                    debug!(
                        "WebSocket invalid JSON from {}: {} — raw: {}",
                        client_id, e, msg
                    );
                    continue;
                }
            };

            // Skip empty messages.
            if ws_msg.content.is_empty() {
                continue;
            }

            let message_type = if ws_msg.content.starts_with('/') {
                MessageType::Command
            } else {
                MessageType::Text
            };

            let source = SessionSource {
                platform: Platform::WebSocket,
                chat_id: client_id.clone(),
                chat_name: None,
                chat_type: "dm".to_string(),
                user_id: Some(client_id.clone()),
                user_name: None,
                thread_id: None,
                chat_topic: None,
            };

            let mut metadata = HashMap::new();
            metadata.insert("ws_role".to_string(), serde_json::json!(ws_msg.role));

            let inbound = InboundMessage {
                channel: Platform::WebSocket,
                sender_id: client_id.clone(),
                chat_id: client_id.clone(),
                content: ws_msg.content,
                media: vec![],
                metadata,
                source: Some(source),
                message_type,
                message_id: None,
                reply_to: None,
                timestamp: chrono::Local::now(),
            };

            if let Err(e) = handler.send(inbound).await {
                warn!(
                    "Failed to forward WebSocket message from {}: {}",
                    client_id, e
                );
            }
        }

        // Client disconnected — clean up.
        clients.remove(&client_id);
        info!("WebSocket client disconnected: {}", client_id);
    }

    /// Write loop for a single client — forwards outbound messages to the
    /// WebSocket sink.
    async fn write_loop(
        sink: futures::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
            tokio_tungstenite::tungstenite::Message,
        >,
        mut rx: mpsc::UnboundedReceiver<String>,
        client_id: String,
        clients: Arc<DashMap<String, mpsc::UnboundedSender<String>>>,
        running: Arc<AtomicBool>,
    ) {
        use futures::SinkExt;
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let mut sink = sink;

        while running.load(Ordering::Relaxed) {
            let msg = match rx.recv().await {
                Some(m) => m,
                None => break,
            };

            if let Err(e) = sink.send(WsMessage::Text(msg.into())).await {
                warn!("WebSocket write error for {}: {}", client_id, e);
                break;
            }
        }

        // Clean up.
        clients.remove(&client_id);
        let _ = sink.close().await;
        debug!("WebSocket write loop ended for {}", client_id);
    }
}

impl Default for WebSocketChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BaseChannel for WebSocketChannel {
    fn name(&self) -> &str {
        "websocket"
    }

    fn platform(&self) -> Platform {
        Platform::WebSocket
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    /// Bind the TCP listener and start the WebSocket accept loop.
    async fn connect(&mut self) -> Result<bool> {
        let listener = TcpListener::bind(&self.listen_addr).await?;
        info!("WebSocket channel bound to {}", self.listen_addr);

        self.connected = true;
        self.running.store(true, Ordering::Relaxed);

        if let Some(handler) = self.message_handler.clone() {
            let running = self.running.clone();
            let clients = self.clients.clone();
            let listen_addr = listener.local_addr()?.to_string();
            tokio::spawn(async move {
                Self::run_accept_loop(listener, handler, running, clients).await;
                info!("WebSocket server on {} stopped", listen_addr);
            });
            info!("WebSocket accept loop spawned on {}", self.listen_addr);
        } else {
            warn!("WebSocket channel has no message handler set; not starting accept loop");
        }

        Ok(true)
    }

    /// Stop the server and disconnect all clients.
    async fn disconnect(&mut self) -> Result<()> {
        self.running.store(false, Ordering::Relaxed);
        self.clients.clear();
        self.connected = false;
        info!("WebSocket channel disconnected");
        Ok(())
    }

    /// Send a text message to a specific client.
    async fn send_message(
        &self,
        chat_id: &str,
        content: &str,
        _reply_to: Option<&str>,
    ) -> Result<SendResult> {
        let client = match self.clients.get(chat_id) {
            Some(c) => c,
            None => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("WebSocket client '{}' not connected", chat_id)),
                    retryable: false,
                });
            }
        };

        let msg = WsOutboundMessage {
            role: "assistant",
            content,
        };
        let json = serde_json::to_string(&msg)?;

        match client.send(json) {
            Ok(()) => {
                debug!("Sent message to WebSocket client {}", chat_id);
                Ok(SendResult {
                    success: true,
                    message_id: Some(format!("ws_{}", chat_id)),
                    error: None,
                    retryable: false,
                })
            }
            Err(e) => {
                warn!("Failed to send to WebSocket client {}: {}", chat_id, e);
                Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("WebSocket send error: {}", e)),
                    retryable: true,
                })
            }
        }
    }

    /// Send a typing indicator to a specific client.
    async fn send_typing(&self, chat_id: &str) -> Result<()> {
        if let Some(client) = self.clients.get(chat_id) {
            let msg = WsTypingIndicator { r#type: "typing" };
            let json = serde_json::to_string(&msg)?;
            let _ = client.send(json);
        }
        Ok(())
    }

    /// Send an image to a specific client.
    async fn send_image(
        &self,
        chat_id: &str,
        image_url: &str,
        caption: Option<&str>,
    ) -> Result<SendResult> {
        let client = match self.clients.get(chat_id) {
            Some(c) => c,
            None => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("WebSocket client '{}' not connected", chat_id)),
                    retryable: false,
                });
            }
        };

        let msg = WsImageMessage {
            r#type: "image",
            url: image_url,
            caption,
        };
        let json = serde_json::to_string(&msg)?;

        match client.send(json) {
            Ok(()) => Ok(SendResult {
                success: true,
                message_id: Some(format!("ws_img_{}", chat_id)),
                error: None,
                retryable: false,
            }),
            Err(e) => Ok(SendResult {
                success: false,
                message_id: None,
                error: Some(format!("WebSocket image send error: {}", e)),
                retryable: true,
            }),
        }
    }

    fn set_message_handler(&mut self, handler: mpsc::Sender<InboundMessage>) {
        self.message_handler = Some(handler);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    // -----------------------------------------------------------------------
    // Basic construction tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_websocket_new() {
        let channel = WebSocketChannel::new();
        assert_eq!(channel.name(), "websocket");
        assert_eq!(channel.platform(), Platform::WebSocket);
        assert!(!channel.is_connected());
        assert_eq!(channel.client_count(), 0);
    }

    #[test]
    fn test_websocket_default() {
        let channel = WebSocketChannel::default();
        assert_eq!(channel.name(), "websocket");
        assert!(!channel.is_connected());
    }

    #[test]
    fn test_websocket_with_custom_addr() {
        let channel = WebSocketChannel::with_addr("0.0.0.0:9999".to_string());
        assert!(!channel.is_connected());
    }

    #[tokio::test]
    async fn test_websocket_disconnect() {
        let mut channel = WebSocketChannel::new();
        channel.connected = true;
        channel.disconnect().await.unwrap();
        assert!(!channel.is_connected());
        assert!(!channel.running.load(Ordering::Relaxed));
        assert_eq!(channel.client_count(), 0);
    }

    #[tokio::test]
    async fn test_websocket_set_message_handler() {
        let mut channel = WebSocketChannel::new();
        let (tx, _rx) = mpsc::channel(10);
        channel.set_message_handler(tx);
        assert!(channel.message_handler.is_some());
    }

    // -----------------------------------------------------------------------
    // JSON message format tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_inbound_message_parse() {
        let json = r#"{"role": "user", "content": "Hello"}"#;
        let msg: WsInboundMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.role, "user");
        assert_eq!(msg.content, "Hello");
    }

    #[test]
    fn test_inbound_message_defaults_role() {
        let json = r#"{"content": "Hi"}"#;
        let msg: WsInboundMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.role, "user");
        assert_eq!(msg.content, "Hi");
    }

    #[test]
    fn test_outbound_message_format() {
        let msg = WsOutboundMessage {
            role: "assistant",
            content: "The answer is 42.",
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["role"], "assistant");
        assert_eq!(parsed["content"], "The answer is 42.");
    }

    #[test]
    fn test_typing_indicator_format() {
        let msg = WsTypingIndicator { r#type: "typing" };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "typing");
    }

    #[test]
    fn test_image_message_format() {
        let msg = WsImageMessage {
            r#type: "image",
            url: "https://example.com/img.png",
            caption: Some("a caption"),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "image");
        assert_eq!(parsed["url"], "https://example.com/img.png");
        assert_eq!(parsed["caption"], "a caption");
    }

    #[test]
    fn test_image_message_no_caption() {
        let msg = WsImageMessage {
            r#type: "image",
            url: "https://example.com/img.png",
            caption: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("caption").is_none());
    }

    // -----------------------------------------------------------------------
    // Live WebSocket tests (bind to random port)
    // -----------------------------------------------------------------------

    /// Helper: bind a WebSocket server on a random port and return the address.
    async fn get_random_addr() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        addr
    }

    /// Helper: create a server channel bound to a random port.
    async fn setup_server() -> (WebSocketChannel, String, mpsc::Receiver<InboundMessage>) {
        let addr = get_random_addr().await;
        let mut channel = WebSocketChannel::with_addr(addr.clone());
        let (tx, rx) = mpsc::channel(100);
        channel.set_message_handler(tx);
        (channel, addr, rx)
    }

    #[tokio::test]
    async fn test_connect_starts_server() {
        let (mut channel, _addr, _rx) = setup_server().await;
        let result = channel.connect().await.unwrap();
        assert!(result);
        assert!(channel.is_connected());

        channel.disconnect().await.unwrap();
        assert!(!channel.is_connected());
    }

    #[tokio::test]
    async fn test_client_connect_and_track() {
        let (mut channel, addr, _rx) = setup_server().await;
        channel.connect().await.unwrap();

        // Connect a test client.
        let (_ws, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
            .await
            .unwrap();

        // Give the server a moment to register the client.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(channel.client_count(), 1);

        channel.disconnect().await.unwrap();
    }

    #[tokio::test]
    async fn test_send_message_to_client() {
        let (mut channel, addr, _rx) = setup_server().await;
        channel.connect().await.unwrap();

        // Connect a test client.
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Get the client_id (the only connected client).
        let client_id: String = channel
            .clients
            .iter()
            .next()
            .map(|e| e.key().clone())
            .unwrap();

        // Send a message to the client.
        let result = channel
            .send_message(&client_id, "Hello from server!", None)
            .await
            .unwrap();
        assert!(result.success);

        // Client should receive the message.
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        match msg {
            WsMessage::Text(text) => {
                let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["role"], "assistant");
                assert_eq!(parsed["content"], "Hello from server!");
            }
            _ => panic!("Expected text message"),
        }

        channel.disconnect().await.unwrap();
    }

    #[tokio::test]
    async fn test_send_message_to_disconnected_client() {
        let (mut channel, addr, _rx) = setup_server().await;
        channel.connect().await.unwrap();

        // Connect and immediately drop.
        {
            let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
                .await
                .unwrap();
            drop(ws);
        }

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Sending to an unknown client should fail gracefully.
        let result = channel
            .send_message("nonexistent_client", "Hello", None)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(!result.retryable);

        channel.disconnect().await.unwrap();
    }

    #[tokio::test]
    async fn test_send_image_to_client() {
        let (mut channel, addr, _rx) = setup_server().await;
        channel.connect().await.unwrap();

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let client_id: String = channel
            .clients
            .iter()
            .next()
            .map(|e| e.key().clone())
            .unwrap();

        let result = channel
            .send_image(&client_id, "https://example.com/img.png", Some("caption"))
            .await
            .unwrap();
        assert!(result.success);

        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        match msg {
            WsMessage::Text(text) => {
                let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["type"], "image");
                assert_eq!(parsed["url"], "https://example.com/img.png");
                assert_eq!(parsed["caption"], "caption");
            }
            _ => panic!("Expected text message"),
        }

        channel.disconnect().await.unwrap();
    }

    #[tokio::test]
    async fn test_multiple_clients() {
        let (mut channel, addr, _rx) = setup_server().await;
        channel.connect().await.unwrap();

        let (mut ws1, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
            .await
            .unwrap();
        let (mut ws2, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert_eq!(channel.client_count(), 2);

        // Get both client IDs.
        let client_ids: Vec<String> =
            channel.clients.iter().map(|e| e.key().clone()).collect();
        assert_eq!(client_ids.len(), 2);

        // Send to each client independently.
        for id in &client_ids {
            let result = channel.send_message(id, "Hello!", None).await.unwrap();
            assert!(result.success);
        }

        // Both clients should receive messages.
        let _msg1 = tokio::time::timeout(std::time::Duration::from_secs(2), ws1.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let msg2 = tokio::time::timeout(std::time::Duration::from_secs(2), ws2.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        assert!(matches!(_msg1, WsMessage::Text(_)));
        assert!(matches!(msg2, WsMessage::Text(_)));

        channel.disconnect().await.unwrap();
    }

    // -----------------------------------------------------------------------
    // send_message when not connected
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_send_message_no_clients() {
        let channel = WebSocketChannel::new();
        let result = channel
            .send_message("some_client", "Hello", None)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(!result.retryable);
    }

    // -----------------------------------------------------------------------
    // Default listen address constant
    // -----------------------------------------------------------------------

    #[test]
    fn test_default_listen_addr() {
        assert_eq!(DEFAULT_LISTEN_ADDR, "127.0.0.1:8090");
    }
}
