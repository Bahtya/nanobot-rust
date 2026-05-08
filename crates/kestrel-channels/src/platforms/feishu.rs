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

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use kestrel_bus::events::InboundMessage;
use kestrel_config::schema::FeishuConfig;
use kestrel_core::{MediaAttachment, MessageType, Platform, SessionSource};

use crate::base::{BaseChannel, SendResult};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const FEISHU_BASE_URL: &str = "https://open.feishu.cn/open-apis";
const FEISHU_WS_URL: &str = "wss://open.feishu.cn/open-apis/callback/ws/event";
const TOKEN_REFRESH_MARGIN_SECS: u64 = 300; // refresh 5 min before expiry
const _FEISHU_DEDUP_TTL_SECS: u64 = 86400; // 24 hours
const FEISHU_BATCH_WINDOW_MS: u64 = 600; // 0.6s normal batch window
const FEISHU_BATCH_SPLIT_WINDOW_MS: u64 = 2000; // 2s for near-limit split detection
const FEISHU_SPLIT_THRESHOLD_CHARS: usize = 3800; // ~near Feishu's ~4000 char limit
const WS_RECONNECT_DELAY_SECS: u64 = 5;

// ---------------------------------------------------------------------------
// Feishu message deduplication
// ---------------------------------------------------------------------------

/// Deduplication tracker for Feishu messages.
///
/// Prevents duplicate processing based on:
/// - `message_id` — unique per event
/// - Content fingerprint (`sender_id:text_prefix`) — catches same content
///   delivered through different event types
///
/// Entries are automatically pruned after 24 hours.
pub struct FeishuDedup {
    ttl_secs: u64,
    seen: parking_lot::Mutex<HashMap<String, Instant>>,
    last_prune: parking_lot::Mutex<Instant>,
}

impl FeishuDedup {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            ttl_secs,
            seen: parking_lot::Mutex::new(HashMap::new()),
            last_prune: parking_lot::Mutex::new(Instant::now()),
        }
    }

    /// Check if a key has been seen before. Returns `true` if duplicate.
    ///
    /// Automatically prunes stale entries every ~60 seconds.
    pub fn is_duplicate(&self, key: &str) -> bool {
        let mut seen = self.seen.lock();
        let now = Instant::now();

        if now.duration_since(*self.last_prune.lock()) > Duration::from_secs(60) {
            let ttl = Duration::from_secs(self.ttl_secs);
            seen.retain(|_, t| now.duration_since(*t) < ttl);
            *self.last_prune.lock() = now;
        }

        if seen.contains_key(key) {
            return true;
        }
        seen.insert(key.to_string(), now);
        false
    }
}

// ---------------------------------------------------------------------------
// Feishu message batching
// ---------------------------------------------------------------------------

/// Tracks a pending batch of messages for a single chat.
struct PendingBatch {
    messages: Vec<InboundMessage>,
    timer: Instant,
    extended: bool,
}

/// Batches rapid consecutive Feishu messages before dispatching to the agent.
///
/// When a user sends several messages in quick succession (e.g. typing then
/// sending), or when a client splits a long message into fragments near the
/// platform limit, this batches them into a single merged message.
pub struct FeishuBatcher {
    dedup: Arc<FeishuDedup>,
    pending: parking_lot::Mutex<HashMap<String, PendingBatch>>,
}

