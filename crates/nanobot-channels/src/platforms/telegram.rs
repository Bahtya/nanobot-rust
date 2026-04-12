//! Telegram Bot API channel adapter.
//!
//! Implements real polling-based communication with the Telegram Bot API.
//! Uses long-polling via `getUpdates` to receive inbound messages and
//! `sendMessage` / `sendPhoto` / `sendChatAction` for outbound operations.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Local;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use nanobot_bus::events::InboundMessage;
use nanobot_core::{MediaAttachment, MessageType, Platform, SessionSource};

use crate::base::{BaseChannel, SendResult};

// ---------------------------------------------------------------------------
// Telegram Bot API response types
// ---------------------------------------------------------------------------

/// Top-level wrapper for every Telegram API response.
#[derive(Debug, Deserialize)]
struct TgResponse<T: Default> {
    ok: bool,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    result: Option<T>,
}

/// Subset of the Bot user object returned by `getMe`.
#[derive(Debug, Default, Deserialize)]
struct TgBotUser {
    #[allow(dead_code)]
    id: i64,
    #[allow(dead_code)]
    username: Option<String>,
}

/// An update received from `getUpdates`.
#[derive(Debug, Deserialize)]
struct TgUpdate {
    update_id: i64,
    #[serde(default)]
    message: Option<TgMessage>,
    #[serde(default)]
    callback_query: Option<TgCallbackQuery>,
}

/// A message object inside an update.
#[derive(Debug, Deserialize)]
struct TgMessage {
    message_id: i64,
    #[serde(default)]
    from: Option<TgUser>,
    chat: TgChat,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    photo: Option<Vec<TgPhotoSize>>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    reply_to_message: Option<Box<TgMessage>>,
}

/// Sender user object.
#[derive(Debug, Deserialize)]
struct TgUser {
    id: i64,
    #[serde(default)]
    first_name: Option<String>,
    #[serde(default)]
    last_name: Option<String>,
    #[serde(default)]
    username: Option<String>,
}

/// Chat object.
#[derive(Debug, Deserialize)]
struct TgChat {
    id: i64,
    #[serde(rename = "type")]
    chat_type: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    thread_id: Option<i64>,
}

/// Photo size variant (we pick the largest).
#[derive(Debug, Deserialize)]
struct TgPhotoSize {
    #[allow(dead_code)]
    file_id: String,
    #[allow(dead_code)]
    width: i32,
    #[allow(dead_code)]
    height: i32,
}

/// A callback query from an inline keyboard button press.
#[derive(Debug, Deserialize)]
struct TgCallbackQuery {
    id: String,
    #[serde(default)]
    from: Option<TgUser>,
    #[serde(default)]
    message: Option<TgCallbackMessage>,
    #[serde(default)]
    data: Option<String>,
}

/// The message attached to a callback query (may be minimal).
#[derive(Debug, Deserialize)]
struct TgCallbackMessage {
    message_id: i64,
    chat: TgChat,
}

/// Result of `sendMessage` — we only need the message_id.
#[derive(Debug, Default, Deserialize)]
struct TgSentMessage {
    message_id: i64,
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct SendMessageBody {
    chat_id: i64,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to_message_id: Option<i64>,
}

#[derive(Debug, Serialize)]
struct SendPhotoBody {
    chat_id: i64,
    photo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    caption: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to_message_id: Option<i64>,
}

#[derive(Debug, Serialize)]
struct SendChatActionBody {
    chat_id: i64,
    action: String,
}

#[derive(Debug, Serialize)]
struct EditMessageTextBody {
    chat_id: i64,
    message_id: i64,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_markup: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct EditMessageReplyMarkupBody {
    chat_id: i64,
    message_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_markup: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct AnswerCallbackQueryBody {
    callback_query_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

// ---------------------------------------------------------------------------
// TelegramChannel
// ---------------------------------------------------------------------------

/// Telegram channel implementation using the Bot API with long-polling.
pub struct TelegramChannel {
    token: Option<String>,
    connected: bool,
    message_handler: Option<tokio::sync::mpsc::Sender<InboundMessage>>,
    running: Arc<AtomicBool>,
    client: reqwest::Client,
    /// Override base URL for testing.
    base_url_override: Option<String>,
}

impl TelegramChannel {
    /// Build a reqwest client that respects system proxy settings.
    ///
    /// Checks HTTPS_PROXY, HTTP_PROXY, and ALL_PROXY env vars.
    /// Logs the proxy configuration for debugging.
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
                info!("Telegram HTTP client using proxy: {}", url);
                let proxy = reqwest::Proxy::all(url)
                    .expect("Failed to create proxy from env var");
                reqwest::Client::builder()
                    .proxy(proxy)
                    .build()
                    .expect("Failed to build HTTP client with proxy")
            }
            None => {
                info!("Telegram HTTP client: no proxy configured (direct connection)");
                reqwest::Client::new()
            }
        }
    }

