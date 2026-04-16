//! WebSocket channel adapter — server for browser-based chat clients.
//!
//! Runs a WebSocket server that browsers connect to. Each connected client
//! becomes an independent chat session. Messages use a rich envelope protocol
//! with backward compatibility for the old `{role, content}` JSON format.
//!
//! ## Envelope protocol (new)
//!
//! **Inbound (browser → server):**
//! ```json
//! {"type": "message", "id": "uuid", "content": "Hello"}
//! ```
//!
//! **Outbound (server → browser):**
//! ```json
//! {"type": "message", "id": "uuid", "content": "Response text"}
//! ```
//!
//! **Ping/Pong:**
//! ```json
//! {"type": "ping"}
//! // Server responds:
//! {"type": "pong", "id": "uuid"}
//! ```
//!
//! **Welcome (sent on connect):**
//! ```json
//! {"type": "welcome", "id": "uuid", "client_id": "...", "server_version": "0.1.0"}
//! ```
//!
//! **Error:**
//! ```json
//! {"type": "error", "id": "uuid", "code": "bad_request", "message": "..."}
//! ```
//!
//! ## Legacy format (still accepted)
//!
//! **Inbound:**
//! ```json
//! {"role": "user", "content": "Hello"}
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
// Constants
// ---------------------------------------------------------------------------

/// Default WebSocket listen address.
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:8090";

/// Server version reported in welcome messages.
const SERVER_VERSION: &str = "0.1.0";

// ---------------------------------------------------------------------------
// Rich message envelope
// ---------------------------------------------------------------------------

/// WebSocket message envelope — structured bidirectional protocol.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WsEnvelope {
    /// Message type: message, streaming, tool_call, tool_result, error, pong, welcome, auth.
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Unique message ID (UUID).
    #[serde(default)]
    pub id: String,
    /// For request-response correlation.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// Message content (for type=message/error/auth).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Streaming chunk text (for type=streaming).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk: Option<String>,
    /// Whether streaming is complete (for type=streaming).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done: Option<bool>,
    /// Tool name (for type=tool_call).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Tool arguments (for type=tool_call).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
    /// Error code (for type=error).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Client ID (for type=welcome).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Server version (for type=welcome).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
}

impl WsEnvelope {
    /// Create a new envelope with a random UUID.
    pub fn new(msg_type: &str) -> Self {
        Self {
            msg_type: msg_type.to_string(),
            id: uuid::Uuid::new_v4().to_string(),
            reply_to: None,
            content: None,
            chunk: None,
            done: None,
            tool: None,
            args: None,
            code: None,
            client_id: None,
            server_version: None,
        }
    }

    /// Create a message envelope with content.
    pub fn message(content: &str) -> Self {
        let mut env = Self::new("message");
        env.content = Some(content.to_string());
        env
    }

    /// Create a pong response.
    pub fn pong() -> Self {
        Self::new("pong")
    }

    /// Create a welcome message.
    pub fn welcome(client_id: &str) -> Self {
        let mut env = Self::new("welcome");
        env.client_id = Some(client_id.to_string());
        env.server_version = Some(SERVER_VERSION.to_string());
        env
    }

    /// Create an error envelope.
    pub fn error(code: &str, message: &str) -> Self {
        let mut env = Self::new("error");
        env.code = Some(code.to_string());
        env.content = Some(message.to_string());
        env
    }

    /// Serialize to JSON string.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }
}

// ---------------------------------------------------------------------------
// Legacy format helpers
// ---------------------------------------------------------------------------

/// Inbound message from a browser client (legacy format).
#[derive(Debug, serde::Deserialize)]
struct WsInboundMessage {
    #[serde(default = "default_user_role")]
    #[allow(dead_code)]
    role: String,
    #[serde(default)]
    content: String,
}