impl FeishuBatcher {
    pub fn new(dedup: Arc<FeishuDedup>) -> Self {
        Self {
            dedup,
            pending: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    /// Process an inbound message through dedup and batching.
    ///
    /// Returns:
    /// - `Ok(Some(msg))` — message is ready to dispatch immediately (no batch)
    /// - `Ok(None)` — message was buffered into an existing or new batch
    /// - `Err(msg)` — message is a duplicate, discard it
    #[allow(clippy::result_large_err)]
    pub fn process(
        &self,
        msg: InboundMessage,
    ) -> std::result::Result<Option<InboundMessage>, InboundMessage> {
        let message_id = msg.message_id.as_deref().unwrap_or("");
        let sender_id = &msg.sender_id;

        // message_id dedup
        if !message_id.is_empty() && self.dedup.is_duplicate(message_id) {
            debug!("Feishu dedup: skipping duplicate message_id={}", message_id);
            return Err(msg);
        }

        // Content fingerprint dedup
        let prefix_len = msg.content.len().min(200);
        let content_key = format!("{}:{}", sender_id, &msg.content[..prefix_len]);
        if self.dedup.is_duplicate(&content_key) {
            debug!(
                "Feishu dedup: skipping content duplicate from {}",
                sender_id
            );
            return Err(msg);
        }

        let now = Instant::now();
        let chat_id = msg.chat_id.clone();
        let mut pending = self.pending.lock();

        // Check if there's an existing batch for this chat
        if let Some(batch) = pending.get_mut(&chat_id) {
            // Check if the batch window has expired
            let window = if batch.extended {
                Duration::from_millis(FEISHU_BATCH_SPLIT_WINDOW_MS)
            } else {
                Duration::from_millis(FEISHU_BATCH_WINDOW_MS)
            };

            if now.duration_since(batch.timer) < window {
                // Within window — buffer this message
                let near_limit = msg.content.len() >= FEISHU_SPLIT_THRESHOLD_CHARS;
                if near_limit {
                    batch.extended = true;
                }
                batch.messages.push(msg);
                return Ok(None);
            }
            // Window expired — we should have already flushed, but handle defensively
        }

        // No active batch — start a new one with just this message
        let near_limit = msg.content.len() >= FEISHU_SPLIT_THRESHOLD_CHARS;
        pending.insert(
            chat_id,
            PendingBatch {
                messages: vec![msg],
                timer: now,
                extended: near_limit,
            },
        );
        Ok(None)
    }

    /// Drain all batches whose timer has expired, returning merged messages.
    pub fn drain_ready(&self) -> Vec<InboundMessage> {
        let now = Instant::now();
        let mut pending = self.pending.lock();
        let mut results = Vec::new();

        let expired_keys: Vec<String> = pending
            .iter()
            .filter(|(_, batch)| {
                let window = if batch.extended {
                    Duration::from_millis(FEISHU_BATCH_SPLIT_WINDOW_MS)
                } else {
                    Duration::from_millis(FEISHU_BATCH_WINDOW_MS)
                };
                now.duration_since(batch.timer) >= window
            })
            .map(|(k, _)| k.clone())
            .collect();

        for key in expired_keys {
            if let Some(batch) = pending.remove(&key) {
                if let Some(merged) = merge_batch(batch.messages) {
                    results.push(merged);
                }
            }
        }

        results
    }

    /// Force-drain all pending batches regardless of timer, returning merged messages.
    pub fn force_flush_all(&self) -> Vec<InboundMessage> {
        let mut pending = self.pending.lock();
        let batches: Vec<_> = pending.drain().collect();
        let mut results = Vec::new();
        for (_, batch) in batches {
            if let Some(merged) = merge_batch(batch.messages) {
                results.push(merged);
            }
        }
        results
    }
}

/// Merge a batch of messages into a single `InboundMessage`.
///
/// Uses the first message's `message_id` and `chat_id`.
/// Concatenates content with newlines.
fn merge_batch(messages: Vec<InboundMessage>) -> Option<InboundMessage> {
    if messages.is_empty() {
        return None;
    }
    if messages.len() == 1 {
        return Some(messages.into_iter().next().unwrap());
    }

    let first = &messages[0];
    let content = messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let merged = InboundMessage {
        channel: first.channel.clone(),
        sender_id: first.sender_id.clone(),
        chat_id: first.chat_id.clone(),
        content,
        media: first.media.clone(),
        metadata: first.metadata.clone(),
        source: first.source.clone(),
        message_type: first.message_type.clone(),
        message_id: first.message_id.clone(),
        trace_id: first.trace_id.clone(),
        reply_to: first.reply_to.clone(),
        timestamp: first.timestamp,
    };

    info!(
        "Feishu batcher: merged {} messages for chat {}",
        messages.len(),
        merged.chat_id
    );

    Some(merged)
}

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

/// Reaction response data.
#[derive(Debug, Deserialize, Default)]
struct ReactionData {
    #[serde(default)]
    reaction_id: Option<String>,
}

/// Image upload response data.
#[derive(Debug, Deserialize, Default)]
struct ImageUploadData {
    #[serde(default)]
    image_key: String,
}

/// File upload response data.
#[derive(Debug, Deserialize, Default)]
#[allow(dead_code)]
struct FileUploadData {
    #[serde(default)]
    file_key: String,
}

// ---------------------------------------------------------------------------
// Inbound media content types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ImageContent {
    #[serde(default)]
    image_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FileContent {
    #[serde(default)]
    file_key: Option<String>,
    #[serde(default)]
    file_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AudioContent {
    #[serde(default)]
    file_key: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    duration: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct VideoContent {
    #[serde(default)]
    file_key: Option<String>,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    duration: Option<u64>,
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

/// Encrypted webhook envelope (when `Encrypt Key` is configured in Feishu).
#[derive(Debug, Deserialize)]
struct EncryptedEnvelope {
    #[serde(default)]
    encrypt: Option<String>,
}

/// Admission check result.
#[derive(Debug, PartialEq)]
pub enum Admission {
    /// Message is allowed.
    Allow,
    /// Message is denied with a reason.
    Deny(String),
}

/// Check if a webhook event should be accepted based on FeishuConfig.
///
/// Evaluates:
/// - **Verification token**: if configured, `header.token` must match.
/// - **Group policy**: `open` / `allowlist` / `blacklist` / `disabled`.
/// - **DM allowed users**: if non-empty, sender must be in the list.
/// - **Bot policy**: `none` / `mentions` / `all`.
/// - **Mention-only**: in groups, skip unless the bot is @mentioned.
pub fn check_admission(event: &WebhookEvent, config: &FeishuConfig) -> Admission {
    // URL verification challenges bypass admission.
    if event.challenge.is_some() {
        return Admission::Allow;
    }

    // Verification token check.
    let env_token = std::env::var("FEISHU_VERIFICATION_TOKEN").ok();
    let configured_token = config
        .verification_token
        .as_deref()
        .or(env_token.as_deref());
    if let Some(expected) = configured_token {
        let header_token = event
            .header
            .as_ref()
            .and_then(|h| h.token.as_deref())
            .unwrap_or("");
        if header_token != expected {
            warn!("Feishu webhook: verification token mismatch");
            return Admission::Deny("verification token mismatch".to_string());
        }
    }

    // Extract chat type and sender info from the event payload.
    let event_json = match &event.event {
        Some(v) => v,
        None => return Admission::Allow, // non-message events pass through
    };

    let msg_event: MessageEvent = match serde_json::from_value(event_json.clone()) {
        Ok(m) => m,
        Err(_) => return Admission::Allow,
    };

    let message = match &msg_event.message {
        Some(m) => m,
        None => return Admission::Allow,
    };

    let chat_type = message.chat_type.as_deref().unwrap_or("p2p");
    let chat_id = message.chat_id.as_deref().unwrap_or("");
    let sender_id = msg_event
        .sender
        .as_ref()
        .and_then(|s| s.sender_id.as_ref())
        .and_then(|id| id.open_id.as_deref().or(id.user_id.as_deref()))
        .unwrap_or("");

    let sender_type = msg_event
        .sender
        .as_ref()
        .and_then(|s| s.sender_type.as_deref());

    // Bot policy check.
    if sender_type == Some("app") {
        match config.allow_bots.as_str() {
            "none" => {
                debug!("Feishu admission: bot message denied (allow_bots=none)");
                return Admission::Deny("bot messages not allowed".to_string());
            }
            "mentions" | "all" => {}
            _ => {
                return Admission::Deny("bot messages not allowed".to_string());
            }
        }
    }

    match chat_type {
        "group" => {
            // Group policy check.
            match config.group_policy.as_str() {
                "disabled" => {
                    debug!("Feishu admission: group messages disabled");
                    return Admission::Deny("group messages disabled".to_string());
                }
                "allowlist"
                    if !config.group_allowlist.is_empty()
                        && !config.group_allowlist.iter().any(|g| g == chat_id) =>
                {
                    debug!("Feishu admission: group {chat_id} not in allowlist");
                    return Admission::Deny("group not in allowlist".to_string());
                }
                "blacklist" if config.group_blacklist.iter().any(|g| g == chat_id) => {
                    debug!("Feishu admission: group {chat_id} is blacklisted");
                    return Admission::Deny("group is blacklisted".to_string());
                }
                "open" => {}
                _ => {}
            }

            // Mention-only check for groups.
            if config.mention_only {
                // Feishu includes @mentions in the message content as
                // `<at user_id="...">name</at>` tags. Check if the content
                // contains an <at> tag.
                let has_mention = message
                    .content
                    .as_deref()
                    .map(|c| c.contains("<at "))
                    .unwrap_or(false);
                if !has_mention {
                    debug!("Feishu admission: group message without @mention skipped");
                    return Admission::Deny("mention required in groups".to_string());
                }
            }
        }
        "p2p"
            if !config.allowed_users.is_empty()
                && !config.allowed_users.iter().any(|u| u == sender_id) =>
        {
            debug!("Feishu admission: DM user {sender_id} not in allowed_users");
            return Admission::Deny("user not allowed".to_string());
        }
        _ => {}
    }

    Admission::Allow
}

/// Decrypt an encrypted Feishu webhook payload.
///
/// Feishu uses AES-256-GCM with the key derived from the `Encrypt Key`
/// configured in the Feishu developer console. The encrypted body is
/// `base64(Nonce || Ciphertext || Tag)`.
pub fn decrypt_event(body: &[u8], encrypt_key: &str) -> Result<Vec<u8>> {
    let envelope: EncryptedEnvelope =
        serde_json::from_slice(body).context("failed to parse encrypted envelope")?;

    let encrypted = envelope
        .encrypt
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("missing encrypt field"))?;

    let ciphertext = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encrypted)
        .context("failed to base64-decrypt encrypted payload")?;

    // Feishu uses the encrypt_key directly as a 32-byte AES key (padded or truncated).
    let key_bytes = encrypt_key.as_bytes();
    let mut key = [0u8; 32];
    let copy_len = key_bytes.len().min(32);
    key[..copy_len].copy_from_slice(&key_bytes[..copy_len]);

    // First 12 bytes are the nonce.
    if ciphertext.len() < 12 {
        anyhow::bail!("encrypted payload too short");
    }
    let (nonce_bytes, ct_and_tag) = ciphertext.split_at(12);

    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|e| anyhow::anyhow!("invalid AES key: {e}"))?;
    let nonce = Nonce::from_slice(nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, ct_and_tag)
        .map_err(|e| anyhow::anyhow!("AES decryption failed: {e}"))?;

    Ok(plaintext)
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
    /// Card action triggered by a user interacting with an interactive card.
    CardAction(CardActionEvent),
    /// Ignored event (not a message or unsupported type).
    Ignored,
}

/// Card action event from Feishu interactive card.
#[derive(Debug, Clone)]
pub struct CardActionEvent {
    /// Open ID of the user who triggered the action.
    pub user_open_id: String,
    /// Chat ID where the card was displayed.
    pub chat_id: String,
    /// Message ID of the card message.
    pub message_id: String,
    /// Action key (e.g. "approve_once", "approve_session", "approve_always", "deny").
    pub action_key: String,
    /// Action value payload.
    pub action_value: serde_json::Value,
}

/// Parse a Feishu webhook POST body.
///
/// Handles several cases:
/// - **Encrypted event**: When `encrypt_key` is configured, decrypts the payload first.
/// - **URL verification**: Feishu sends `{"challenge": "...", "token": "..."}`
///   during initial setup; respond with the same challenge string.
/// - **Event callback**: Extracts message content and returns InboundMessage(s).
///
/// If `config` is provided, runs admission checks (verification token,
/// group/DM policy, bot policy).
pub fn parse_webhook(body: &[u8], config: Option<&FeishuConfig>) -> Result<WebhookResult> {
    // Check if the payload is encrypted.
    let raw_body: Vec<u8> = if let Some(cfg) = config {
        let env_key = std::env::var("FEISHU_ENCRYPT_KEY").ok();
        let encrypt_key = cfg.encrypt_key.as_deref().or(env_key.as_deref());
        if let Some(key) = encrypt_key {
            if !key.is_empty() {
                let prelim: serde_json::Value =
                    serde_json::from_slice(body).context("invalid JSON in webhook body")?;
                if prelim.get("encrypt").is_some() {
                    debug!("Feishu webhook: decrypting encrypted payload");
                    decrypt_event(body, key)?
                } else {
                    body.to_vec()
                }
            } else {
                body.to_vec()
            }
        } else {
            body.to_vec()
        }
    } else {
        body.to_vec()
    };

    let event: WebhookEvent =
        serde_json::from_slice(&raw_body).context("invalid Feishu webhook JSON")?;

    // Admission check if config is provided.
    if let Some(cfg) = config {
        match check_admission(&event, cfg) {
            Admission::Allow => {}
            Admission::Deny(reason) => {
                info!("Feishu webhook: admission denied: {reason}");
                return Ok(WebhookResult::Ignored);
            }
        }
    }

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

    if event_type == "card.action.trigger" {
        let event_json = event.event.as_ref().cloned().unwrap_or_default();
        return parse_card_action(&event_json);
    }

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
            let (text, media) = parse_image_content(message.content.as_deref());
            (text, MessageType::Photo, media)
        }
        "file" => {
            let (text, media) = parse_file_content(message.content.as_deref());
            (text, MessageType::Document, media)
        }
        "audio" => {
            let (text, media) = parse_audio_content(message.content.as_deref());
            (text, MessageType::Audio, media)
        }
        "video" => {
            let (text, media) = parse_video_content(message.content.as_deref());
            (text, MessageType::Video, media)
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
fn parse_image_content(content: Option<&str>) -> (String, Vec<MediaAttachment>) {
    let raw = match content {
        Some(c) => c,
        None => return (String::new(), vec![]),
    };

    let parsed: ImageContent = match serde_json::from_str(raw) {
        Ok(p) => p,
        Err(_) => return (String::new(), vec![]),
    };

    if let Some(key) = parsed.image_key {
        let desc = format!("[image: {key}]");
        let media = vec![MediaAttachment {
            url: format!("feishu://image/{key}"),
            mime_type: Some("image/jpeg".to_string()),
            caption: None,
            file_name: None,
            file_size: None,
        }];
        (desc, media)
    } else {
        (String::new(), vec![])
    }
}

/// Parse file content from a Feishu message.
fn parse_file_content(content: Option<&str>) -> (String, Vec<MediaAttachment>) {
    let raw = match content {
        Some(c) => c,
        None => return (String::new(), vec![]),
    };

    let parsed: FileContent = match serde_json::from_str(raw) {
        Ok(p) => p,
        Err(_) => return (String::new(), vec![]),
    };

    if let Some(key) = parsed.file_key {
        let name = parsed.file_name.as_deref().unwrap_or("file");
        let desc = format!("[file: {name}]");
        let media = vec![MediaAttachment {
            url: format!("feishu://file/{key}"),
            mime_type: Some("application/octet-stream".to_string()),
            caption: None,
            file_name: parsed.file_name,
            file_size: None,
        }];
        (desc, media)
    } else {
        (String::new(), vec![])
    }
}

/// Parse audio content from a Feishu message.
fn parse_audio_content(content: Option<&str>) -> (String, Vec<MediaAttachment>) {
    let raw = match content {
        Some(c) => c,
        None => return (String::new(), vec![]),
    };

    let parsed: AudioContent = match serde_json::from_str(raw) {
        Ok(p) => p,
        Err(_) => return (String::new(), vec![]),
    };

    if let Some(key) = parsed.file_key {
        let desc = format!("[audio: {key}]");
        let media = vec![MediaAttachment {
            url: format!("feishu://file/{key}"),
            mime_type: Some("audio/mpeg".to_string()),
            caption: None,
            file_name: None,
            file_size: None,
        }];
        (desc, media)
    } else {
        (String::new(), vec![])
    }
}

/// Parse video content from a Feishu message.
fn parse_video_content(content: Option<&str>) -> (String, Vec<MediaAttachment>) {
    let raw = match content {
        Some(c) => c,
        None => return (String::new(), vec![]),
    };

    let parsed: VideoContent = match serde_json::from_str(raw) {
        Ok(p) => p,
        Err(_) => return (String::new(), vec![]),
    };

    if let Some(key) = parsed.file_key {
        let name = parsed.file_name.as_deref().unwrap_or("video");
        let desc = format!("[video: {name}]");
        let media = vec![MediaAttachment {
            url: format!("feishu://file/{key}"),
            mime_type: Some("video/mp4".to_string()),
            caption: None,
            file_name: parsed.file_name,
            file_size: None,
        }];
        (desc, media)
    } else {
        (String::new(), vec![])
    }
}

/// Parse a card action trigger event.
///
/// Feishu sends card action events when a user clicks a button on an
/// interactive card. The payload structure differs from message events.
fn parse_card_action(event_json: &serde_json::Value) -> Result<WebhookResult> {
    let user_open_id = event_json
        .get("operator")
        .and_then(|o| o.get("open_id"))
        .or_else(|| event_json.get("open_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let chat_id = event_json
        .get("open_chat_id")
        .or_else(|| {
            event_json
                .get("event")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.get("chat_id"))
        })
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let message_id = event_json
        .get("open_message_id")
        .or_else(|| {
            event_json
                .get("event")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.get("message_id"))
        })
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let action = event_json.get("action").unwrap_or(event_json);

    let action_key = action
        .get("key")
        .or_else(|| action.get("tag"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let action_value = action
        .get("value")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    if action_key.is_empty() {
        debug!("Feishu card action missing action key, ignoring");
        return Ok(WebhookResult::Ignored);
    }

    Ok(WebhookResult::CardAction(CardActionEvent {
        user_open_id,
        chat_id,
        message_id,
        action_key,
        action_value,
    }))
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
    /// Last message_id per chat_id (for typing indicator reactions).
    last_message_ids: Arc<Mutex<HashMap<String, String>>>,
    /// Active typing reaction_id per chat_id (for removal).
    typing_reactions: Arc<Mutex<HashMap<String, String>>>,
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
            last_message_ids: Arc::new(Mutex::new(HashMap::new())),
            typing_reactions: Arc::new(Mutex::new(HashMap::new())),
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
            last_message_ids: Arc::new(Mutex::new(HashMap::new())),
            typing_reactions: Arc::new(Mutex::new(HashMap::new())),
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

    /// Add an emoji reaction to a message.
    ///
    /// Returns the reaction_id on success.
    async fn add_reaction(&self, message_id: &str, emoji_key: &str) -> Result<String> {
        let token = self.get_access_token().await?;
        let url = format!("{FEISHU_BASE_URL}/im/v1/messages/{message_id}/reactions");
        let body = serde_json::json!({
            "reaction_type": {
                "emoji_type": {
                    "emoji_key": emoji_key
                }
            }
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to add Feishu reaction")?;

        let feishu_resp: FeishuResponse<ReactionData> = resp
            .json()
            .await
            .context("Failed to parse Feishu reaction response")?;

        if feishu_resp.code != 0 {
            warn!(
                "Feishu add reaction failed: code={}, msg={:?}",
                feishu_resp.code, feishu_resp.msg
            );
            anyhow::bail!("Feishu add reaction failed: code={}", feishu_resp.code);
        }

        let reaction_id = feishu_resp
            .data
            .and_then(|d| d.reaction_id)
            .unwrap_or_default();

        debug!("Added '{emoji_key}' reaction to message {message_id}: reaction_id={reaction_id}");
        Ok(reaction_id)
    }

    /// Remove an emoji reaction from a message.
    async fn delete_reaction(&self, message_id: &str, reaction_id: &str) -> Result<()> {
        let token = self.get_access_token().await?;
        let url = format!("{FEISHU_BASE_URL}/im/v1/messages/{message_id}/reactions/{reaction_id}");

        let resp = self
            .client
            .delete(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .context("Failed to delete Feishu reaction")?;

        let body: serde_json::Value = resp
            .json()
            .await
            .context("Failed to parse Feishu delete reaction response")?;

        let code = body.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            warn!(
                "Feishu delete reaction failed: code={}, msg={:?}",
                code,
                body.get("msg").and_then(|m| m.as_str())
            );
        } else {
            debug!("Removed reaction {reaction_id} from message {message_id}");
        }

        Ok(())
    }

    /// Store the message_id for a chat_id (for typing indicator).
    pub fn track_message_id(&self, chat_id: &str, message_id: &str) {
        self.last_message_ids
            .lock()
            .insert(chat_id.to_string(), message_id.to_string());
    }

    /// Remove the typing indicator (emoji reaction) for a chat.
    pub async fn remove_typing_reaction(&self, chat_id: &str) {
        let reaction_info = self.typing_reactions.lock().remove(chat_id);
        let message_id = self.last_message_ids.lock().get(chat_id).cloned();

        if let (Some(reaction_id), Some(msg_id)) = (reaction_info, message_id) {
            if let Err(e) = self.delete_reaction(&msg_id, &reaction_id).await {
                warn!("Failed to remove typing reaction for chat {chat_id}: {e}");
            }
        }
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

    /// Download media from Feishu CDN using an image_key or file_key.
    ///
    /// For images: uses `GET /im/v1/images/{image_key}`
    /// For files/audio/video: uses `GET /im/v1/messages/{message_id}/resources/{file_key}`
    pub async fn download_media(&self, url: &str, message_id: Option<&str>) -> Result<Vec<u8>> {
        let token = self.get_access_token().await?;

        if let Some(key) = url.strip_prefix("feishu://image/") {
            let api_url = format!("{FEISHU_BASE_URL}/im/v1/images/{key}");
            let resp = self
                .client
                .get(&api_url)
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .context("Failed to download Feishu image")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Feishu image download failed: status={status}, body={body}");
            }

            let bytes = resp
                .bytes()
                .await
                .context("Failed to read Feishu image response body")?;
            Ok(bytes.to_vec())
        } else if let Some(key) = url.strip_prefix("feishu://file/") {
            let mid = message_id.context("message_id required for file download")?;
            let api_url =
                format!("{FEISHU_BASE_URL}/im/v1/messages/{mid}/resources/{key}?type=file");
            let resp = self
                .client
                .get(&api_url)
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .context("Failed to download Feishu file")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Feishu file download failed: status={status}, body={body}");
            }

            let bytes = resp
                .bytes()
                .await
                .context("Failed to read Feishu file response body")?;
            Ok(bytes.to_vec())
        } else {
            anyhow::bail!("Unsupported Feishu media URL scheme: {url}");
        }
    }

    /// Upload an image to Feishu and return the `image_key`.
    async fn upload_image(&self, image_data: &[u8]) -> Result<String> {
        let token = self.get_access_token().await?;
        let url = format!("{FEISHU_BASE_URL}/im/v1/images");

        let part = reqwest::multipart::Part::bytes(image_data.to_vec())
            .file_name("image.png")
            .mime_str("image/png")
            .context("invalid mime type")?;

        let form = reqwest::multipart::Form::new()
            .text("image_type", "message_image")
            .part("image", part);

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .multipart(form)
            .send()
            .await
            .context("Failed to upload Feishu image")?;

        let feishu_resp: FeishuResponse<ImageUploadData> = resp
            .json()
            .await
            .context("Failed to parse Feishu image upload response")?;

        if feishu_resp.code != 0 {
            anyhow::bail!(
                "Feishu image upload failed: code={}, msg={:?}",
                feishu_resp.code,
                feishu_resp.msg
            );
        }

        feishu_resp
            .data
            .map(|d| d.image_key)
            .context("Feishu image upload response missing image_key")
    }

    /// Upload a file to Feishu and return the `file_key`.
    #[allow(dead_code)]
    async fn upload_file(
        &self,
        file_data: &[u8],
        file_name: &str,
        file_type: &str,
    ) -> Result<String> {
        let token = self.get_access_token().await?;
        let url = format!("{FEISHU_BASE_URL}/im/v1/files");

        let part = reqwest::multipart::Part::bytes(file_data.to_vec())
            .file_name(file_name.to_string())
            .mime_str("application/octet-stream")
            .context("invalid mime type")?;

        let form = reqwest::multipart::Form::new()
            .text("file_type", file_type.to_string())
            .part("file", part);

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .multipart(form)
            .send()
            .await
            .context("Failed to upload Feishu file")?;

        let feishu_resp: FeishuResponse<FileUploadData> = resp
            .json()
            .await
            .context("Failed to parse Feishu file upload response")?;

        if feishu_resp.code != 0 {
            anyhow::bail!(
                "Feishu file upload failed: code={}, msg={:?}",
                feishu_resp.code,
                feishu_resp.msg
            );
        }

        feishu_resp
            .data
            .map(|d| d.file_key)
            .context("Feishu file upload response missing file_key")
    }

    /// Download bytes from a URL.
    async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .context("Failed to download image from URL")?;

        if !resp.status().is_success() {
            anyhow::bail!("Failed to download image: status={}", resp.status());
        }

        let bytes = resp
            .bytes()
            .await
            .context("Failed to read image response body")?;
        Ok(bytes.to_vec())
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
        self.remove_typing_reaction(chat_id).await;
        self.send_text_message(chat_id, content, reply_to).await
    }

    async fn send_typing(&self, chat_id: &str, _trace_id: Option<&str>) -> Result<()> {
        let message_id = match self.last_message_ids.lock().get(chat_id).cloned() {
            Some(id) => id,
            None => {
                debug!("Feishu typing: no message_id tracked for chat {chat_id}");
                return Ok(());
            }
        };

        if self.typing_reactions.lock().contains_key(chat_id) {
            return Ok(());
        }

        match self.add_reaction(&message_id, "Typing").await {
            Ok(reaction_id) => {
                self.typing_reactions
                    .lock()
                    .insert(chat_id.to_string(), reaction_id);
            }
            Err(e) => {
                debug!("Feishu typing reaction failed: {e}");
            }
        }

        Ok(())
    }

    async fn send_image(
        &self,
        chat_id: &str,
        image_url: &str,
        caption: Option<&str>,
    ) -> Result<SendResult> {
        let token = self.get_access_token().await?;

        let image_data = self.fetch_bytes(image_url).await.map_err(|e| {
            warn!("Feishu send_image: failed to download from URL: {e}");
            e
        })?;

        let image_key = match self.upload_image(&image_data).await {
            Ok(key) => key,
            Err(e) => {
                warn!("Feishu send_image: upload failed, falling back to text: {e}");
                let text = match caption {
                    Some(c) => format!("{c}\n{image_url}"),
                    None => image_url.to_string(),
                };
                return self.send_text_message(chat_id, &text, None).await;
            }
        };

        let url = format!("{FEISHU_BASE_URL}/im/v1/messages?receive_id_type=chat_id");
        let content = serde_json::json!({
            "image_key": image_key
        })
        .to_string();

        let body = serde_json::json!({
            "receive_id": chat_id,
            "msg_type": "image",
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
            .context("Failed to send Feishu image message")?;

        let result = self.handle_send_response(resp).await?;

        if let Some(caption_text) = caption {
            if !caption_text.is_empty() {
                let _ = self.send_text_message(chat_id, caption_text, None).await;
            }
        }

        Ok(result)
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
        let result = parse_webhook(body.as_bytes(), None).unwrap();
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
        let result = parse_webhook(body.as_bytes(), None).unwrap();
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

        let result = parse_webhook(body.as_bytes(), None).unwrap();
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

        let result = parse_webhook(body.as_bytes(), None).unwrap();
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

        let result = parse_webhook(body.as_bytes(), None).unwrap();
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

    #[test]
    fn test_parse_webhook_card_action() {
        let body = r#"{
            "schema": "2.0",
            "header": {
                "event_id": "evt_card_001",
                "event_type": "card.action.trigger",
                "token": "xxx",
                "app_id": "cli_test"
            },
            "event": {
                "operator": {
                    "open_id": "ou_user123"
                },
                "open_chat_id": "oc_chat456",
                "open_message_id": "om_msg789",
                "action": {
                    "key": "approve_once",
                    "value": {"session_id": "sess_abc"}
                }
            }
        }"#;

        let result = parse_webhook(body.as_bytes()).unwrap();
        match result {
            WebhookResult::CardAction(action) => {
                assert_eq!(action.user_open_id, "ou_user123");
                assert_eq!(action.chat_id, "oc_chat456");
                assert_eq!(action.message_id, "om_msg789");
                assert_eq!(action.action_key, "approve_once");
                assert_eq!(
                    action.action_value,
                    serde_json::json!({"session_id": "sess_abc"})
                );
            }
            _ => panic!("Expected CardAction result, got {:?}", result),
        }
    }

    // ─── Dedup tests ─────────────────────────────

    #[test]
    fn test_dedup_new_key_not_duplicate() {
        let dedup = FeishuDedup::new(86400);
        assert!(!dedup.is_duplicate("msg_1"));
    }

    #[test]
    fn test_dedup_same_key_is_duplicate() {
        let dedup = FeishuDedup::new(86400);
        assert!(!dedup.is_duplicate("msg_1"));
        assert!(dedup.is_duplicate("msg_1"));
    }

    #[test]
    fn test_dedup_different_keys_not_duplicate() {
        let dedup = FeishuDedup::new(86400);
        assert!(!dedup.is_duplicate("msg_1"));
        assert!(!dedup.is_duplicate("msg_2"));
    }

    // ─── Batcher tests ───────────────────────────────────

    fn make_msg(id: &str, chat: &str, sender: &str, text: &str) -> InboundMessage {
        InboundMessage {
            channel: Platform::Feishu,
            sender_id: sender.to_string(),
            chat_id: chat.to_string(),
            content: text.to_string(),
            media: vec![],
            metadata: HashMap::new(),
            source: None,
            message_type: MessageType::Text,
            message_id: Some(id.to_string()),
            trace_id: None,
            reply_to: None,
            timestamp: chrono::Local::now(),
        }
    }

    fn make_message_event(
        chat_type: &str,
        chat_id: &str,
        sender_open_id: &str,
        content: &str,
    ) -> WebhookEvent {
        WebhookEvent {
            schema: Some("2.0".to_string()),
            header: Some(WebhookHeader {
                event_id: Some("evt_test".to_string()),
                event_type: Some("im.message.receive_v1".to_string()),
                token: None,
                app_id: None,
            }),
            event: Some(serde_json::json!({
                "message": {
                    "message_id": "msg_test",
                    "chat_id": chat_id,
                    "chat_type": chat_type,
                    "message_type": "text",
                    "content": content,
                },
                "sender": {
                    "sender_id": { "open_id": sender_open_id },
                    "sender_type": "user"
                }
            })),
            challenge: None,
            token: None,
            event_type: None,
        }
    }

    #[test]
    fn test_batcher_dedup_same_message_id() {
        let dedup = Arc::new(FeishuDedup::new(86400));
        let batcher = FeishuBatcher::new(dedup);
        let msg = make_msg("msg_1", "chat_1", "user_1", "hello");

        let result = batcher.process(msg);
        assert!(result.is_ok());

        let msg2 = make_msg("msg_1", "chat_1", "user_1", "hello");
        let result2 = batcher.process(msg2);
        assert!(result2.is_err());
    }

    #[test]
    fn test_batcher_dedup_same_content_fingerprint() {
        let dedup = Arc::new(FeishuDedup::new(86400));
        let batcher = FeishuBatcher::new(dedup);

        let msg1 = make_msg("msg_1", "chat_1", "user_1", "hello world");
        let msg2 = make_msg("msg_2", "chat_1", "user_1", "hello world");

        let result1 = batcher.process(msg1);
        assert!(result1.is_ok());

        let result2 = batcher.process(msg2);
        assert!(result2.is_err());
    }

    #[test]
    fn test_batcher_different_content_not_deduped() {
        let dedup = Arc::new(FeishuDedup::new(86400));
        let batcher = FeishuBatcher::new(dedup);

        let msg1 = make_msg("msg_1", "chat_1", "user_1", "hello");
        let msg2 = make_msg("msg_2", "chat_1", "user_1", "world");

        let result1 = batcher.process(msg1);
        assert!(result1.is_ok());

        let result2 = batcher.process(msg2);
        assert!(result2.is_ok());
    }

    #[test]
    fn test_batcher_buffers_into_batch() {
        let dedup = Arc::new(FeishuDedup::new(86400));
        let batcher = FeishuBatcher::new(dedup);

        let msg1 = make_msg("msg_1", "chat_1", "user_1", "hello");
        let msg2 = make_msg("msg_2", "chat_1", "user_1", "world");

        let r1 = batcher.process(msg1);
        assert!(matches!(r1, Ok(None)));

        let r2 = batcher.process(msg2);
        assert!(matches!(r2, Ok(None)));

        let ready = batcher.drain_ready();
        assert!(ready.is_empty());
    }

    #[test]
    fn test_merge_batch_single_message() {
        let msg = make_msg("msg_1", "chat_1", "user_1", "hello");
        let result = merge_batch(vec![msg]).unwrap();
        assert_eq!(result.content, "hello");
        assert_eq!(result.message_id, Some("msg_1".to_string()));
    }

    #[test]
    fn test_merge_batch_multiple_messages() {
        let msgs = vec![
            make_msg("msg_1", "chat_1", "user_1", "hello"),
            make_msg("msg_2", "chat_1", "user_1", "world"),
            make_msg("msg_3", "chat_1", "user_1", "foo"),
        ];
        let result = merge_batch(msgs).unwrap();
        assert_eq!(result.content, "hello\nworld\nfoo");
        assert_eq!(result.message_id, Some("msg_1".to_string()));
        assert_eq!(result.chat_id, "chat_1");
    }

    #[test]
    fn test_merge_batch_empty_returns_none() {
        assert!(merge_batch(vec![]).is_none());
    }

    // ─── Admission tests ─────────────────────────────────

    #[test]
    fn test_admission_open_group() {
        let event = make_message_event("group", "oc_group1", "ou_user1", "{\"text\":\"hello\"}");
        let config = FeishuConfig::default();
        assert_eq!(check_admission(&event, &config), Admission::Allow);
    }

    #[test]
    fn test_admission_open_dm() {
        let event = make_message_event("p2p", "oc_dm1", "ou_user1", "{\"text\":\"hello\"}");
        let config = FeishuConfig::default();
        assert_eq!(check_admission(&event, &config), Admission::Allow);
    }

    #[test]
    fn test_admission_group_disabled() {
        let event = make_message_event("group", "oc_group1", "ou_user1", "{\"text\":\"hello\"}");
        let mut config = FeishuConfig::default();
        config.group_policy = "disabled".to_string();
        assert_eq!(
            check_admission(&event, &config),
            Admission::Deny("group messages disabled".to_string())
        );
    }

    #[test]
    fn test_admission_group_allowlist_allowed() {
        let event = make_message_event("group", "oc_group1", "ou_user1", "{\"text\":\"hello\"}");
        let mut config = FeishuConfig::default();
        config.group_policy = "allowlist".to_string();
        config.group_allowlist = vec!["oc_group1".to_string()];
        assert_eq!(check_admission(&event, &config), Admission::Allow);
    }

    #[test]
    fn test_admission_group_allowlist_denied() {
        let event = make_message_event("group", "oc_group2", "ou_user1", "{\"text\":\"hello\"}");
        let mut config = FeishuConfig::default();
        config.group_policy = "allowlist".to_string();
        config.group_allowlist = vec!["oc_group1".to_string()];
        assert_eq!(
            check_admission(&event, &config),
            Admission::Deny("group not in allowlist".to_string())
        );
    }

    #[test]
    fn test_admission_group_blacklist_blocked() {
        let event = make_message_event("group", "oc_group1", "ou_user1", "{\"text\":\"hello\"}");
        let mut config = FeishuConfig::default();
        config.group_policy = "blacklist".to_string();
        config.group_blacklist = vec!["oc_group1".to_string()];
        assert_eq!(
            check_admission(&event, &config),
            Admission::Deny("group is blacklisted".to_string())
        );
    }

    #[test]
    fn test_admission_group_blacklist_pass() {
        let event = make_message_event("group", "oc_group2", "ou_user1", "{\"text\":\"hello\"}");
        let mut config = FeishuConfig::default();
        config.group_policy = "blacklist".to_string();
        config.group_blacklist = vec!["oc_group1".to_string()];
        assert_eq!(check_admission(&event, &config), Admission::Allow);
    }

    #[test]
    fn test_admission_dm_allowed_users() {
        let event = make_message_event("p2p", "oc_dm1", "ou_user1", "{\"text\":\"hello\"}");
        let mut config = FeishuConfig::default();
        config.allowed_users = vec!["ou_user1".to_string()];
        assert_eq!(check_admission(&event, &config), Admission::Allow);

        let event2 = make_message_event("p2p", "oc_dm1", "ou_user2", "{\"text\":\"hello\"}");
        assert_eq!(
            check_admission(&event2, &config),
            Admission::Deny("user not allowed".to_string())
        );
    }

    #[test]
    fn test_admission_verification_token_valid() {
        let mut event = make_message_event("p2p", "oc_dm1", "ou_user1", "{\"text\":\"hello\"}");
        event.header.as_mut().unwrap().token = Some("my_token".to_string());
        let mut config = FeishuConfig::default();
        config.verification_token = Some("my_token".to_string());
        assert_eq!(check_admission(&event, &config), Admission::Allow);
    }

    #[test]
    fn test_admission_verification_token_invalid() {
        let mut event = make_message_event("p2p", "oc_dm1", "ou_user1", "{\"text\":\"hello\"}");
        event.header.as_mut().unwrap().token = Some("wrong_token".to_string());
        let mut config = FeishuConfig::default();
        config.verification_token = Some("my_token".to_string());
        assert_eq!(
            check_admission(&event, &config),
            Admission::Deny("verification token mismatch".to_string())
        );
    }

    #[test]
    fn test_admission_challenge_bypasses() {
        let event = WebhookEvent {
            schema: None,
            header: None,
            event: None,
            challenge: Some("test_challenge".to_string()),
            token: Some("token".to_string()),
            event_type: None,
        };
        let config = FeishuConfig::default();
        assert_eq!(check_admission(&event, &config), Admission::Allow);
    }

    #[test]
    fn test_admission_mention_only_in_group() {
        let event = make_message_event("group", "oc_group1", "ou_user1", "{\"text\":\"hello\"}");
        let mut config = FeishuConfig::default();
        config.mention_only = true;
        assert_eq!(
            check_admission(&event, &config),
            Admission::Deny("mention required in groups".to_string())
        );
    }

    #[test]
    fn test_admission_mention_only_with_mention() {
        let event = make_message_event(
            "group",
            "oc_group1",
            "ou_user1",
            "<at user_id=\"ou_bot\">Bot</at> hello",
        );
        let mut config = FeishuConfig::default();
        config.mention_only = true;
        assert_eq!(check_admission(&event, &config), Admission::Allow);
    }
        }
    }

    #[test]
    fn test_parse_webhook_card_action_deny() {
        let body = r#"{
            "schema": "2.0",
            "header": {
                "event_type": "card.action.trigger"
            },
            "event": {
                "operator": {"open_id": "ou_admin"},
                "open_chat_id": "oc_group",
                "open_message_id": "om_card1",
                "action": {
                    "key": "deny",
                    "value": {"reason": "not authorized"}
                }
            }
        }"#;

        let result = parse_webhook(body.as_bytes()).unwrap();
        match result {
            WebhookResult::CardAction(action) => {
                assert_eq!(action.action_key, "deny");
                assert_eq!(action.chat_id, "oc_group");
            }
            _ => panic!("Expected CardAction result"),
        }
    }

    #[test]
    fn test_parse_webhook_card_action_missing_key_ignored() {
        let body = r#"{
            "schema": "2.0",
            "header": {
                "event_type": "card.action.trigger"
            },
            "event": {
                "operator": {"open_id": "ou_user"},
                "open_chat_id": "oc_chat",
                "open_message_id": "om_msg",
                "action": {}
            }
        }"#;

        let result = parse_webhook(body.as_bytes()).unwrap();
        assert!(matches!(result, WebhookResult::Ignored));
    }

    #[test]
    fn test_track_message_id() {
        let ch = FeishuChannel::new();
        assert!(!ch.last_message_ids.lock().contains_key("oc_chat1"));

        ch.track_message_id("oc_chat1", "om_msg123");
        assert_eq!(
            ch.last_message_ids
                .lock()
                .get("oc_chat1")
                .map(|s| s.clone()),
            Some("om_msg123".to_string())
        );

        ch.track_message_id("oc_chat1", "om_msg456");
        assert_eq!(
            ch.last_message_ids
                .lock()
                .get("oc_chat1")
                .map(|s| s.clone()),
            Some("om_msg456".to_string())
        );
    }

    #[test]
    fn test_batcher_dedup_same_message_id() {
        let dedup = Arc::new(FeishuDedup::new(86400));
        let batcher = FeishuBatcher::new(dedup);
        let msg = make_msg("msg_1", "chat_1", "user_1", "hello");

        let result = batcher.process(msg);
        assert!(result.is_ok());

        let msg2 = make_msg("msg_1", "chat_1", "user_1", "hello");
        let result2 = batcher.process(msg2);
        assert!(result2.is_err());
    }

    #[test]
    fn test_batcher_dedup_same_content_fingerprint() {
        let dedup = Arc::new(FeishuDedup::new(86400));
        let batcher = FeishuBatcher::new(dedup);

        let msg1 = make_msg("msg_1", "chat_1", "user_1", "hello world");
        let msg2 = make_msg("msg_2", "chat_1", "user_1", "hello world");

        let result1 = batcher.process(msg1);
        assert!(result1.is_ok());

        let result2 = batcher.process(msg2);
        assert!(result2.is_err());
    }

    #[test]
    fn test_batcher_different_content_not_deduped() {
        let dedup = Arc::new(FeishuDedup::new(86400));
        let batcher = FeishuBatcher::new(dedup);

        let msg1 = make_msg("msg_1", "chat_1", "user_1", "hello");
        let msg2 = make_msg("msg_2", "chat_1", "user_1", "world");

        let result1 = batcher.process(msg1);
        assert!(result1.is_ok());

        let result2 = batcher.process(msg2);
        assert!(result2.is_ok());
    }

    #[test]
    fn test_batcher_buffers_into_batch() {
        let dedup = Arc::new(FeishuDedup::new(86400));
        let batcher = FeishuBatcher::new(dedup);

        let msg1 = make_msg("msg_1", "chat_1", "user_1", "hello");
        let msg2 = make_msg("msg_2", "chat_1", "user_1", "world");

        let r1 = batcher.process(msg1);
        assert!(matches!(r1, Ok(None)));

        let r2 = batcher.process(msg2);
        assert!(matches!(r2, Ok(None)));

        let ready = batcher.drain_ready();
        assert!(ready.is_empty());
    }

    #[test]
    fn test_merge_batch_single_message() {
        let msg = make_msg("msg_1", "chat_1", "user_1", "hello");
        let result = merge_batch(vec![msg]).unwrap();
        assert_eq!(result.content, "hello");
        assert_eq!(result.message_id, Some("msg_1".to_string()));
    }

    #[test]
    fn test_merge_batch_multiple_messages() {
        let msgs = vec![
            make_msg("msg_1", "chat_1", "user_1", "hello"),
            make_msg("msg_2", "chat_1", "user_1", "world"),
            make_msg("msg_3", "chat_1", "user_1", "foo"),
        ];
        let result = merge_batch(msgs).unwrap();
        assert_eq!(result.content, "hello\nworld\nfoo");
        assert_eq!(result.message_id, Some("msg_1".to_string()));
        assert_eq!(result.chat_id, "chat_1");
    }

    #[test]
    fn test_merge_batch_empty_returns_none() {
        assert!(merge_batch(vec![]).is_none());
    }

    // ─── Admission tests ─────────────────────────────────

    #[test]
    fn test_admission_open_group() {
        let event = make_message_event("group", "oc_group1", "ou_user1", "{\"text\":\"hello\"}");
        let config = FeishuConfig::default();
        assert_eq!(check_admission(&event, &config), Admission::Allow);
    }

    #[test]
    fn test_admission_open_dm() {
        let event = make_message_event("p2p", "oc_dm1", "ou_user1", "{\"text\":\"hello\"}");
        let config = FeishuConfig::default();
        assert_eq!(check_admission(&event, &config), Admission::Allow);
    }

    #[test]
    fn test_admission_group_disabled() {
        let event = make_message_event("group", "oc_group1", "ou_user1", "{\"text\":\"hello\"}");
        let mut config = FeishuConfig::default();
        config.group_policy = "disabled".to_string();
        assert_eq!(
            check_admission(&event, &config),
            Admission::Deny("group messages disabled".to_string())
        );
    }

    #[test]
    fn test_admission_group_allowlist_allowed() {
        let event = make_message_event("group", "oc_group1", "ou_user1", "{\"text\":\"hello\"}");
        let mut config = FeishuConfig::default();
        config.group_policy = "allowlist".to_string();
        config.group_allowlist = vec!["oc_group1".to_string()];
        assert_eq!(check_admission(&event, &config), Admission::Allow);
    }

    #[test]
    fn test_admission_group_allowlist_denied() {
        let event = make_message_event("group", "oc_group2", "ou_user1", "{\"text\":\"hello\"}");
        let mut config = FeishuConfig::default();
        config.group_policy = "allowlist".to_string();
        config.group_allowlist = vec!["oc_group1".to_string()];
        assert_eq!(
            check_admission(&event, &config),
            Admission::Deny("group not in allowlist".to_string())
        );
    }

    #[test]
    fn test_admission_group_blacklist_blocked() {
        let event = make_message_event("group", "oc_group1", "ou_user1", "{\"text\":\"hello\"}");
        let mut config = FeishuConfig::default();
        config.group_policy = "blacklist".to_string();
        config.group_blacklist = vec!["oc_group1".to_string()];
        assert_eq!(
            check_admission(&event, &config),
            Admission::Deny("group is blacklisted".to_string())
        );
    }

    #[test]
    fn test_admission_group_blacklist_pass() {
        let event = make_message_event("group", "oc_group2", "ou_user1", "{\"text\":\"hello\"}");
        let mut config = FeishuConfig::default();
        config.group_policy = "blacklist".to_string();
        config.group_blacklist = vec!["oc_group1".to_string()];
        assert_eq!(check_admission(&event, &config), Admission::Allow);
    }

    #[test]
    fn test_admission_dm_allowed_users() {
        let event = make_message_event("p2p", "oc_dm1", "ou_user1", "{\"text\":\"hello\"}");
        let mut config = FeishuConfig::default();
        config.allowed_users = vec!["ou_user1".to_string()];
        assert_eq!(check_admission(&event, &config), Admission::Allow);

        let event2 = make_message_event("p2p", "oc_dm1", "ou_user2", "{\"text\":\"hello\"}");
        assert_eq!(
            check_admission(&event2, &config),
            Admission::Deny("user not allowed".to_string())
        );
    }

    #[test]
    fn test_admission_verification_token_valid() {
        let mut event = make_message_event("p2p", "oc_dm1", "ou_user1", "{\"text\":\"hello\"}");
        event.header.as_mut().unwrap().token = Some("my_token".to_string());
        let mut config = FeishuConfig::default();
        config.verification_token = Some("my_token".to_string());
        assert_eq!(check_admission(&event, &config), Admission::Allow);
    }

    #[test]
    fn test_admission_verification_token_invalid() {
        let mut event = make_message_event("p2p", "oc_dm1", "ou_user1", "{\"text\":\"hello\"}");
        event.header.as_mut().unwrap().token = Some("wrong_token".to_string());
        let mut config = FeishuConfig::default();
        config.verification_token = Some("my_token".to_string());
        assert_eq!(
            check_admission(&event, &config),
            Admission::Deny("verification token mismatch".to_string())
        );
    }

    #[test]
    fn test_admission_challenge_bypasses() {
        let event = WebhookEvent {
            schema: None,
            header: None,
            event: None,
            challenge: Some("test_challenge".to_string()),
            token: Some("token".to_string()),
            event_type: None,
        };
        let config = FeishuConfig::default();
        assert_eq!(check_admission(&event, &config), Admission::Allow);
    }

    #[test]
    fn test_admission_mention_only_in_group() {
        let event = make_message_event("group", "oc_group1", "ou_user1", "{\"text\":\"hello\"}");
        let mut config = FeishuConfig::default();
        config.mention_only = true;
        assert_eq!(
            check_admission(&event, &config),
            Admission::Deny("mention required in groups".to_string())
        );
    }

    #[test]
    fn test_admission_mention_only_with_mention() {
        let event = make_message_event(
            "group",
            "oc_group1",
            "ou_user1",
            "<at user_id=\"ou_bot\">Bot</at> hello",
        );
        let mut config = FeishuConfig::default();
        config.mention_only = true;
        assert_eq!(check_admission(&event, &config), Admission::Allow);
    }
}