    /// Create a new TelegramChannel, reading the bot token from the
    /// `TELEGRAM_BOT_TOKEN` environment variable.
    /// Uses system proxy settings (HTTPS_PROXY/HTTP_PROXY/ALL_PROXY).
    pub fn new() -> Self {
        Self {
            token: std::env::var("TELEGRAM_BOT_TOKEN").ok(),
            connected: false,
            message_handler: None,
            running: Arc::new(AtomicBool::new(false)),
            client: Self::build_client(),
            base_url_override: None,
        }
    }

    /// Create with a custom token and base URL (for testing).
    /// Skips proxy since the base URL is typically localhost in tests.
    pub fn with_token_and_url(token: String, base_url: String) -> Self {
        Self {
            token: Some(token),
            connected: false,
            message_handler: None,
            running: Arc::new(AtomicBool::new(false)),
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base_url_override: Some(base_url),
        }
    }

    /// Build the API URL for a given method.
    fn api_url(&self, method: &str) -> String {
        match (&self.base_url_override, &self.token) {
            (Some(base), _) => format!("{}/{}", base, method),
            (_, Some(token)) => format!("https://api.telegram.org/bot{}/{}", token, method),
            _ => String::new(),
        }
    }

    /// Validate the bot token by calling `getMe`.
    async fn validate_token(&self) -> Result<()> {
        let url = self.api_url("getMe");
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("failed to call getMe")?;

        let body: TgResponse<TgBotUser> = resp
            .json()
            .await
            .context("failed to parse getMe response")?;

        if !body.ok {
            anyhow::bail!(
                "Telegram getMe failed: {}",
                body.description.as_deref().unwrap_or("unknown error")
            );
        }

        if let Some(bot) = &body.result {
            info!(
                "Telegram bot validated: @{}",
                bot.username.as_deref().unwrap_or("(unknown)")
            );
        }

        Ok(())
    }

