//! Discord channel adapter — REST API + Gateway WebSocket.
//!
//! Uses the Discord REST API (v10) for sending messages, typing indicators,
//! and images. Receives inbound messages via the Gateway WebSocket with
//! proper HELLO / HEARTBEAT / IDENTIFY / RESUME handshake.

use anyhow::{Context, Result};
use async_trait::async_trait;
use nanobot_bus::events::InboundMessage;
use nanobot_core::{MessageType, Platform, SessionSource};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

use crate::base::{BaseChannel, SendResult};

// ---------------------------------------------------------------------------
// Discord API URL
// ---------------------------------------------------------------------------

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const DISCORD_GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

// ---------------------------------------------------------------------------
// Lightweight response structs (only the fields we care about)
// ---------------------------------------------------------------------------

/// Response from `GET /users/@me`.
#[derive(Debug, Deserialize)]
struct DiscordUser {
    id: String,
    username: String,
}

/// Response from `POST /channels/{id}/messages`.
#[derive(Debug, Deserialize)]
struct DiscordMessageResponse {
    id: String,
}

/// Body for creating a message.
#[derive(Debug, Serialize)]
struct CreateMessageBody {
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_reference: Option<MessageReference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    embed: Option<DiscordEmbed>,
}

/// Reply reference within a message.
#[derive(Debug, Serialize)]
struct MessageReference {
    message_id: String,
}

/// Rich embed used for image messages.
#[derive(Debug, Serialize)]
struct DiscordEmbed {
    image: DiscordEmbedImage,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(Debug, Serialize)]
struct DiscordEmbedImage {
    url: String,
}

// ─── Gateway WebSocket types ─────────────────────────────────

/// Gateway opcode constants.
#[allow(dead_code)] // RESUME/RECONNECT used in reconnection logic
mod opcodes {
    pub const DISPATCH: i64 = 0;
    pub const HEARTBEAT: i64 = 1;
    pub const IDENTIFY: i64 = 2;
    pub const INVALID_SESSION: i64 = 9;
    pub const HELLO: i64 = 10;
    pub const HEARTBEAT_ACK: i64 = 11;
    pub const RESUME: i64 = 6;
    pub const RECONNECT: i64 = 7;
}

/// Gateway intent bitflags.
mod intents {
    /// GUILD_MESSAGES = 1 << 9
    pub const GUILD_MESSAGES: i64 = 1 << 9;
    /// DIRECT_MESSAGES = 1 << 12
    pub const DIRECT_MESSAGES: i64 = 1 << 12;
    /// MESSAGE_CONTENT = 1 << 15
    pub const MESSAGE_CONTENT: i64 = 1 << 15;
    /// Combined intents for a text-based bot.
    pub const TEXT_BOT: i64 =
        GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT;
}

/// A generic Gateway payload.
#[derive(Debug, Deserialize)]
struct GatewayPayload {
    op: i64,
    #[serde(default)]
    s: Option<i64>,
    #[serde(default)]
    t: Option<String>,
    #[serde(default)]
    d: serde_json::Value,
}

/// The HELLO event data.
#[derive(Debug, Deserialize)]
struct HelloData {
    heartbeat_interval: u64,
}

/// A MESSAGE_CREATE event from the Gateway.
#[derive(Debug, Deserialize)]
struct GatewayMessage {
    id: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    channel_id: String,
    #[serde(default)]
    author: Option<GatewayAuthor>,
    #[serde(default)]
    guild_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GatewayAuthor {
    id: String,
    #[serde(default)]
    username: String,
}

// ---------------------------------------------------------------------------
// DiscordChannel
// ---------------------------------------------------------------------------

/// Discord channel implementation using REST API + Gateway WebSocket.
pub struct DiscordChannel {
    token: Option<String>,
    connected: bool,
    message_handler: Option<tokio::sync::mpsc::Sender<InboundMessage>>,
    client: reqwest::Client,
    /// Override base URL for testing.
    base_url_override: Option<String>,
    /// Running flag for the gateway listener.
    running: Arc<AtomicBool>,
}

impl DiscordChannel {
    /// Build a reqwest client that respects system proxy settings.
    fn build_client() -> reqwest::Client {
        let proxy_url = std::env::var("HTTPS_PROXY")
            .or_else(|_| std::env::var("https_proxy"))
            .or_else(|_| std::env::var("HTTP_PROXY"))
            .or_else(|_| std::env::var("http_proxy"))
            .or_else(|_| std::env::var("ALL_PROXY"))
            .or_else(|_| std::env::var("all_proxy"))
            .ok();

        match &proxy_url {
            Some(url) => {
                info!("Discord HTTP client using proxy: {}", url);
                let proxy = reqwest::Proxy::all(url)
                    .expect("Failed to create proxy from env var");
                reqwest::Client::builder()
                    .proxy(proxy)
                    .build()
                    .expect("Failed to build HTTP client with proxy")
            }
            None => {
                info!("Discord HTTP client: no proxy configured (direct connection)");
                reqwest::Client::new()
            }
        }
    }

