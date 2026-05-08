//! Feishu (Lark) channel adapter.
//!
//! Implements the BaseChannel trait for Feishu's open platform API.
//! Supports two connection modes:
//! - **WebSocket**: Persistent outbound connection via `wss://` (recommended)
//! - **Webhook**: HTTP callback endpoint for receiving events
//!
//! Both modes use the same REST API for sending messages.
//!
//! ## Authentication
//!
//! Uses tenant_access_token (app_id + app_secret) for API calls.
//! Tokens are cached and auto-refreshed before expiry.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use kestrel_bus::events::InboundMessage;
use kestrel_core::{MediaAttachment, MessageType, Platform, SessionSource};

use crate::base::{BaseChannel, SendResult};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const FEISHU_BASE_URL: &str = "https://open.feishu.cn/open-apis";
const FEISHU_WS_URL: &str = "wss://open.feishu.cn/open-apis/callback/ws/event";
const TOKEN_REFRESH_MARGIN_SECS: u64 = 300; // refresh 5 min before expiry
const WS_RECONNECT_DELAY_SECS: u64 = 5;

// ---------------------------------------------------------------------------
// Feishu API response types
// ---------------------------------------------------------------------------

/// Top-level wrapper for Feishu API responses.
#[derive(Debug, Deserialize)]
struct FeishuResponse<T> {
    code: i64,
    #[allow(dead_code)]
    msg: Option<String>,
    #[serde(default)]
    data: Option<T>,
}

/// Token response from `auth/v3/tenant_access_token/internal`.
#[derive(Debug, Deserialize, Default)]
struct TokenData {
    tenant_access_token: String,
    expire: u64,
}

// ---------------------------------------------------------------------------
// Webhook event types (public — used by the API server webhook route)
// ---------------------------------------------------------------------------

/// Top-level envelope for Feishu webhook callbacks.
#[derive(Debug, Deserialize)]
pub struct WebhookEvent {
    /// Schema version (e.g. "2.0").
    #[serde(default)]
    pub schema: Option<String>,
    /// Header containing event metadata.
    #[serde(default)]
    pub header: Option<WebhookHeader>,
    /// Event payload (varies by type).
    #[serde(default)]
    pub event: Option<serde_json::Value>,
    /// URL verification challenge.
    #[serde(default)]
    pub challenge: Option<String>,
    /// Token for verification.
    #[serde(default)]
    pub token: Option<String>,
    /// Event type string.
    #[serde(rename = "type")]
    #[serde(default)]
    pub event_type: Option<String>,
}

/// Header in a Feishu webhook event.
#[derive(Debug, Deserialize)]
pub struct WebhookHeader {
    #[serde(default)]
    pub event_id: Option<String>,
    #[serde(default)]
    pub event_type: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
}

/// Parsed message event from Feishu.
#[derive(Debug, Deserialize)]
pub struct MessageEvent {
    #[serde(default)]
    pub message: Option<MessageData>,
    #[serde(default)]
    pub sender: Option<SenderData>,
}

/// Message data in a Feishu event.
#[derive(Debug, Deserialize)]
pub struct MessageData {
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub chat_id: Option<String>,
    #[serde(default)]
    pub chat_type: Option<String>,
    #[serde(default)]
    pub message_type: Option<String>,
    /// Content is a JSON-encoded string (e.g. `"{\"text\":\"hello\"}"`).
    #[serde(default)]
    pub content: Option<String>,
    /// Thread ID for threaded messages in groups.
    #[serde(default)]
    pub root_id: Option<String>,
}

/// Sender data in a Feishu event.
#[derive(Debug, Deserialize)]
pub struct SenderData {
    #[serde(default)]
    pub sender_id: Option<SenderId>,
    #[serde(default)]
    pub sender_type: Option<String>,
}

/// Sender identifiers.
#[derive(Debug, Deserialize)]
pub struct SenderId {
    #[serde(default)]
    pub open_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub union_id: Option<String>,
}

/// Result of parsing a webhook request.
#[derive(Debug)]
pub enum WebhookResult {
    /// URL verification challenge — respond with this JSON body.
    Challenge(String),
    /// One or more inbound messages extracted from the event.
    Messages(Vec<InboundMessage>),
    /// Ignored event (not a message or unsupported type).
    Ignored,
}