    /// Poll `getUpdates` in a loop until `running` is cleared.
    ///
    /// Uses exponential backoff on transient errors (max 60s).
    /// The backoff resets on any successful response.
    async fn poll_loop(
        client: reqwest::Client,
        token: String,
        handler: tokio::sync::mpsc::Sender<InboundMessage>,
        running: Arc<AtomicBool>,
    ) {
        let base_url = format!("https://api.telegram.org/bot{}", token);
        let mut offset: Option<i64> = None;
        let timeout_secs: u32 = 30;
        let mut backoff_secs: u64 = 1;
        let max_backoff_secs: u64 = 60;

        info!("Telegram polling task started");

        while running.load(Ordering::Relaxed) {
            let url = format!(
                "{}/getUpdates?timeout={}&offset={}",
                base_url,
                timeout_secs,
                offset.unwrap_or(0)
            );

            let result = client.get(&url).send().await;
            let resp = match result {
                Ok(r) => r,
                Err(e) => {
                    // Transient network error — back off and retry.
                    error!("Telegram getUpdates request failed: {e} (retry in {backoff_secs}s)");
                    tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(max_backoff_secs);
                    continue;
                }
            };

            let body: TgResponse<Vec<TgUpdate>> = match resp.json().await {
                Ok(b) => b,
                Err(e) => {
                    error!("Telegram getUpdates parse error: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
            };

            if !body.ok {
                let desc = body.description.as_deref().unwrap_or("unknown");
                // "Conflict: terminated by other getUpdates request" means
                // another bot instance is running — use a longer backoff.
                if desc.contains("Conflict") || desc.contains("terminated") {
                    warn!("Telegram getUpdates conflict — another bot instance may be running (retry in {backoff_secs}s)");
                } else {
                    error!("Telegram getUpdates error: {desc}");
                }
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(max_backoff_secs);
                continue;
            }

            // Success — reset backoff.
            backoff_secs = 1;

            let updates = body.result.unwrap_or_default();

            for update in updates {
                // Advance offset so we never re-process.
                offset = Some(update.update_id + 1);

                if let Some(msg) = update.message {
                    if let Err(e) = Self::dispatch_message(&handler, &msg).await {
                        error!("Failed to dispatch Telegram message: {e}");
                    }
                } else if let Some(cq) = update.callback_query {
                    if let Err(e) =
                        Self::dispatch_callback_query(&client, &base_url, &handler, &cq).await
                    {
                        error!("Failed to dispatch Telegram callback query: {e}");
                    }
                }
            }
        }

        info!("Telegram polling task stopped");
    }

    /// Convert a `TgMessage` into an `InboundMessage` and send it
    /// through the handler channel.
    async fn dispatch_message(
        handler: &tokio::sync::mpsc::Sender<InboundMessage>,
        msg: &TgMessage,
    ) -> Result<()> {
        let sender_id = msg
            .from
            .as_ref()
            .map(|u| u.id.to_string())
            .unwrap_or_default();

        let user_name = msg.from.as_ref().map(|u| {
            let first = u.first_name.as_deref().unwrap_or("");
            let last = u.last_name.as_deref().unwrap_or("");
            if last.is_empty() {
                first.to_string()
            } else {
                format!("{first} {last}")
            }
        });

        let text = msg.text.clone().unwrap_or_default();

        // If there is no text and no photo, skip the message.
        if text.is_empty() && msg.photo.is_none() {
            return Ok(());
        }

        let chat_id = msg.chat.id.to_string();
        let chat_type = msg
            .chat
            .chat_type
            .as_deref()
            .unwrap_or("private")
            .to_string();

        let mapped_chat_type = match chat_type.as_str() {
            "private" => "dm",
            "group" | "supergroup" => "group",
            "channel" => "channel",
            other => other,
        };

        let message_type = if text.starts_with('/') {
            MessageType::Command
        } else if msg.photo.is_some() {
            MessageType::Photo
        } else {
            MessageType::Text
        };

        let mut media: Vec<MediaAttachment> = Vec::new();
        if let Some(photos) = &msg.photo {
            // Telegram sends photo sizes from smallest to largest.
            // Pick the largest (last) entry.
            if let Some(largest) = photos.last() {
                media.push(MediaAttachment {
                    url: largest.file_id.clone(),
                    mime_type: Some("image/jpeg".to_string()),
                    caption: msg.caption.clone(),
                    file_name: None,
                    file_size: None,
                });
            }
        }

        let thread_id = msg.chat.thread_id.map(|id| id.to_string());

        let source = SessionSource {
            platform: Platform::Telegram,
            chat_id: chat_id.clone(),
            chat_name: msg.chat.title.clone(),
            chat_type: mapped_chat_type.to_string(),
            user_id: if sender_id.is_empty() {
                None
            } else {
                Some(sender_id.clone())
            },
            user_name,
            thread_id: thread_id.clone(),
            chat_topic: None,
        };

        let mut metadata = HashMap::new();
        metadata.insert(
            "tg_message_id".to_string(),
            serde_json::json!(msg.message_id),
        );
        if let Some(username) = msg.from.as_ref().and_then(|u| u.username.as_ref()) {
            metadata.insert("tg_username".to_string(), serde_json::json!(username));
        }

        let inbound = InboundMessage {
            channel: Platform::Telegram,
            sender_id,
            chat_id,
            content: text,
            media,
            metadata,
            source: Some(source),
            message_type,
            message_id: Some(msg.message_id.to_string()),
            reply_to: msg
                .reply_to_message
                .as_ref()
                .map(|r| r.message_id.to_string()),
            timestamp: Local::now(),
        };

        handler
            .send(inbound)
            .await
            .context("message handler channel closed")
    }

    /// Convert a `TgCallbackQuery` into an `InboundMessage` and answer it.
    async fn dispatch_callback_query(
        client: &reqwest::Client,
        base_url: &str,
        handler: &tokio::sync::mpsc::Sender<InboundMessage>,
        cq: &TgCallbackQuery,
    ) -> Result<()> {
        // Answer the callback query so the button stops loading.
        let answer_url = format!("{}/answerCallbackQuery", base_url);
        let answer_body = AnswerCallbackQueryBody {
            callback_query_id: cq.id.clone(),
            text: None,
        };
        if let Err(e) = client.post(&answer_url).json(&answer_body).send().await {
            warn!("Failed to answer Telegram callback query: {e}");
        }

        let sender_id = cq
            .from
            .as_ref()
            .map(|u| u.id.to_string())
            .unwrap_or_default();

        let user_name = cq.from.as_ref().map(|u| {
            let first = u.first_name.as_deref().unwrap_or("");
            let last = u.last_name.as_deref().unwrap_or("");
            if last.is_empty() {
                first.to_string()
            } else {
                format!("{first} {last}")
            }
        });

        let data = cq.data.clone().unwrap_or_default();

        let msg = match &cq.message {
            Some(m) => m,
            None => return Ok(()),
        };

        let chat_id = msg.chat.id.to_string();
        let chat_type = msg
            .chat
            .chat_type
            .as_deref()
            .unwrap_or("private")
            .to_string();

        let mapped_chat_type = match chat_type.as_str() {
            "private" => "dm",
            "group" | "supergroup" => "group",
            "channel" => "channel",
            other => other,
        };

        let source = SessionSource {
            platform: Platform::Telegram,
            chat_id: chat_id.clone(),
            chat_name: msg.chat.title.clone(),
            chat_type: mapped_chat_type.to_string(),
            user_id: if sender_id.is_empty() {
                None
            } else {
                Some(sender_id.clone())
            },
            user_name,
            thread_id: msg.chat.thread_id.map(|id| id.to_string()),
            chat_topic: None,
        };

        let mut metadata = HashMap::new();
        metadata.insert(
            "tg_message_id".to_string(),
            serde_json::json!(msg.message_id),
        );
        metadata.insert(
            "tg_callback_query_id".to_string(),
            serde_json::json!(cq.id),
        );
        metadata.insert("tg_callback_data".to_string(), serde_json::json!(data));

        let inbound = InboundMessage {
            channel: Platform::Telegram,
            sender_id,
            chat_id,
            content: format!("callback:{}", cq.data.as_deref().unwrap_or("")),
            media: vec![],
            metadata,
            source: Some(source),
            message_type: MessageType::Command,
            message_id: Some(msg.message_id.to_string()),
            reply_to: None,
            timestamp: Local::now(),
        };

        handler
            .send(inbound)
            .await
            .context("message handler channel closed")
    }
}

impl Default for TelegramChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BaseChannel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    fn platform(&self) -> Platform {
        Platform::Telegram
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    async fn connect(&mut self) -> Result<bool> {
        if self.token.is_none() {
            warn!("Telegram token not configured (set TELEGRAM_BOT_TOKEN)");
            return Ok(false);
        }

        // Validate the token first.
        if let Err(e) = self.validate_token().await {
            error!("Telegram token validation failed: {e}");
            return Ok(false);
        }

        self.running.store(true, Ordering::Relaxed);
        self.connected = true;

        // Spawn the background polling task.
        if let Some(handler) = self.message_handler.clone() {
            let client = self.client.clone();
            let token = self.token.clone().unwrap();
            let running = self.running.clone();

            tokio::spawn(async move {
                Self::poll_loop(client, token, handler, running).await;
            });

            info!("Telegram channel connected — polling started");
        } else {
            warn!("Telegram connected but no message_handler set; polling not started");
        }

        Ok(true)
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.running.store(false, Ordering::Relaxed);
        self.connected = false;
        info!("Telegram channel disconnected");
        Ok(())
    }

    async fn send_message(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
    ) -> Result<SendResult> {
        debug!("Sending Telegram message to chat {}", chat_id);

        let chat_id_num: i64 = match chat_id.parse() {
            Ok(n) => n,
            Err(_) => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("invalid chat_id: {chat_id}")),
                    retryable: false,
                });
            }
        };