fn default_user_role() -> String {
    "user".to_string()
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
            let accept =
                tokio::time::timeout(std::time::Duration::from_secs(1), listener.accept()).await;

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

            // Send welcome message before inserting (uses the sender).
            let welcome = WsEnvelope::welcome(&client_id);
            if let Ok(json) = welcome.to_json() {
                if client_tx.send(json).is_err() {
                    warn!("Failed to send welcome to {}", client_id);
                }
            }

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

            // Try to parse as a generic JSON value first.
            let raw_value: serde_json::Value = match serde_json::from_str(&msg) {
                Ok(v) => v,
                Err(e) => {
                    debug!(
                        "WebSocket invalid JSON from {}: {} — raw: {}",
                        client_id, e, msg
                    );
                    continue;
                }
            };

            // Detect format: envelope has "type", legacy has "role" + "content".
            let content_text = if raw_value.get("type").is_some() {
                // New envelope format.
                let envelope: WsEnvelope = match serde_json::from_value(raw_value.clone()) {
                    Ok(e) => e,
                    Err(e) => {
                        debug!(
                            "WebSocket invalid envelope from {}: {} — raw: {}",
                            client_id, e, msg
                        );
                        continue;
                    }
                };

                match envelope.msg_type.as_str() {
                    "ping" => {
                        // Respond with pong.
                        let pong = WsEnvelope::pong();
                        if let Ok(json) = pong.to_json() {
                            if let Some(client_tx) = clients.get(&client_id) {
                                let _ = client_tx.send(json);
                            }
                        }
                        continue;
                    }
                    "message" => envelope.content.clone().unwrap_or_default(),
                    _ => {
                        // Ignore unknown envelope types for now.
                        debug!(
                            "WebSocket unknown envelope type '{}' from {}",
                            envelope.msg_type, client_id
                        );
                        continue;
                    }
                }
            } else if raw_value.get("role").is_some() {
                // Legacy {role, content} format — backward compat.
                let legacy: WsInboundMessage = match serde_json::from_value(raw_value) {
                    Ok(m) => m,
                    Err(e) => {
                        debug!(
                            "WebSocket invalid legacy message from {}: {} — raw: {}",
                            client_id, e, msg
                        );
                        continue;
                    }
                };
                legacy.content
            } else {
                debug!(
                    "WebSocket unrecognized message format from {}: {}",
                    client_id, msg
                );
                continue;
            };

            // Skip empty messages.
            if content_text.is_empty() {
                continue;
            }

            let message_type = if content_text.starts_with('/') {
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

            let metadata = HashMap::new();

            let inbound = InboundMessage {
                channel: Platform::WebSocket,
                sender_id: client_id.clone(),
                chat_id: client_id.clone(),
                content: content_text,
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

    /// Send a text message to a specific client using envelope format.
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

        let envelope = WsEnvelope::message(content);
        let json = match envelope.to_json() {
            Ok(j) => j,
            Err(e) => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("Failed to serialize message: {}", e)),
                    retryable: false,
                });
            }
        };

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

    /// Send an image to a specific client using envelope format.
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
    use futures::{SinkExt, StreamExt};
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
    // Envelope tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_envelope_message() {
        let env = WsEnvelope::message("Hello");
        assert_eq!(env.msg_type, "message");
        assert_eq!(env.content.unwrap(), "Hello");
        assert!(!env.id.is_empty());
    }

    #[test]
    fn test_envelope_welcome() {
        let env = WsEnvelope::welcome("client-123");
        assert_eq!(env.msg_type, "welcome");
        assert_eq!(env.client_id.unwrap(), "client-123");
        assert_eq!(env.server_version.unwrap(), SERVER_VERSION);
    }

    #[test]
    fn test_envelope_pong() {
        let env = WsEnvelope::pong();
        assert_eq!(env.msg_type, "pong");
        assert!(!env.id.is_empty());
    }

    #[test]
    fn test_envelope_error() {
        let env = WsEnvelope::error("bad_request", "Invalid input");
        assert_eq!(env.msg_type, "error");
        assert_eq!(env.code.unwrap(), "bad_request");
        assert_eq!(env.content.unwrap(), "Invalid input");
    }

    #[test]
    fn test_envelope_serialization() {
        let env = WsEnvelope::message("test");
        let json = env.to_json().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "message");
        assert_eq!(parsed["content"], "test");
        assert!(parsed.get("id").is_some());
        // Optional fields should not appear when None
        assert!(parsed.get("reply_to").is_none());
        assert!(parsed.get("chunk").is_none());
        assert!(parsed.get("done").is_none());
        assert!(parsed.get("tool").is_none());
        assert!(parsed.get("args").is_none());
        assert!(parsed.get("code").is_none());
        assert!(parsed.get("client_id").is_none());
        assert!(parsed.get("server_version").is_none());
    }

    #[test]
    fn test_envelope_deserialization() {
        let json = r#"{"type":"message","id":"test-id","content":"hello","reply_to":"prev-id"}"#;
        let env: WsEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.msg_type, "message");
        assert_eq!(env.id, "test-id");
        assert_eq!(env.content.unwrap(), "hello");
        assert_eq!(env.reply_to.unwrap(), "prev-id");
    }

    #[test]
    fn test_envelope_full_fields() {
        let json = r#"{
            "type": "streaming",
            "id": "s1",
            "reply_to": "m1",
            "content": null,
            "chunk": "Hello ",
            "done": false,
            "tool": "search",
            "args": {"query": "test"},
            "code": null,
            "client_id": null,
            "server_version": null
        }"#;
        let env: WsEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(env.msg_type, "streaming");
        assert_eq!(env.chunk.unwrap(), "Hello ");
        assert_eq!(env.done.unwrap(), false);
        assert_eq!(env.tool.unwrap(), "search");
        assert_eq!(env.args.unwrap()["query"], "test");
    }

    // -----------------------------------------------------------------------
    // Legacy JSON message format tests (backward compat)
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
    async fn test_welcome_message_on_connect() {
        let (mut channel, addr, _rx) = setup_server().await;
        channel.connect().await.unwrap();

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
            .await
            .unwrap();

        // First message should be welcome.
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        match msg {
            WsMessage::Text(text) => {
                let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["type"], "welcome");
                assert!(parsed["id"].is_string());
                assert!(parsed["client_id"].is_string());
                assert_eq!(parsed["server_version"], SERVER_VERSION);
            }
            _ => panic!("Expected text message"),
        }

        channel.disconnect().await.unwrap();
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
    async fn test_send_envelope_message_to_client() {
        let (mut channel, addr, _rx) = setup_server().await;
        channel.connect().await.unwrap();

        // Connect a test client.
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Drain the welcome message.
        let _welcome = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

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

        // Client should receive an envelope message.
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        match msg {
            WsMessage::Text(text) => {
                let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["type"], "message");
                assert_eq!(parsed["content"], "Hello from server!");
                assert!(parsed["id"].is_string());
            }
            _ => panic!("Expected text message"),
        }

        channel.disconnect().await.unwrap();
    }

    #[tokio::test]
    async fn test_backward_compat_legacy_inbound() {
        let (mut channel, addr, mut rx) = setup_server().await;
        channel.connect().await.unwrap();

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Drain the welcome message.
        let _welcome = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        // Send a legacy {role, content} message.
        let legacy_msg = r#"{"role":"user","content":"legacy hello"}"#;
        ws.send(WsMessage::Text(legacy_msg.into())).await.unwrap();

        // The message should be received as an InboundMessage.
        let inbound = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inbound.content, "legacy hello");
        assert_eq!(inbound.channel, Platform::WebSocket);

        channel.disconnect().await.unwrap();
    }

    #[tokio::test]
    async fn test_envelope_inbound_message() {
        let (mut channel, addr, mut rx) = setup_server().await;
        channel.connect().await.unwrap();

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Drain the welcome message.
        let _welcome = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        // Send a new envelope format message.
        let envelope = WsEnvelope::message("envelope hello");
        let json = envelope.to_json().unwrap();
        ws.send(WsMessage::Text(json.into())).await.unwrap();

        let inbound = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inbound.content, "envelope hello");
        assert_eq!(inbound.channel, Platform::WebSocket);

        channel.disconnect().await.unwrap();
    }

    #[tokio::test]
    async fn test_ping_pong() {
        let (mut channel, addr, _rx) = setup_server().await;
        channel.connect().await.unwrap();

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}", addr))
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Drain the welcome message.
        let _welcome = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        // Send a ping envelope.
        let ping_json = r#"{"type":"ping","id":"ping-1"}"#;
        ws.send(WsMessage::Text(ping_json.into())).await.unwrap();

        // Should receive a pong.
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        match msg {
            WsMessage::Text(text) => {
                let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(parsed["type"], "pong");
                assert!(parsed["id"].is_string());
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

        // Drain the welcome message.
        let _welcome = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

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

        // Drain welcome messages.
        let _w1 = tokio::time::timeout(std::time::Duration::from_secs(2), ws1.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let _w2 = tokio::time::timeout(std::time::Duration::from_secs(2), ws2.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        // Get both client IDs.
        let client_ids: Vec<String> = channel.clients.iter().map(|e| e.key().clone()).collect();
        assert_eq!(client_ids.len(), 2);

        // Send to each client independently.
        for id in &client_ids {
            let result = channel.send_message(id, "Hello!", None).await.unwrap();
            assert!(result.success);
        }

        // Both clients should receive envelope messages.
        let msg1 = tokio::time::timeout(std::time::Duration::from_secs(2), ws1.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let msg2 = tokio::time::timeout(std::time::Duration::from_secs(2), ws2.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        assert!(matches!(msg1, WsMessage::Text(_)));
        assert!(matches!(msg2, WsMessage::Text(_)));

        // Verify envelope format.
        if let WsMessage::Text(text) = msg1 {
            let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(parsed["type"], "message");
            assert_eq!(parsed["content"], "Hello!");
        }

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

    #[test]
    fn test_server_version() {
        assert_eq!(SERVER_VERSION, "0.1.0");
    }
}