/// Parse a Feishu webhook POST body.
///
/// Handles two cases:
/// - **URL verification**: Feishu sends `{"challenge": "...", "token": "..."}`
///   during initial setup; respond with the same challenge string.
/// - **Event callback**: Extracts message content and returns InboundMessage(s).
pub fn parse_webhook(body: &[u8]) -> Result<WebhookResult> {
    let event: WebhookEvent =
        serde_json::from_slice(body).context("invalid Feishu webhook JSON")?;

    // URL verification challenge.
    if let Some(challenge) = &event.challenge {
        let response = serde_json::json!({
            "challenge": challenge,
            "token": event.token.clone().unwrap_or_default()
        });
        info!("Feishu URL verification challenge received");
        return Ok(WebhookResult::Challenge(response.to_string()));
    }

    // Event callback — try to extract message event.
    let header = match &event.header {
        Some(h) => h,
        None => return Ok(WebhookResult::Ignored),
    };

    let event_type = header.event_type.as_deref().unwrap_or("");
    if !event_type.starts_with("im.message.receive_v") {
        debug!("Ignoring Feishu event type: {event_type}");
        return Ok(WebhookResult::Ignored);
    }

    let event_json = match &event.event {
        Some(v) => v,
        None => return Ok(WebhookResult::Ignored),
    };

    let msg_event: MessageEvent = serde_json::from_value(event_json.clone())
        .context("failed to parse Feishu message event")?;

    let message = match msg_event.message {
        Some(m) => m,
        None => return Ok(WebhookResult::Ignored),
    };

    let chat_id = match &message.chat_id {
        Some(id) if !id.is_empty() => id.clone(),
        _ => return Ok(WebhookResult::Ignored),
    };

    let message_id = message.message_id.clone();
    let msg_type = message.message_type.as_deref().unwrap_or("text");
    let root_id = message.root_id.clone();

    let sender_id = msg_event
        .sender
        .as_ref()
        .and_then(|s| s.sender_id.as_ref())
        .and_then(|id| id.open_id.clone().or(id.user_id.clone()))
        .unwrap_or_default();

    let chat_type_str = message.chat_type.as_deref().unwrap_or("p2p");
    let chat_type = match chat_type_str {
        "p2p" => "dm",
        "group" => "group",
        other => other,
    };

    // Parse content based on message type.
    let (content, message_type, media) = match msg_type {
        "text" => {
            let text = parse_text_content(message.content.as_deref());
            (text, MessageType::Text, vec![])
        }
        "image" => {
            let (text, media) = parse_image_content(message.content.as_deref(), chat_id.clone());
            (text, MessageType::Photo, media)
        }
        "post" => {
            let text = parse_post_content(message.content.as_deref());
            (text, MessageType::Text, vec![])
        }
        other => {
            debug!("Unsupported Feishu message type: {other}");
            return Ok(WebhookResult::Ignored);
        }
    };

    if content.trim().is_empty() {
        return Ok(WebhookResult::Ignored);
    }

    let mut metadata = HashMap::new();
    if let Some(ref mid) = message_id {
        metadata.insert(
            "message_id".to_string(),
            serde_json::Value::String(mid.clone()),
        );
    }

    let inbound = InboundMessage {
        channel: Platform::Feishu,
        sender_id: sender_id.clone(),
        chat_id: chat_id.clone(),
        content,
        media,
        metadata,
        source: Some(SessionSource {
            platform: Platform::Feishu,
            chat_id: chat_id.clone(),
            chat_name: None,
            chat_type: chat_type.to_string(),
            user_id: Some(sender_id),
            user_name: None,
            thread_id: root_id,
            chat_topic: None,
        }),
        message_type,
        message_id,
        trace_id: None,
        reply_to: None,
        timestamp: chrono::Local::now(),
    };

    Ok(WebhookResult::Messages(vec![inbound]))
}