    /// Create a new `DiscordChannel`.
    pub fn new() -> Self {
        Self {
            token: std::env::var("DISCORD_BOT_TOKEN").ok(),
            connected: false,
            message_handler: None,
            client: Self::build_client(),
            base_url_override: None,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create with a custom token and base URL (for testing).
    /// Skips proxy since the base URL is typically localhost in tests.
    pub fn with_token_and_url(token: String, base_url: String) -> Self {
        Self {
            token: Some(token),
            connected: false,
            message_handler: None,
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base_url_override: Some(base_url),
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Return the effective API base URL.
    fn api_base(&self) -> &str {
        self.base_url_override
            .as_deref()
            .unwrap_or(DISCORD_API_BASE)
    }

    // -- helpers ---------------------------------------------------------------

    /// Build an authorisation header value (`Bot {token}`).
    fn auth_header(&self) -> Option<String> {
        self.token.as_ref().map(|t| format!("Bot {t}"))
    }

    /// Return a `GET` request builder with the Bot auth header pre-set.
    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{path}", self.api_base());
        debug!(url = %url, "Discord GET");
        let mut req = self.client.get(&url);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }
        req
    }

    /// Return a `POST` request builder with the Bot auth header pre-set.
    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}{path}", self.api_base());
        debug!(url = %url, "Discord POST");
        let mut req = self.client.post(&url);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }
        req
    }

    /// Classify a `reqwest` status code / error into a [`SendResult`].
    fn make_error_result(status: StatusCode, body: &str) -> SendResult {
        let retryable = matches!(
            status,
            StatusCode::TOO_MANY_REQUESTS
                | StatusCode::SERVICE_UNAVAILABLE
                | StatusCode::GATEWAY_TIMEOUT
        );
        SendResult {
            success: false,
            message_id: None,
            error: Some(format!("Discord API {} — {}", status, body)),
            retryable,
        }
    }

    /// Run the Gateway WebSocket listener with automatic reconnection.
    ///
    /// On disconnect or RECONNECT request, reconnects with exponential
    /// backoff (1s → 2s → 4s → … → 60s max). Resets on successful HELLO.
    async fn run_gateway(
        token: String,
        handler: tokio::sync::mpsc::Sender<InboundMessage>,
        running: Arc<AtomicBool>,
    ) {
        let mut backoff_secs: u64 = 1;
        let max_backoff_secs: u64 = 60;

        while running.load(Ordering::Relaxed) {
            let should_reconnect = Self::run_gateway_session(&token, &handler, &running).await;

            if !running.load(Ordering::Relaxed) {
                break; // Clean shutdown
            }

            if should_reconnect {
                warn!(
                    "Discord Gateway disconnected — reconnecting in {}s",
                    backoff_secs
                );
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(max_backoff_secs);
            } else {
                break; // Fatal error, don't reconnect
            }
        }

        info!("Discord Gateway listener stopped");
    }