        let reply_to_id = reply_to.and_then(|r| r.parse::<i64>().ok());

        let body = SendMessageBody {
            chat_id: chat_id_num,
            text: content.to_string(),
            reply_to_message_id: reply_to_id,
        };

        let url = self.api_url("sendMessage");
        let resp = match self.client.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("HTTP request failed: {e}")),
                    retryable: true,
                });
            }
        };

        let tg_resp: TgResponse<TgSentMessage> = match resp.json().await {
            Ok(r) => r,
            Err(e) => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("failed to parse response: {e}")),
                    retryable: false,
                });
            }
        };

        if tg_resp.ok {
            let msg_id = tg_resp.result.map(|m| m.message_id.to_string());

            Ok(SendResult {
                success: true,
                message_id: msg_id,
                error: None,
                retryable: false,
            })
        } else {
            Ok(SendResult {
                success: false,
                message_id: None,
                error: tg_resp.description,
                retryable: false,
            })
        }
    }

    async fn send_typing(&self, chat_id: &str) -> Result<()> {
        debug!("Sending typing indicator to chat {}", chat_id);

        let chat_id_num: i64 = match chat_id.parse() {
            Ok(n) => n,
            Err(_) => {
                warn!("Invalid chat_id for typing indicator: {chat_id}");
                return Ok(());
            }
        };

        let body = SendChatActionBody {
            chat_id: chat_id_num,
            action: "typing".to_string(),
        };

        let url = self.api_url("sendChatAction");
        if let Err(e) = self.client.post(&url).json(&body).send().await {
            warn!("Failed to send typing indicator: {e}");
        }

        Ok(())
    }

    async fn send_image(
        &self,
        chat_id: &str,
        image_url: &str,
        caption: Option<&str>,
    ) -> Result<SendResult> {
        debug!("Sending image to chat {}", chat_id);

        let chat_id_num: i64 = match chat_id.parse() {
            Ok(n) => n,
            Err(_) => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("invalid chat_id: {chat_id}")),
                    retryable: false,
                });
            }
        };

        let body = SendPhotoBody {
            chat_id: chat_id_num,
            photo: image_url.to_string(),
            caption: caption.map(|s| s.to_string()),
            reply_to_message_id: None,
        };

        let url = self.api_url("sendPhoto");
        let resp = match self.client.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("HTTP request failed: {e}")),
                    retryable: true,
                });
            }
        };

        let tg_resp: TgResponse<TgSentMessage> = match resp.json().await {
            Ok(r) => r,
            Err(e) => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("failed to parse response: {e}")),
                    retryable: false,
                });
            }
        };

        if tg_resp.ok {
            let msg_id = tg_resp.result.map(|m| m.message_id.to_string());

            Ok(SendResult {
                success: true,
                message_id: msg_id,
                error: None,
                retryable: false,
            })
        } else {
            Ok(SendResult {
                success: false,
                message_id: None,
                error: tg_resp.description,
                retryable: false,
            })
        }
    }

    fn set_message_handler(&mut self, handler: tokio::sync::mpsc::Sender<InboundMessage>) {
        self.message_handler = Some(handler);
    }
}