/// Parse text content from a Feishu message.
///
/// Feishu sends `content` as a JSON-encoded string: `{"text":"hello"}`.
fn parse_text_content(content: Option<&str>) -> String {
    let raw = match content {
        Some(c) => c,
        None => return String::new(),
    };

    #[derive(Deserialize)]
    struct TextContent {
        #[serde(default)]
        text: Option<String>,
    }

    serde_json::from_str::<TextContent>(raw)
        .ok()
        .and_then(|c| c.text)
        .unwrap_or_else(|| raw.to_string())
}

/// Parse rich text (post) content from a Feishu message.
///
/// Posts contain an array of content blocks with text runs.
/// We flatten them to plain text.
fn parse_post_content(content: Option<&str>) -> String {
    let raw = match content {
        Some(c) => c,
        None => return String::new(),
    };

    #[derive(Deserialize)]
    struct PostContent {
        #[serde(default)]
        content: Vec<Vec<PostElement>>,
    }

    #[derive(Deserialize)]
    #[serde(tag = "tag")]
    enum PostElement {
        #[serde(rename = "text")]
        Text {
            #[serde(default)]
            text: Option<String>,
        },
        #[serde(rename = "a")]
        Link {
            #[serde(default)]
            text: Option<String>,
            #[serde(default)]
            href: Option<String>,
        },
        #[serde(other)]
        Other,
    }

    let mut lang_posts: HashMap<String, PostContent> = match serde_json::from_str(raw) {
        Ok(p) => p,
        Err(_) => return raw.to_string(),
    };

    // Prefer zh_cn, then en_us, then first available.
    let post = lang_posts
        .remove("zh_cn")
        .or_else(|| lang_posts.remove("en_us"))
        .or_else(|| lang_posts.into_values().next())
        .unwrap_or(PostContent { content: vec![] });

    let mut result = String::new();
    for line in &post.content {
        if !result.is_empty() {
            result.push('\n');
        }
        for element in line {
            match element {
                PostElement::Text { text: Some(t) } => result.push_str(t),
                PostElement::Link {
                    text: Some(t),
                    href: Some(h),
                } => {
                    result.push_str(t);
                    result.push_str(" (");
                    result.push_str(h);
                    result.push(')');
                }
                PostElement::Link { text: Some(t), .. } => result.push_str(t),
                _ => {}
            }
        }
    }
    result
}

/// Parse image content from a Feishu message.
fn parse_image_content(content: Option<&str>, _chat_id: String) -> (String, Vec<MediaAttachment>) {
    let raw = match content {
        Some(c) => c,
        None => return (String::new(), vec![]),
    };

    #[derive(Deserialize)]
    struct ImageContent {
        #[serde(default)]
        image_key: Option<String>,
    }

    let parsed: ImageContent = match serde_json::from_str(raw) {
        Ok(p) => p,
        Err(_) => return (String::new(), vec![]),
    };

    let desc = parsed
        .image_key
        .as_deref()
        .map(|k| format!("[image: {k}]"))
        .unwrap_or_default();

    (desc, vec![])
}

// ---------------------------------------------------------------------------
// FeishuChannel
// ---------------------------------------------------------------------------

/// Feishu channel adapter implementing BaseChannel.
///
/// Handles:
/// - Tenant access token management (auto-refresh)
/// - Sending text and image messages via Feishu REST API
/// - Webhook event parsing (via `parse_webhook`)
pub struct FeishuChannel {
    app_id: String,
    app_secret: String,
    #[allow(dead_code)]
    proxy: Option<String>,
    connection_mode: String,
    message_handler: Option<tokio::sync::mpsc::Sender<InboundMessage>>,
    running: Arc<AtomicBool>,
    client: reqwest::Client,
    access_token: Arc<Mutex<Option<String>>>,
    token_expires_at: Arc<Mutex<Instant>>,
}

impl Default for FeishuChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl FeishuChannel {
    /// Build a reqwest client with optional proxy support.
    fn build_client(proxy_config: Option<&str>) -> reqwest::Client {
        let proxy_url = proxy_config
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| {
                std::env::var("HTTPS_PROXY")
                    .or_else(|_| std::env::var("https_proxy"))
                    .or_else(|_| std::env::var("HTTP_PROXY"))
                    .or_else(|_| std::env::var("http_proxy"))
                    .or_else(|_| std::env::var("ALL_PROXY"))
                    .or_else(|_| std::env::var("all_proxy"))
                    .ok()
            });