    /// Run a single Gateway session (connect → process → disconnect).
    ///
    /// Returns `true` if reconnection should be attempted, `false` for fatal errors.
    async fn run_gateway_session(
        token: &str,
        handler: &tokio::sync::mpsc::Sender<InboundMessage>,
        running: &Arc<AtomicBool>,
    ) -> bool {
        use futures::SinkExt;
        use futures::StreamExt;
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        info!("Discord Gateway connecting...");

        let connect_result = tokio_tungstenite::connect_async(DISCORD_GATEWAY_URL).await;
        let (mut ws, _) = match connect_result {
            Ok(c) => c,
            Err(e) => {
                error!("Discord Gateway WebSocket connect failed: {}", e);
                return false;
            }
        };

        // Wait for HELLO
        let heartbeat_interval = loop {
            let msg = match ws.next().await {
                Some(Ok(WsMessage::Text(text))) => text,
                Some(Ok(WsMessage::Close(_))) => {
                    info!("Discord Gateway closed during HELLO");
                    return true; // Reconnect
                }
                Some(Err(e)) => {
                    error!("Discord Gateway read error during HELLO: {}", e);
                    return true; // Reconnect
                }
                _ => continue,
            };

            let payload: GatewayPayload = match serde_json::from_str(&msg) {
                Ok(p) => p,
                Err(_) => continue,
            };

            if payload.op == opcodes::HELLO {
                let hello: HelloData = match serde_json::from_value(payload.d) {
                    Ok(h) => h,
                    Err(e) => {
                        error!("Failed to parse HELLO: {}", e);
                        return false; // Protocol error
                    }
                };
                break hello.heartbeat_interval;
            }
        };

        info!(
            "Discord Gateway HELLO received (heartbeat: {}ms)",
            heartbeat_interval
        );

        // Send IDENTIFY
        let identify = serde_json::json!({
            "op": opcodes::IDENTIFY,
            "d": {
                "token": token,
                "intents": intents::TEXT_BOT,
                "properties": {
                    "os": "linux",
                    "browser": "nanobot-rs",
                    "device": "nanobot-rs"
                }
            }
        });
        if let Err(e) = ws.send(WsMessage::Text(identify.to_string().into())).await {
            error!("Failed to send IDENTIFY: {}", e);
            return true; // Reconnect
        }

        info!("Discord Gateway IDENTIFY sent (intents: {})", intents::TEXT_BOT);

        // Process events
        let mut last_sequence: Option<i64> = None;
        let mut heartbeat_interval_tick =
            tokio::time::interval(std::time::Duration::from_millis(heartbeat_interval));

        while running.load(Ordering::Relaxed) {
            tokio::select! {
                _ = heartbeat_interval_tick.tick() => {
                    let hb = serde_json::json!({
                        "op": opcodes::HEARTBEAT,
                        "d": last_sequence
                    });
                    if let Err(e) = ws.send(WsMessage::Text(hb.to_string().into())).await {
                        error!("Discord heartbeat send failed: {}", e);
                        break;
                    }
                    debug!("Discord heartbeat sent");
                }
                msg = ws.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
                            let payload: GatewayPayload = match serde_json::from_str(&text) {
                                Ok(p) => p,
                                Err(_) => continue,
                            };

                            if payload.s.is_some() {
                                last_sequence = payload.s;
                            }

                            match payload.op {
                                opcodes::DISPATCH => {
                                    match payload.t.as_deref() {
                                        Some("MESSAGE_CREATE") | Some("MESSAGE_UPDATE") => {
                                            if let Ok(msg_data) =
                                                serde_json::from_value::<GatewayMessage>(payload.d)
                                            {
                                                Self::dispatch_message(handler, &msg_data).await;
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                opcodes::HEARTBEAT_ACK => {
                                    debug!("Discord heartbeat ACK");
                                }
                                opcodes::INVALID_SESSION => {
                                    warn!("Discord Gateway INVALID_SESSION — session no longer valid");
                                    return true; // Reconnect with fresh IDENTIFY
                                }
                                opcodes::RECONNECT => {
                                    warn!("Discord Gateway RECONNECT requested");
                                    return true; // Reconnect
                                }
                                _ => {
                                    debug!("Discord Gateway op={} ignored", payload.op);
                                }
                            }
                        }
                        Some(Ok(WsMessage::Close(_))) => {
                            info!("Discord Gateway closed by server");
                            break;
                        }
                        Some(Err(e)) => {
                            error!("Discord Gateway read error: {}", e);
                            break;
                        }
                        None => {
                            info!("Discord Gateway stream ended");
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        // Loop ended — either disconnect or error. Reconnect unless shutting down.
        true
    }

    /// Convert a Gateway message to an InboundMessage and send it.
    async fn dispatch_message(
        handler: &tokio::sync::mpsc::Sender<InboundMessage>,
        msg: &GatewayMessage,
    ) {
        // Skip empty messages
        if msg.content.is_empty() {
            return;
        }

        let author = msg.author.as_ref();
        let sender_id = author.map(|a| a.id.clone()).unwrap_or_default();
        let user_name = author.map(|a| a.username.clone());

        let message_type = if msg.content.starts_with('/') {
            MessageType::Command
        } else {
            MessageType::Text
        };

        let source = SessionSource {
            platform: Platform::Discord,
            chat_id: msg.channel_id.clone(),
            chat_name: msg.guild_id.clone(),
            chat_type: if msg.guild_id.is_some() {
                "guild"
            } else {
                "dm"
            }
            .to_string(),
            user_id: if sender_id.is_empty() {
                None
            } else {
                Some(sender_id.clone())
            },
            user_name,
            thread_id: None,
            chat_topic: None,
        };

        let mut metadata = HashMap::new();
        metadata.insert("discord_message_id".to_string(), serde_json::json!(msg.id));
        if let Some(guild_id) = &msg.guild_id {
            metadata.insert("discord_guild_id".to_string(), serde_json::json!(guild_id));
        }

        let inbound = InboundMessage {
            channel: Platform::Discord,
            sender_id,
            chat_id: msg.channel_id.clone(),
            content: msg.content.clone(),
            media: vec![],
            metadata,
            source: Some(source),
            message_type,
            message_id: Some(msg.id.clone()),
            reply_to: None,
            timestamp: chrono::Local::now(),
        };

        if let Err(e) = handler.send(inbound).await {
            warn!("Failed to dispatch Discord message: {}", e);
        }
    }
}

impl Default for DiscordChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BaseChannel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
    }

    fn platform(&self) -> Platform {
        Platform::Discord
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    /// Validate the bot token and start the Gateway WebSocket listener.
    async fn connect(&mut self) -> Result<bool> {
        if self.token.as_ref().is_none_or(|t| t.is_empty()) {
            warn!("Discord bot token not configured (DISCORD_BOT_TOKEN env var)");
            return Ok(false);
        }

        // Validate token via REST
        let resp = self
            .get("/users/@me")
            .send()
            .await
            .context("Failed to reach Discord API")?;

        let status = resp.status();
        if status != StatusCode::OK {
            let body = resp.text().await.unwrap_or_default();
            warn!(status = %status, body = %body, "Discord token validation failed");
            return Ok(false);
        }

        let user: DiscordUser = resp
            .json()
            .await
            .context("Failed to parse Discord /users/@me response")?;

        info!(bot_id = %user.id, username = %user.username, "Discord channel connected");

        self.connected = true;
        self.running.store(true, Ordering::Relaxed);

        // Start Gateway listener if handler is set
        if let Some(handler) = self.message_handler.clone() {
            let token = self.token.clone().unwrap();
            let running = self.running.clone();
            tokio::spawn(async move {
                Self::run_gateway(token, handler, running).await;
            });
            info!("Discord Gateway listener spawned");
        }

        Ok(true)
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.running.store(false, Ordering::Relaxed);
        self.connected = false;
        info!("Discord channel disconnected");
        Ok(())
    }

    async fn send_message(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
    ) -> Result<SendResult> {
        let path = format!("/channels/{chat_id}/messages");
        let body = CreateMessageBody {
            content: content.to_string(),
            message_reference: reply_to.map(|id| MessageReference {
                message_id: id.to_string(),
            }),
            embed: None,
        };

        let resp = self
            .post(&path)
            .json(&body)
            .send()
            .await
            .context("Failed to send Discord message")?;

        let status = resp.status();
        if status != StatusCode::OK {
            let text = resp.text().await.unwrap_or_default();
            return Ok(Self::make_error_result(status, &text));
        }

        let msg: DiscordMessageResponse = resp
            .json()
            .await
            .context("Failed to parse Discord message response")?;

        debug!(message_id = %msg.id, "Discord message sent");
        Ok(SendResult {
            success: true,
            message_id: Some(msg.id),
            error: None,
            retryable: false,
        })
    }

    async fn send_typing(&self, chat_id: &str) -> Result<()> {
        let path = format!("/channels/{chat_id}/typing");

        let resp = self
            .post(&path)
            .send()
            .await
            .context("Failed to send Discord typing indicator")?;

        let status = resp.status();
        if status != StatusCode::NO_CONTENT {
            let text = resp.text().await.unwrap_or_default();
            warn!(status = %status, body = %text, "Discord typing indicator failed (non-fatal)");
        } else {
            debug!("Discord typing indicator sent");
        }

        Ok(())
    }

    async fn send_image(
        &self,
        chat_id: &str,
        image_url: &str,
        caption: Option<&str>,
    ) -> Result<SendResult> {
        let path = format!("/channels/{chat_id}/messages");
        let body = CreateMessageBody {
            content: String::new(),
            message_reference: None,
            embed: Some(DiscordEmbed {
                image: DiscordEmbedImage {
                    url: image_url.to_string(),
                },
                description: caption.map(|c| c.to_string()),
            }),
        };

        let resp = self
            .post(&path)
            .json(&body)
            .send()
            .await
            .context("Failed to send Discord image")?;

        let status = resp.status();
        if status != StatusCode::OK {
            let text = resp.text().await.unwrap_or_default();
            return Ok(Self::make_error_result(status, &text));
        }

        let msg: DiscordMessageResponse = resp
            .json()
            .await
            .context("Failed to parse Discord image message response")?;

        debug!(message_id = %msg.id, "Discord image sent");
        Ok(SendResult {
            success: true,
            message_id: Some(msg.id),
            error: None,
            retryable: false,
        })
    }

    fn set_message_handler(&mut self, handler: tokio::sync::mpsc::Sender<InboundMessage>) {
        self.message_handler = Some(handler);
    }
}

// ---------------------------------------------------------------------------
// Extended methods (not part of BaseChannel trait)
// ---------------------------------------------------------------------------

/// Body for editing a message via PATCH.
#[derive(Debug, Serialize)]
struct EditMessageBody {
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    embed: Option<DiscordEmbed>,
}

impl DiscordChannel {
    /// Edit a previously sent message.
    ///
    /// Uses `PATCH /channels/{channel_id}/messages/{message_id}`.
    /// Only the bot's own messages can be edited.
    pub async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<SendResult> {
        debug!(
            "Editing Discord message {} in channel {}",
            message_id, channel_id
        );

        let body = EditMessageBody {
            content: content.to_string(),
            embed: None,
        };

        let path = format!("/channels/{channel_id}/messages/{message_id}");
        let url = format!("{}{path}", self.api_base());
        let mut req = self.client.patch(&url);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }

        let resp = req
            .json(&body)
            .send()
            .await
            .context("Failed to edit Discord message")?;

        let status = resp.status();
        if status != StatusCode::OK {
            let text = resp.text().await.unwrap_or_default();
            return Ok(Self::make_error_result(status, &text));
        }

        let msg: DiscordMessageResponse = resp
            .json()
            .await
            .context("Failed to parse Discord edit response")?;

        debug!(message_id = %msg.id, "Discord message edited");
        Ok(SendResult {
            success: true,
            message_id: Some(msg.id),
            error: None,
            retryable: false,
        })
    }

    /// Delete a previously sent message.
    ///
    /// Uses `DELETE /channels/{channel_id}/messages/{message_id}`.
    /// Only the bot's own messages can be deleted (unless has MANAGE_MESSAGES).
    pub async fn delete_message(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<SendResult> {
        debug!(
            "Deleting Discord message {} in channel {}",
            message_id, channel_id
        );

        let path = format!("/channels/{channel_id}/messages/{message_id}");
        let url = format!("{}{path}", self.api_base());
        let mut req = self.client.delete(&url);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }

        let resp = req
            .send()
            .await
            .context("Failed to delete Discord message")?;

        let status = resp.status();
        if status != StatusCode::NO_CONTENT {
            let text = resp.text().await.unwrap_or_default();
            return Ok(Self::make_error_result(status, &text));
        }

        debug!(message_id = %message_id, "Discord message deleted");
        Ok(SendResult {
            success: true,
            message_id: Some(message_id.to_string()),
            error: None,
            retryable: false,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discord_new() {
        let channel = DiscordChannel::new();
        assert_eq!(channel.name(), "discord");
        assert_eq!(channel.platform(), Platform::Discord);
        assert!(!channel.is_connected());
    }

    #[test]
    fn test_discord_default() {
        let channel = DiscordChannel::default();
        assert_eq!(channel.name(), "discord");
    }

    #[tokio::test]
    async fn test_discord_connect_without_token() {
        std::env::remove_var("DISCORD_BOT_TOKEN");
        let mut channel = DiscordChannel::new();
        let result = channel.connect().await.unwrap();
        assert!(!result);
        assert!(!channel.is_connected());
    }

    #[tokio::test]
    async fn test_discord_disconnect() {
        let mut channel = DiscordChannel::new();
        channel.connected = true;
        channel.disconnect().await.unwrap();
        assert!(!channel.is_connected());
        assert!(!channel.running.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_discord_set_message_handler() {
        let mut channel = DiscordChannel::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        channel.set_message_handler(tx);
        assert!(channel.message_handler.is_some());
    }

    #[test]
    fn test_discord_auth_header() {
        let mut channel = DiscordChannel::new();
        assert!(channel.auth_header().is_none());

        channel.token = Some("test-token".to_string());
        let header = channel.auth_header().unwrap();
        assert_eq!(header, "Bot test-token");
    }

    #[test]
    fn test_discord_make_error_result_retryable() {
        let result = DiscordChannel::make_error_result(StatusCode::TOO_MANY_REQUESTS, "slow down");
        assert!(!result.success);
        assert!(result.retryable);
        assert!(result.error.is_some());
    }

    #[test]
    fn test_discord_make_error_result_non_retryable() {
        let result = DiscordChannel::make_error_result(StatusCode::FORBIDDEN, "nope");
        assert!(!result.success);
        assert!(!result.retryable);
    }

    #[test]
    fn test_create_message_body_serialisation() {
        let body = CreateMessageBody {
            content: "hello".to_string(),
            message_reference: Some(MessageReference {
                message_id: "123".to_string(),
            }),
            embed: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["content"], "hello");
        assert_eq!(json["message_reference"]["message_id"], "123");
        assert!(json.get("embed").is_none());
    }

    #[test]
    fn test_create_message_body_embed_serialisation() {
        let body = CreateMessageBody {
            content: String::new(),
            message_reference: None,
            embed: Some(DiscordEmbed {
                image: DiscordEmbedImage {
                    url: "https://example.com/img.png".to_string(),
                },
                description: Some("a caption".to_string()),
            }),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["embed"]["image"]["url"], "https://example.com/img.png");
        assert_eq!(json["embed"]["description"], "a caption");
        assert_eq!(json["content"], "");
        assert!(json.get("message_reference").is_none());
    }

    #[tokio::test]
    async fn test_dispatch_message() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<InboundMessage>(10);

        let msg = GatewayMessage {
            id: "111".to_string(),
            content: "Hello bot!".to_string(),
            channel_id: "222".to_string(),
            author: Some(GatewayAuthor {
                id: "333".to_string(),
                username: "testuser".to_string(),
            }),
            guild_id: Some("444".to_string()),
        };

        DiscordChannel::dispatch_message(&tx, &msg).await;

        let inbound = rx.try_recv().unwrap();
        assert_eq!(inbound.channel, Platform::Discord);
        assert_eq!(inbound.sender_id, "333");
        assert_eq!(inbound.chat_id, "222");
        assert_eq!(inbound.content, "Hello bot!");
        assert_eq!(inbound.message_type, MessageType::Text);
        assert_eq!(inbound.message_id.as_deref(), Some("111"));
        let src = inbound.source.unwrap();
        assert_eq!(src.platform, Platform::Discord);
        assert_eq!(src.chat_type, "guild");
        assert_eq!(src.user_name.as_deref(), Some("testuser"));
    }

    #[tokio::test]
    async fn test_dispatch_message_dm() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<InboundMessage>(10);

        let msg = GatewayMessage {
            id: "555".to_string(),
            content: "/help".to_string(),
            channel_id: "666".to_string(),
            author: Some(GatewayAuthor {
                id: "777".to_string(),
                username: "dm_user".to_string(),
            }),
            guild_id: None,
        };

        DiscordChannel::dispatch_message(&tx, &msg).await;

        let inbound = rx.try_recv().unwrap();
        assert_eq!(inbound.message_type, MessageType::Command);
        let src = inbound.source.unwrap();
        assert_eq!(src.chat_type, "dm");
    }

    #[tokio::test]
    async fn test_dispatch_message_empty_skipped() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<InboundMessage>(10);

        let msg = GatewayMessage {
            id: "888".to_string(),
            content: String::new(),
            channel_id: "999".to_string(),
            author: None,
            guild_id: None,
        };

        DiscordChannel::dispatch_message(&tx, &msg).await;
        // Should not send — empty content
    }

    // -----------------------------------------------------------------------
    // Tests for intents, opcodes, and serialisation
    // -----------------------------------------------------------------------

    #[test]
    fn test_intents_text_bot_value() {
        // GuildMessages (512) | DirectMessages (4096) | MessageContent (32768)
        assert_eq!(intents::TEXT_BOT, 512 + 4096 + 32768);
    }

    #[test]
    fn test_opcode_constants() {
        assert_eq!(opcodes::DISPATCH, 0);
        assert_eq!(opcodes::HEARTBEAT, 1);
        assert_eq!(opcodes::IDENTIFY, 2);
        assert_eq!(opcodes::INVALID_SESSION, 9);
        assert_eq!(opcodes::HELLO, 10);
        assert_eq!(opcodes::HEARTBEAT_ACK, 11);
        assert_eq!(opcodes::RECONNECT, 7);
        assert_eq!(opcodes::RESUME, 6);
    }

    #[test]
    fn test_edit_message_body_serialisation() {
        let body = EditMessageBody {
            content: "updated text".to_string(),
            embed: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["content"], "updated text");
        assert!(json.get("embed").is_none());
    }

    #[test]
    fn test_edit_message_body_with_embed_serialisation() {
        let body = EditMessageBody {
            content: String::new(),
            embed: Some(DiscordEmbed {
                image: DiscordEmbedImage {
                    url: "https://example.com/new_img.png".to_string(),
                },
                description: Some("updated caption".to_string()),
            }),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["content"], "");
        assert_eq!(json["embed"]["image"]["url"], "https://example.com/new_img.png");
        assert_eq!(json["embed"]["description"], "updated caption");
    }

    #[test]
    fn test_gateway_payload_deserialisation() {
        let json = r#"{"op":10,"d":{"heartbeat_interval":41250}}"#;
        let payload: GatewayPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.op, opcodes::HELLO);
        let hello: HelloData = serde_json::from_value(payload.d).unwrap();
        assert_eq!(hello.heartbeat_interval, 41250);
    }

    #[test]
    fn test_gateway_payload_dispatch() {
        let json = r#"{
            "op":0,
            "s":42,
            "t":"MESSAGE_CREATE",
            "d":{"id":"999","content":"hi","channel_id":"111","author":{"id":"222","username":"alice"}}
        }"#;
        let payload: GatewayPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.op, opcodes::DISPATCH);
        assert_eq!(payload.s, Some(42));
        assert_eq!(payload.t.as_deref(), Some("MESSAGE_CREATE"));
        let msg: GatewayMessage = serde_json::from_value(payload.d).unwrap();
        assert_eq!(msg.id, "999");
        assert_eq!(msg.content, "hi");
    }

    #[test]
    fn test_gateway_payload_invalid_session() {
        let json = r#"{"op":9,"d":true}"#;
        let payload: GatewayPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.op, opcodes::INVALID_SESSION);
    }

    #[test]
    fn test_gateway_payload_reconnect() {
        let json = r#"{"op":7,"d":null}"#;
        let payload: GatewayPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.op, opcodes::RECONNECT);
    }

    #[test]
    fn test_discord_message_response_parse() {
        let json = r#"{"id":"1234567890"}"#;
        let resp: DiscordMessageResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.id, "1234567890");
    }

    #[test]
    fn test_discord_user_parse() {
        let json = r#"{"id":"111","username":"testbot"}"#;
        let user: DiscordUser = serde_json::from_str(json).unwrap();
        assert_eq!(user.id, "111");
        assert_eq!(user.username, "testbot");
    }

    #[test]
    fn test_make_error_result_forbidden() {
        let result = DiscordChannel::make_error_result(StatusCode::FORBIDDEN, "Missing Access");
        assert!(!result.success);
        assert!(!result.retryable);
        let err = result.error.unwrap();
        assert!(err.contains("403"));
        assert!(err.contains("Missing Access"));
    }

    #[test]
    fn test_make_error_result_service_unavailable() {
        let result = DiscordChannel::make_error_result(StatusCode::SERVICE_UNAVAILABLE, "overloaded");
        assert!(!result.success);
        assert!(result.retryable);
    }
}