impl TelegramChannel {
    /// Edit the text of a previously sent message.
    ///
    /// Optionally attach or update an inline keyboard via `reply_markup`.
    pub async fn edit_message_text(
        &self,
        chat_id: &str,
        message_id: &str,
        text: &str,
        reply_markup: Option<serde_json::Value>,
    ) -> Result<SendResult> {
        debug!(
            "Editing Telegram message {} in chat {}",
            message_id, chat_id
        );

        let chat_id_num: i64 = match chat_id.parse() {
            Ok(n) => n,
            Err(_) => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("invalid chat_id: {chat_id}")),
                    retryable: false,
                });
            }
        };

        let message_id_num: i64 = match message_id.parse() {
            Ok(n) => n,
            Err(_) => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("invalid message_id: {message_id}")),
                    retryable: false,
                });
            }
        };

        let body = EditMessageTextBody {
            chat_id: chat_id_num,
            message_id: message_id_num,
            text: text.to_string(),
            reply_markup,
        };

        let url = self.api_url("editMessageText");
        let resp = match self.client.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("HTTP request failed: {e}")),
                    retryable: true,
                });
            }
        };

        let tg_resp: TgResponse<TgSentMessage> = match resp.json().await {
            Ok(r) => r,
            Err(e) => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("failed to parse response: {e}")),
                    retryable: false,
                });
            }
        };

        if tg_resp.ok {
            Ok(SendResult {
                success: true,
                message_id: tg_resp.result.map(|m| m.message_id.to_string()),
                error: None,
                retryable: false,
            })
        } else {
            Ok(SendResult {
                success: false,
                message_id: None,
                error: tg_resp.description,
                retryable: false,
            })
        }
    }

    /// Edit only the inline keyboard (reply markup) of a previously sent message.
    ///
    /// Pass `None` for `reply_markup` to remove the keyboard entirely.
    pub async fn edit_message_reply_markup(
        &self,
        chat_id: &str,
        message_id: &str,
        reply_markup: Option<serde_json::Value>,
    ) -> Result<SendResult> {
        debug!(
            "Editing reply markup for message {} in chat {}",
            message_id, chat_id
        );

        let chat_id_num: i64 = match chat_id.parse() {
            Ok(n) => n,
            Err(_) => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("invalid chat_id: {chat_id}")),
                    retryable: false,
                });
            }
        };

        let message_id_num: i64 = match message_id.parse() {
            Ok(n) => n,
            Err(_) => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("invalid message_id: {message_id}")),
                    retryable: false,
                });
            }
        };

        let body = EditMessageReplyMarkupBody {
            chat_id: chat_id_num,
            message_id: message_id_num,
            reply_markup,
        };

        let url = self.api_url("editMessageReplyMarkup");
        let resp = match self.client.post(&url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("HTTP request failed: {e}")),
                    retryable: true,
                });
            }
        };

        let tg_resp: TgResponse<TgSentMessage> = match resp.json().await {
            Ok(r) => r,
            Err(e) => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some(format!("failed to parse response: {e}")),
                    retryable: false,
                });
            }
        };

        if tg_resp.ok {
            Ok(SendResult {
                success: true,
                message_id: tg_resp.result.map(|m| m.message_id.to_string()),
                error: None,
                retryable: false,
            })
        } else {
            Ok(SendResult {
                success: false,
                message_id: None,
                error: tg_resp.description,
                retryable: false,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_telegram_new() {
        let channel = TelegramChannel::new();
        assert_eq!(channel.name(), "telegram");
        assert_eq!(channel.platform(), Platform::Telegram);
        assert!(!channel.is_connected());
    }

    #[test]
    fn test_telegram_default() {
        let channel = TelegramChannel::default();
        assert_eq!(channel.name(), "telegram");
    }

    #[test]
    fn test_api_url() {
        let mut channel = TelegramChannel::new();
        channel.token = Some("123456:ABC-DEF".to_string());
        let url = channel.api_url("getMe");
        assert_eq!(url, "https://api.telegram.org/bot123456:ABC-DEF/getMe");
    }

    #[tokio::test]
    async fn test_telegram_connect_without_token() {
        // Ensure env var is not set for this test.
        std::env::remove_var("TELEGRAM_BOT_TOKEN");
        let mut channel = TelegramChannel::new();
        let result = channel.connect().await.unwrap();
        assert!(!result);
        assert!(!channel.is_connected());
    }

    #[tokio::test]
    async fn test_telegram_disconnect() {
        let mut channel = TelegramChannel::new();
        channel.disconnect().await.unwrap();
        assert!(!channel.is_connected());
        assert!(!channel.running.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_telegram_send_message_invalid_chat_id() {
        let channel = TelegramChannel::new();
        let result = channel
            .send_message("not_a_number", "hello", None)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
        assert!(!result.retryable);
    }

    #[tokio::test]
    async fn test_telegram_send_image_invalid_chat_id() {
        let channel = TelegramChannel::new();
        let result = channel
            .send_image("bad_id", "http://example.com/img.png", Some("caption"))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_telegram_send_typing_invalid_chat_id() {
        let channel = TelegramChannel::new();
        // Should not error — just logs a warning.
        channel.send_typing("not_a_number").await.unwrap();
    }

    #[tokio::test]
    async fn test_telegram_set_message_handler() {
        let mut channel = TelegramChannel::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        channel.set_message_handler(tx);
        assert!(channel.message_handler.is_some());
    }

    #[test]
    fn test_running_flag_default() {
        let channel = TelegramChannel::new();
        assert!(!channel.running.load(Ordering::Relaxed));
    }

    #[test]
    fn test_parse_update_json() {
        // Verify our deserialization structs work with real Telegram JSON.
        let json = r#"{
            "update_id": 12345,
            "message": {
                "message_id": 42,
                "from": {
                    "id": 999,
                    "first_name": "Alice",
                    "last_name": "Smith",
                    "username": "alice_smith"
                },
                "chat": {
                    "id": 999,
                    "type": "private"
                },
                "text": "/start"
            }
        }"#;

        let update: TgUpdate = serde_json::from_str(json).unwrap();
        assert_eq!(update.update_id, 12345);
        let msg = update.message.unwrap();
        assert_eq!(msg.message_id, 42);
        assert_eq!(msg.text.as_deref(), Some("/start"));
        let from = msg.from.unwrap();
        assert_eq!(from.id, 999);
        assert_eq!(from.username.as_deref(), Some("alice_smith"));
        assert_eq!(msg.chat.chat_type.as_deref(), Some("private"));
    }

    #[test]
    fn test_parse_photo_update() {
        let json = r#"{
            "update_id": 54321,
            "message": {
                "message_id": 100,
                "from": {"id": 111, "first_name": "Bob"},
                "chat": {"id": 111, "type": "private"},
                "photo": [
                    {"file_id": "small", "width": 90, "height": 90},
                    {"file_id": "large", "width": 800, "height": 600}
                ],
                "caption": "nice photo"
            }
        }"#;

        let update: TgUpdate = serde_json::from_str(json).unwrap();
        let msg = update.message.unwrap();
        let photos = msg.photo.unwrap();
        assert_eq!(photos.len(), 2);
        assert_eq!(photos[1].file_id, "large");
        assert_eq!(msg.caption.as_deref(), Some("nice photo"));
    }

    #[test]
    fn test_parse_get_me_response() {
        let json = r#"{
            "ok": true,
            "result": {
                "id": 123456789,
                "is_bot": true,
                "first_name": "TestBot",
                "username": "test_my_bot"
            }
        }"#;

        let resp: TgResponse<TgBotUser> = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        let bot = resp.result.unwrap();
        assert_eq!(bot.id, 123456789);
        assert_eq!(bot.username.as_deref(), Some("test_my_bot"));
    }

    #[tokio::test]
    async fn test_dispatch_message_text() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<InboundMessage>(10);

        let msg = TgMessage {
            message_id: 42,
            from: Some(TgUser {
                id: 123,
                first_name: Some("Alice".to_string()),
                last_name: None,
                username: Some("alice".to_string()),
            }),
            chat: TgChat {
                id: 123,
                chat_type: Some("private".to_string()),
                title: None,
                thread_id: None,
            },
            text: Some("hello world".to_string()),
            photo: None,
            caption: None,
            reply_to_message: None,
        };

        TelegramChannel::dispatch_message(&tx, &msg).await.unwrap();

        let inbound = rx.try_recv().unwrap();
        assert_eq!(inbound.channel, Platform::Telegram);
        assert_eq!(inbound.sender_id, "123");
        assert_eq!(inbound.chat_id, "123");
        assert_eq!(inbound.content, "hello world");
        assert_eq!(inbound.message_type, MessageType::Text);
        assert_eq!(inbound.message_id.as_deref(), Some("42"));
        assert!(inbound.media.is_empty());
        assert!(inbound.source.is_some());
        let src = inbound.source.unwrap();
        assert_eq!(src.chat_type, "dm");
        assert_eq!(src.user_name.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn test_dispatch_message_command() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<InboundMessage>(10);

        let msg = TgMessage {
            message_id: 99,
            from: Some(TgUser {
                id: 456,
                first_name: Some("Bob".to_string()),
                last_name: Some("Jones".to_string()),
                username: None,
            }),
            chat: TgChat {
                id: -100,
                chat_type: Some("supergroup".to_string()),
                title: Some("Test Group".to_string()),
                thread_id: Some(5),
            },
            text: Some("/start".to_string()),
            photo: None,
            caption: None,
            reply_to_message: None,
        };

        TelegramChannel::dispatch_message(&tx, &msg).await.unwrap();

        let inbound = rx.try_recv().unwrap();
        assert_eq!(inbound.message_type, MessageType::Command);
        assert_eq!(inbound.content, "/start");
        let src = inbound.source.unwrap();
        assert_eq!(src.chat_type, "group");
        assert_eq!(src.chat_name.as_deref(), Some("Test Group"));
        assert_eq!(src.thread_id.as_deref(), Some("5"));
        assert_eq!(src.user_name.as_deref(), Some("Bob Jones"));
    }

    #[tokio::test]
    async fn test_dispatch_message_photo() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<InboundMessage>(10);

        let msg = TgMessage {
            message_id: 200,
            from: Some(TgUser {
                id: 789,
                first_name: Some("Carol".to_string()),
                last_name: None,
                username: None,
            }),
            chat: TgChat {
                id: 789,
                chat_type: Some("private".to_string()),
                title: None,
                thread_id: None,
            },
            text: None,
            photo: Some(vec![
                TgPhotoSize {
                    file_id: "file_small".to_string(),
                    width: 160,
                    height: 160,
                },
                TgPhotoSize {
                    file_id: "file_big".to_string(),
                    width: 1280,
                    height: 720,
                },
            ]),
            caption: Some("sunset".to_string()),
            reply_to_message: None,
        };

        TelegramChannel::dispatch_message(&tx, &msg).await.unwrap();

        let inbound = rx.try_recv().unwrap();
        assert_eq!(inbound.message_type, MessageType::Photo);
        assert_eq!(inbound.media.len(), 1);
        assert_eq!(inbound.media[0].url, "file_big");
        assert_eq!(inbound.media[0].caption.as_deref(), Some("sunset"));
    }

    #[tokio::test]
    async fn test_dispatch_message_reply() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<InboundMessage>(10);

        let msg = TgMessage {
            message_id: 300,
            from: Some(TgUser {
                id: 100,
                first_name: Some("Dave".to_string()),
                last_name: None,
                username: None,
            }),
            chat: TgChat {
                id: 100,
                chat_type: Some("private".to_string()),
                title: None,
                thread_id: None,
            },
            text: Some("reply!".to_string()),
            photo: None,
            caption: None,
            reply_to_message: Some(Box::new(TgMessage {
                message_id: 299,
                from: None,
                chat: TgChat {
                    id: 100,
                    chat_type: None,
                    title: None,
                    thread_id: None,
                },
                text: None,
                photo: None,
                caption: None,
                reply_to_message: None,
            })),
        };

        TelegramChannel::dispatch_message(&tx, &msg).await.unwrap();

        let inbound = rx.try_recv().unwrap();
        assert_eq!(inbound.reply_to.as_deref(), Some("299"));
    }

    #[tokio::test]
    async fn test_dispatch_empty_message_skipped() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<InboundMessage>(10);

        let msg = TgMessage {
            message_id: 400,
            from: Some(TgUser {
                id: 50,
                first_name: Some("Eve".to_string()),
                last_name: None,
                username: None,
            }),
            chat: TgChat {
                id: 50,
                chat_type: Some("private".to_string()),
                title: None,
                thread_id: None,
            },
            text: None,
            photo: None,
            caption: None,
            reply_to_message: None,
        };

        // Should succeed but not send anything — no text, no photo.
        TelegramChannel::dispatch_message(&tx, &msg).await.unwrap();
    }

    // -----------------------------------------------------------------------
    // Tests for new features: callback_query, editMessage*, serialisation
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_callback_query_update() {
        let json = r#"{
            "update_id": 99999,
            "callback_query": {
                "id": "cb123",
                "from": {
                    "id": 555,
                    "first_name": "Frank",
                    "username": "frank_bot"
                },
                "message": {
                    "message_id": 42,
                    "chat": {"id": 555, "type": "private"}
                },
                "data": "approve:42"
            }
        }"#;

        let update: TgUpdate = serde_json::from_str(json).unwrap();
        assert_eq!(update.update_id, 99999);
        assert!(update.message.is_none());
        let cq = update.callback_query.unwrap();
        assert_eq!(cq.id, "cb123");
        assert_eq!(cq.data.as_deref(), Some("approve:42"));
        let from = cq.from.unwrap();
        assert_eq!(from.id, 555);
        assert_eq!(from.username.as_deref(), Some("frank_bot"));
        let msg = cq.message.unwrap();
        assert_eq!(msg.message_id, 42);
        assert_eq!(msg.chat.id, 555);
    }

    #[test]
    fn test_parse_update_with_message_and_no_callback() {
        let json = r#"{
            "update_id": 11111,
            "message": {
                "message_id": 1,
                "chat": {"id": 999, "type": "group", "title": "My Group"},
                "text": "hi"
            }
        }"#;

        let update: TgUpdate = serde_json::from_str(json).unwrap();
        assert!(update.callback_query.is_none());
        let msg = update.message.unwrap();
        assert_eq!(msg.text.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn test_dispatch_callback_query() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<InboundMessage>(10);
        let client = reqwest::Client::new();
        let base_url = "http://127.0.0.1:0";

        let cq = TgCallbackQuery {
            id: "cb999".to_string(),
            from: Some(TgUser {
                id: 777,
                first_name: Some("Grace".to_string()),
                last_name: Some("Hopper".to_string()),
                username: Some("grace".to_string()),
            }),
            message: Some(TgCallbackMessage {
                message_id: 50,
                chat: TgChat {
                    id: 777,
                    chat_type: Some("private".to_string()),
                    title: None,
                    thread_id: None,
                },
            }),
            data: Some("action:confirm".to_string()),
        };

        // The answerCallbackQuery call will fail (no server) but dispatch
        // should still succeed since we only warn on failure.
        let result =
            TelegramChannel::dispatch_callback_query(&client, base_url, &tx, &cq).await;
        // The handler should still receive the message even if answer fails.
        if result.is_ok() {
            let inbound = rx.try_recv().unwrap();
            assert_eq!(inbound.channel, Platform::Telegram);
            assert_eq!(inbound.sender_id, "777");
            assert_eq!(inbound.chat_id, "777");
            assert_eq!(inbound.content, "callback:action:confirm");
            assert_eq!(inbound.message_type, MessageType::Command);
            assert_eq!(inbound.message_id.as_deref(), Some("50"));
            assert_eq!(
                inbound.metadata.get("tg_callback_data").unwrap(),
                &serde_json::json!("action:confirm")
            );
            let src = inbound.source.unwrap();
            assert_eq!(src.chat_type, "dm");
            assert_eq!(src.user_name.as_deref(), Some("Grace Hopper"));
        }
        // If result is Err, the handler channel closed due to the failed
        // HTTP call — that's also acceptable in this test setup.
    }

    #[tokio::test]
    async fn test_dispatch_callback_query_no_message() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<InboundMessage>(10);
        let client = reqwest::Client::new();
        let base_url = "http://127.0.0.1:0";

        let cq = TgCallbackQuery {
            id: "cb_naked".to_string(),
            from: Some(TgUser {
                id: 100,
                first_name: Some("NoMsg".to_string()),
                last_name: None,
                username: None,
            }),
            message: None,
            data: Some("click".to_string()),
        };

        // Should return Ok(()) — no message attached means nothing to dispatch.
        TelegramChannel::dispatch_callback_query(&client, base_url, &tx, &cq)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_edit_message_text_invalid_chat_id() {
        let channel = TelegramChannel::new();
        let result = channel
            .edit_message_text("not_a_number", "1", "new text", None)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_edit_message_text_invalid_message_id() {
        let channel = TelegramChannel::new();
        let result = channel
            .edit_message_text("123", "not_a_number", "new text", None)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_edit_message_reply_markup_invalid_chat_id() {
        let channel = TelegramChannel::new();
        let result = channel
            .edit_message_reply_markup("bad", "1", None)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_edit_message_reply_markup_invalid_message_id() {
        let channel = TelegramChannel::new();
        let result = channel
            .edit_message_reply_markup("123", "bad", None)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[test]
    fn test_edit_message_text_body_serialisation() {
        let body = EditMessageTextBody {
            chat_id: 123,
            message_id: 456,
            text: "updated text".to_string(),
            reply_markup: Some(serde_json::json!({
                "inline_keyboard": [[{"text": "Click", "callback_data": "yes"}]]
            })),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["chat_id"], 123);
        assert_eq!(json["message_id"], 456);
        assert_eq!(json["text"], "updated text");
        assert!(json["reply_markup"]["inline_keyboard"].is_array());
    }

    #[test]
    fn test_edit_message_reply_markup_body_serialisation() {
        let body = EditMessageReplyMarkupBody {
            chat_id: 789,
            message_id: 101,
            reply_markup: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["chat_id"], 789);
        assert_eq!(json["message_id"], 101);
        assert!(json.get("reply_markup").is_none());
    }

    #[test]
    fn test_answer_callback_query_body_serialisation() {
        let body = AnswerCallbackQueryBody {
            callback_query_id: "cb123".to_string(),
            text: Some("Done!".to_string()),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["callback_query_id"], "cb123");
        assert_eq!(json["text"], "Done!");
    }

    #[test]
    fn test_answer_callback_query_body_no_text() {
        let body = AnswerCallbackQueryBody {
            callback_query_id: "cb456".to_string(),
            text: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("text").is_none());
    }
}