        let dns = kestrel_core::dns::build_dns_resolver();

        match proxy_url {
            Some(ref url) if url.starts_with("socks5") => {
                info!("Feishu HTTP client using SOCKS5 proxy: {}", url);
                let proxy =
                    reqwest::Proxy::all(url).expect("Failed to create SOCKS5 proxy from config");
                reqwest::Client::builder()
                    .dns_resolver(dns)
                    .proxy(proxy)
                    .build()
                    .expect("Failed to build HTTP client with SOCKS5 proxy")
            }
            Some(ref url) if url.starts_with("http") => {
                info!("Feishu HTTP client using HTTP proxy: {}", url);
                let http_proxy =
                    reqwest::Proxy::http(url).expect("Failed to create HTTP proxy from config");
                let https_proxy =
                    reqwest::Proxy::https(url).expect("Failed to create HTTPS proxy from config");
                reqwest::Client::builder()
                    .dns_resolver(dns)
                    .proxy(http_proxy)
                    .proxy(https_proxy)
                    .build()
                    .expect("Failed to build HTTP client with HTTP proxy")
            }
            Some(ref url) => {
                info!(
                    "Feishu HTTP client: unsupported proxy scheme in '{}', falling back to direct",
                    url
                );
                reqwest::Client::builder()
                    .dns_resolver(dns)
                    .build()
                    .expect("Failed to build HTTP client")
            }
            None => {
                info!("Feishu HTTP client: no proxy configured (direct connection)");
                reqwest::Client::builder()
                    .dns_resolver(dns)
                    .build()
                    .expect("Failed to build HTTP client")
            }
        }
    }

    /// Create from environment variables `FEISHU_APP_ID` / `FEISHU_APP_SECRET`.
    pub fn new() -> Self {
        let app_id = std::env::var("FEISHU_APP_ID").unwrap_or_default();
        let app_secret = std::env::var("FEISHU_APP_SECRET").unwrap_or_default();
        let connection_mode =
            std::env::var("FEISHU_CONNECTION_MODE").unwrap_or_else(|_| "webhook".to_string());
        Self {
            app_id,
            app_secret,
            proxy: None,
            connection_mode,
            message_handler: None,
            running: Arc::new(AtomicBool::new(false)),
            client: Self::build_client(None),
            access_token: Arc::new(Mutex::new(None)),
            token_expires_at: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// Create from config struct.
    pub fn new_with_config(config: &kestrel_config::schema::FeishuConfig) -> Self {
        let app_id = config
            .app_id
            .clone()
            .or_else(|| std::env::var("FEISHU_APP_ID").ok())
            .unwrap_or_default();
        let app_secret = config
            .app_secret
            .clone()
            .or_else(|| std::env::var("FEISHU_APP_SECRET").ok())
            .unwrap_or_default();
        let proxy = config.proxy.clone();
        let connection_mode = config
            .connection_mode
            .clone()
            .or_else(|| std::env::var("FEISHU_CONNECTION_MODE").ok())
            .unwrap_or_else(|| "webhook".to_string());
        let client = Self::build_client(proxy.as_deref());
        Self {
            app_id,
            app_secret,
            proxy,
            connection_mode,
            message_handler: None,
            running: Arc::new(AtomicBool::new(false)),
            client,
            access_token: Arc::new(Mutex::new(None)),
            token_expires_at: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// Get a valid tenant_access_token, refreshing if needed.
    async fn get_access_token(&self) -> Result<String> {
        {
            let token = self.access_token.lock();
            let expires = self.token_expires_at.lock();
            if let Some(ref t) = *token {
                if *expires > Instant::now() {
                    return Ok(t.clone());
                }
            }
        }

        debug!("Refreshing Feishu tenant_access_token");
        let url = format!("{FEISHU_BASE_URL}/auth/v3/tenant_access_token/internal");
        let body = serde_json::json!({
            "app_id": self.app_id,
            "app_secret": self.app_secret,
        });

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to request Feishu tenant_access_token")?;

        let feishu_resp: FeishuResponse<TokenData> = resp
            .json()
            .await
            .context("Failed to parse Feishu token response")?;

        if feishu_resp.code != 0 {
            anyhow::bail!(
                "Feishu token request failed: code={}, msg={:?}",
                feishu_resp.code,
                feishu_resp.msg
            );
        }

        let data = feishu_resp
            .data
            .context("Feishu token response missing data")?;

        let expire_secs = data.expire.saturating_sub(TOKEN_REFRESH_MARGIN_SECS);
        let expires_at = Instant::now() + Duration::from_secs(expire_secs);

        *self.access_token.lock() = Some(data.tenant_access_token.clone());
        *self.token_expires_at.lock() = expires_at;

        info!(
            "Feishu tenant_access_token refreshed, expires in {}s",
            expire_secs
        );
        Ok(data.tenant_access_token)
    }

    /// Send a text message via Feishu API.
    async fn send_text_message(
        &self,
        chat_id: &str,
        text: &str,
        reply_to: Option<&str>,
    ) -> Result<SendResult> {
        let token = self.get_access_token().await?;

        let url = format!("{FEISHU_BASE_URL}/im/v1/messages?receive_id_type=chat_id");
        let content = serde_json::json!({
            "text": text
        })
        .to_string();

        let mut body = serde_json::json!({
            "receive_id": chat_id,
            "msg_type": "text",
            "content": content,
        });

        if let Some(reply_msg_id) = reply_to {
            body["reply_in_thread"] = serde_json::Value::Bool(true);
            // Feishu doesn't have a direct reply_to field in send API;
            // instead, we use the reply API endpoint.
            return self
                .reply_message(&token, chat_id, reply_msg_id, &content, "text")
                .await;
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send Feishu message")?;

        self.handle_send_response(resp).await
    }

    /// Reply to a specific message.
    async fn reply_message(
        &self,
        token: &str,
        _chat_id: &str,
        message_id: &str,
        content: &str,
        msg_type: &str,
    ) -> Result<SendResult> {
        let url = format!("{FEISHU_BASE_URL}/im/v1/messages/{message_id}/reply");
        let body = serde_json::json!({
            "msg_type": msg_type,
            "content": content,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to reply to Feishu message")?;

        self.handle_send_response(resp).await
    }

    /// Handle the response from a send/reply API call.
    async fn handle_send_response(&self, resp: reqwest::Response) -> Result<SendResult> {
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse Feishu send response")?;

        let code = body.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);

        if code == 0 {
            let message_id = body
                .get("data")
                .and_then(|d| d.get("message_id"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string());

            Ok(SendResult {
                success: true,
                message_id,
                error: None,
                retryable: false,
            })
        } else {
            let msg = body
                .get("msg")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            let retryable = status.is_server_error() || code == 99991400;
            error!("Feishu send failed: code={code}, msg={msg}, status={status}");
            Ok(SendResult {
                success: false,
                message_id: None,
                error: Some(format!("Feishu API error: {msg} (code {code})")),
                retryable,
            })
        }
    }

    /// Spawn the WebSocket event loop in a background task.
    fn spawn_ws_loop(
        &self,
        handler: tokio::sync::mpsc::Sender<InboundMessage>,
        running: Arc<AtomicBool>,
    ) {
        let app_id = self.app_id.clone();
        let app_secret = self.app_secret.clone();

        tokio::spawn(async move {
            run_ws_event_loop(app_id, app_secret, handler, running).await;
        });
    }
}

// ---------------------------------------------------------------------------
// WebSocket event loop
// ---------------------------------------------------------------------------

/// WebSocket frame types from Feishu long-connection protocol.
#[derive(Debug, Deserialize)]
struct WsFrame {
    #[serde(default)]
    cmd: Option<String>,
    #[serde(default)]
    data: Option<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    msg_id: Option<String>,
}

/// Auth request sent to Feishu after WebSocket connection.
#[derive(Debug, Serialize)]
struct WsAuthRequest {
    cmd: String,
    data: WsAuthData,
}

#[derive(Debug, Serialize)]
struct WsAuthData {
    app_id: String,
    app_secret: String,
}

/// Run the WebSocket event loop for Feishu long-connection mode.
///
/// Connects to Feishu's WebSocket endpoint, authenticates with app credentials,
/// and forwards incoming events to the message handler.
async fn run_ws_event_loop(
    app_id: String,
    app_secret: String,
    handler: tokio::sync::mpsc::Sender<InboundMessage>,
    running: Arc<AtomicBool>,
) {
    while running.load(Ordering::Relaxed) {
        if let Err(e) = ws_connect_and_listen(&app_id, &app_secret, &handler, &running).await {
            error!("[feishu:ws] connection error: {e}");
        }

        if !running.load(Ordering::Relaxed) {
            break;
        }

        info!(
            "[feishu:ws] reconnecting in {}s...",
            WS_RECONNECT_DELAY_SECS
        );
        tokio::time::sleep(Duration::from_secs(WS_RECONNECT_DELAY_SECS)).await;
    }

    info!("[feishu:ws] event loop exited");
}

/// Connect to Feishu WebSocket, authenticate, and process incoming frames.
async fn ws_connect_and_listen(
    app_id: &str,
    app_secret: &str,
    handler: &tokio::sync::mpsc::Sender<InboundMessage>,
    running: &AtomicBool,
) -> Result<()> {
    info!("[feishu:ws] connecting to {FEISHU_WS_URL}");
    let (ws_stream, _) = connect_async(FEISHU_WS_URL)
        .await
        .context("Feishu WebSocket connect failed")?;

    info!("[feishu:ws] connected, authenticating");
    let (mut write, mut read) = ws_stream.split();

    // Send authentication frame.
    let auth = WsAuthRequest {
        cmd: "command".to_string(),
        data: WsAuthData {
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
        },
    };
    let auth_json = serde_json::to_string(&auth).context("serialize WS auth")?;
    write
        .send(Message::Text(auth_json.into()))
        .await
        .context("send WS auth")?;

    // Process incoming frames.
    while let Some(msg) = read.next().await {
        if !running.load(Ordering::Relaxed) {
            info!("[feishu:ws] shutting down");
            break;
        }

        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                warn!("[feishu:ws] read error: {e}");
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                if let Err(e) = handle_ws_text_frame(&text, handler).await {
                    debug!("[feishu:ws] frame handling error: {e}");
                }
            }
            Message::Ping(data) => {
                if let Err(e) = write.send(Message::Pong(data)).await {
                    warn!("[feishu:ws] pong error: {e}");
                    break;
                }
            }
            Message::Close(_) => {
                info!("[feishu:ws] server closed connection");
                break;
            }
            _ => {}
        }
    }

    let _ = write.close().await;
    Ok(())
}

/// Handle a text frame from the Feishu WebSocket.
async fn handle_ws_text_frame(
    text: &str,
    handler: &tokio::sync::mpsc::Sender<InboundMessage>,
) -> Result<()> {
    let frame: WsFrame = serde_json::from_str(text).context("parse WS frame")?;

    match frame.cmd.as_deref() {
        Some("command") => {
            // Auth response or config response.
            if let Some(data) = &frame.data {
                let code = data.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
                if code == 0 {
                    info!("[feishu:ws] authentication successful");
                } else {
                    let msg = data
                        .get("msg")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown");
                    error!("[feishu:ws] auth failed: code={code}, msg={msg}");
                }
            }
        }
        Some("ping") => {
            // Server ping — handled at the Message::Ping level.
            debug!("[feishu:ws] received server ping");
        }
        Some("event") | Some("callback") => {
            // Event callback via WebSocket.
            if let Some(event_data) = frame.data {
                if let Ok(event_bytes) = serde_json::to_vec(&event_data) {
                    match parse_webhook(&event_bytes) {
                        Ok(WebhookResult::Messages(msgs)) => {
                            for msg in msgs {
                                if let Err(e) = handler.send(msg).await {
                                    warn!("[feishu:ws] failed to forward message: {e}");
                                }
                            }
                        }
                        Ok(WebhookResult::Challenge(_)) => {
                            debug!("[feishu:ws] challenge event via WS — not applicable");
                        }
                        Ok(WebhookResult::Ignored) => {}
                        Err(e) => {
                            debug!("[feishu:ws] event parse error: {e}");
                        }
                    }
                }
            }
        }
        other => {
            debug!("[feishu:ws] unknown cmd: {:?}", other);
        }
    }

    Ok(())
}

#[async_trait]
impl BaseChannel for FeishuChannel {
    fn name(&self) -> &str {
        "feishu"
    }

    fn platform(&self) -> Platform {
        Platform::Feishu
    }

    fn is_connected(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    async fn connect(&mut self) -> Result<bool> {
        if self.app_id.is_empty() || self.app_secret.is_empty() {
            warn!("Feishu app_id or app_secret not configured");
            return Ok(false);
        }

        let mode = self.connection_mode.to_lowercase();
        if mode != "websocket" && mode != "webhook" {
            warn!(
                "Feishu unsupported connection_mode='{}', expected 'websocket' or 'webhook'",
                self.connection_mode
            );
            return Ok(false);
        }

        // Pre-fetch tenant_access_token to validate credentials.
        match self.get_access_token().await {
            Ok(_) => {
                self.running.store(true, Ordering::Relaxed);

                if mode == "websocket" {
                    if let Some(handler) = self.message_handler.clone() {
                        self.spawn_ws_loop(handler, self.running.clone());
                    }
                    info!(
                        "Feishu channel connected via WebSocket (app_id={})",
                        self.app_id
                    );
                } else {
                    info!(
                        "Feishu channel connected via Webhook (app_id={})",
                        self.app_id
                    );
                }
                Ok(true)
            }
            Err(e) => {
                error!("Feishu connect failed: {e}");
                Ok(false)
            }
        }
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.running.store(false, Ordering::Relaxed);
        info!("Feishu channel disconnected");
        Ok(())
    }

    async fn send_message(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
    ) -> Result<SendResult> {
        self.send_text_message(chat_id, content, reply_to).await
    }

    async fn send_typing(&self, _chat_id: &str, _trace_id: Option<&str>) -> Result<()> {
        // Feishu has no typing indicator API.
        Ok(())
    }

    async fn send_image(
        &self,
        chat_id: &str,
        image_url: &str,
        caption: Option<&str>,
    ) -> Result<SendResult> {
        let token = self.get_access_token().await?;

        let url = format!("{FEISHU_BASE_URL}/im/v1/messages?receive_id_type=chat_id");

        // Send image as a text message with the URL.
        // Feishu's image message requires uploading to Feishu first;
        // fall back to sending URL as text with an image indicator.
        let text = match caption {
            Some(c) => format!("{c}\n{image_url}"),
            None => image_url.to_string(),
        };

        let content = serde_json::json!({
            "text": text
        })
        .to_string();

        let body = serde_json::json!({
            "receive_id": chat_id,
            "msg_type": "text",
            "content": content,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send Feishu image")?;

        self.handle_send_response(resp).await
    }

    async fn edit_message(
        &self,
        _chat_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<SendResult> {
        let token = self.get_access_token().await?;

        let url = format!("{FEISHU_BASE_URL}/im/v1/messages/{message_id}");
        let body = serde_json::json!({
            "content": serde_json::json!({"text": content}).to_string(),
        });

        let resp = self
            .client
            .patch(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to edit Feishu message")?;

        self.handle_send_response(resp).await
    }

    async fn delete_message(&self, _chat_id: &str, message_id: &str) -> Result<bool> {
        let token = self.get_access_token().await?;

        let url = format!("{FEISHU_BASE_URL}/im/v1/messages/{message_id}");
        let resp = self
            .client
            .delete(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .context("Failed to delete Feishu message")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse Feishu delete response")?;

        let code = body.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        Ok(code == 0)
    }

    fn set_message_handler(&mut self, handler: tokio::sync::mpsc::Sender<InboundMessage>) {
        self.message_handler = Some(handler);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_text_content() {
        let content = r#"{"text":"hello world"}"#;
        assert_eq!(parse_text_content(Some(content)), "hello world");
    }

    #[test]
    fn test_parse_text_content_none() {
        assert_eq!(parse_text_content(None), "");
    }

    #[test]
    fn test_parse_webhook_challenge() {
        let body = r#"{"challenge":"test_challenge_123","token":"verification_token"}"#;
        let result = parse_webhook(body.as_bytes()).unwrap();
        match result {
            WebhookResult::Challenge(json_str) => {
                let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
                assert_eq!(v["challenge"], "test_challenge_123");
            }
            _ => panic!("Expected Challenge result"),
        }
    }

    #[test]
    fn test_parse_webhook_ignored_event() {
        let body = r#"{"schema":"2.0","header":{"event_type":"some.other.event"}}"#;
        let result = parse_webhook(body.as_bytes()).unwrap();
        assert!(matches!(result, WebhookResult::Ignored));
    }

    #[test]
    fn test_parse_webhook_message_event() {
        let body = r#"{
            "schema": "2.0",
            "header": {
                "event_id": "evt_123",
                "event_type": "im.message.receive_v1",
                "token": "xxx",
                "app_id": "cli_test"
            },
            "event": {
                "message": {
                    "message_id": "msg_abc",
                    "chat_id": "oc_chat123",
                    "chat_type": "group",
                    "message_type": "text",
                    "content": "{\"text\":\"hello feishu\"}"
                },
                "sender": {
                    "sender_id": {
                        "open_id": "ou_user123",
                        "user_id": "uid456"
                    },
                    "sender_type": "user"
                }
            }
        }"#;

        let result = parse_webhook(body.as_bytes()).unwrap();
        match result {
            WebhookResult::Messages(msgs) => {
                assert_eq!(msgs.len(), 1);
                let msg = &msgs[0];
                assert_eq!(msg.channel, Platform::Feishu);
                assert_eq!(msg.chat_id, "oc_chat123");
                assert_eq!(msg.content, "hello feishu");
                assert_eq!(msg.sender_id, "ou_user123");
                assert_eq!(msg.message_id, Some("msg_abc".to_string()));
                assert_eq!(msg.source.as_ref().unwrap().chat_type, "group");
            }
            _ => panic!("Expected Messages result"),
        }
    }

    #[test]
    fn test_parse_webhook_p2p_chat() {
        let body = r#"{
            "schema": "2.0",
            "header": {
                "event_type": "im.message.receive_v1"
            },
            "event": {
                "message": {
                    "message_id": "msg_dm",
                    "chat_id": "oc_dm123",
                    "chat_type": "p2p",
                    "message_type": "text",
                    "content": "{\"text\":\"dm message\"}"
                },
                "sender": {
                    "sender_id": {"open_id": "ou_sender"}
                }
            }
        }"#;

        let result = parse_webhook(body.as_bytes()).unwrap();
        match result {
            WebhookResult::Messages(msgs) => {
                assert_eq!(msgs[0].source.as_ref().unwrap().chat_type, "dm");
            }
            _ => panic!("Expected Messages result"),
        }
    }

    #[test]
    fn test_parse_webhook_empty_text_ignored() {
        let body = r#"{
            "schema": "2.0",
            "header": {
                "event_type": "im.message.receive_v1"
            },
            "event": {
                "message": {
                    "message_id": "msg_empty",
                    "chat_id": "oc_chat",
                    "chat_type": "p2p",
                    "message_type": "text",
                    "content": "{\"text\":\"  \"}"
                },
                "sender": {
                    "sender_id": {"open_id": "ou_sender"}
                }
            }
        }"#;

        let result = parse_webhook(body.as_bytes()).unwrap();
        assert!(matches!(result, WebhookResult::Ignored));
    }

    #[test]
    fn test_channel_new_default() {
        let ch = FeishuChannel::new();
        assert_eq!(ch.name(), "feishu");
        assert_eq!(ch.platform(), Platform::Feishu);
        assert!(!ch.is_connected());
    }

    #[test]
    fn test_parse_post_content() {
        let content = r#"{
            "zh_cn": {
                "content": [
                    [
                        {"tag": "text", "text": "Hello "},
                        {"tag": "a", "text": "link", "href": "https://example.com"}
                    ],
                    [
                        {"tag": "text", "text": "Line 2"}
                    ]
                ]
            }
        }"#;
        let result = parse_post_content(Some(content));
        assert_eq!(result, "Hello link (https://example.com)\nLine 2");
    }
}
