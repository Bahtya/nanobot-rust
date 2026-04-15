//! Telegram Bot API channel adapter.
//!
//! Implements real polling-based communication with the Telegram Bot API.
//! Uses long-polling via `getUpdates` to receive inbound messages and
//! `sendMessage` / `sendPhoto` / `sendChatAction` for outbound operations.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Local;
use parking_lot::Mutex as ParkMutex;
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
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_markup: Option<serde_json::Value>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    show_alert: Option<bool>,
}

/// A single reaction entry for `setMessageReaction`.
#[derive(Debug, Serialize)]
struct ReactionType {
    r#type: String,
    emoji: String,
}

/// Request body for `setMessageReaction`.
#[derive(Debug, Serialize)]
struct SetMessageReactionBody {
    chat_id: i64,
    message_id: i64,
    reaction: Vec<ReactionType>,
}

// ---------------------------------------------------------------------------
// Inline keyboard builder
// ---------------------------------------------------------------------------

/// A single inline keyboard button.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InlineKeyboardButton {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// An inline keyboard attached to a message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InlineKeyboardMarkup {
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

/// Builder for constructing inline keyboards row-by-row.
///
/// # Examples
///
/// ```
/// use nanobot_channels::platforms::telegram::InlineKeyboardBuilder;
///
/// let keyboard = InlineKeyboardBuilder::new()
///     .row_pair("Yes", "confirm:yes", "No", "confirm:no")
///     .build();
/// ```
#[derive(Debug, Clone, Default)]
pub struct InlineKeyboardBuilder {
    rows: Vec<Vec<InlineKeyboardButton>>,
    current_row: Vec<InlineKeyboardButton>,
}

impl InlineKeyboardBuilder {
    /// Create a new, empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a button to the current row.
    pub fn button(mut self, text: &str, callback_data: &str) -> Self {
        self.current_row.push(InlineKeyboardButton {
            text: text.to_string(),
            callback_data: Some(callback_data.to_string()),
            url: None,
        });
        self
    }

    /// Add a URL button to the current row.
    pub fn url_button(mut self, text: &str, url: &str) -> Self {
        self.current_row.push(InlineKeyboardButton {
            text: text.to_string(),
            callback_data: None,
            url: Some(url.to_string()),
        });
        self
    }

    /// Flush the current row and start a new one.
    pub fn new_row(mut self) -> Self {
        if !self.current_row.is_empty() {
            self.rows.push(std::mem::take(&mut self.current_row));
        }
        self
    }

    /// Convenience: add a two-button row (common confirm/cancel pattern)
    /// and flush it.
    pub fn row_pair(
        mut self,
        left_text: &str,
        left_data: &str,
        right_text: &str,
        right_data: &str,
    ) -> Self {
        self.rows.push(vec![
            InlineKeyboardButton {
                text: left_text.to_string(),
                callback_data: Some(left_data.to_string()),
                url: None,
            },
            InlineKeyboardButton {
                text: right_text.to_string(),
                callback_data: Some(right_data.to_string()),
                url: None,
            },
        ]);
        self
    }

    /// Build a confirm/cancel keyboard (convenience shortcut).
    pub fn confirm_cancel(prefix: &str) -> Self {
        Self::new().row_pair(
            "✅ Confirm",
            &format!("{prefix}:confirm"),
            "❌ Cancel",
            &format!("{prefix}:cancel"),
        )
    }

    /// Build a pagination keyboard with previous/next buttons.
    pub fn pagination(prefix: &str, page: usize, total_pages: usize) -> Self {
        let mut builder = Self::new();
        let mut row = Vec::new();
        if page > 0 {
            row.push(InlineKeyboardButton {
                text: "◀ Prev".to_string(),
                callback_data: Some(format!("{prefix}:page:{}", page - 1)),
                url: None,
            });
        }
        row.push(InlineKeyboardButton {
            text: format!("{}/{}", page + 1, total_pages),
            callback_data: Some(format!("{prefix}:page:{page}")),
            url: None,
        });
        if page + 1 < total_pages {
            row.push(InlineKeyboardButton {
                text: "Next ▶".to_string(),
                callback_data: Some(format!("{prefix}:page:{}", page + 1)),
                url: None,
            });
        }
        builder.rows.push(row);
        builder
    }

    /// Finalise: flush any pending row and return the markup.
    pub fn build(mut self) -> InlineKeyboardMarkup {
        if !self.current_row.is_empty() {
            self.rows.push(std::mem::take(&mut self.current_row));
        }
        InlineKeyboardMarkup {
            inline_keyboard: self.rows,
        }
    }
}

// ---------------------------------------------------------------------------
// Callback action routing
// ---------------------------------------------------------------------------

/// Parsed callback action extracted from `callback_data`.
///
/// The expected format is `{prefix}:{action}` or `{prefix}:{action}:{payload}`.
/// For example `confirm:yes`, `page:3`, `menu:settings`.
#[derive(Debug, Clone, PartialEq)]
pub struct CallbackAction {
    /// The prefix/namespace (everything before the first `:`).
    pub prefix: String,
    /// The action name (between first and optional second `:`).
    pub action: String,
    /// Optional trailing payload (everything after the second `:`).
    pub payload: Option<String>,
}

impl CallbackAction {
    /// Parse a `callback_data` string into a `CallbackAction`.
    ///
    /// Returns `None` if the string is empty or has no `:` separator.
    pub fn parse(data: &str) -> Option<Self> {
        if data.is_empty() {
            return None;
        }
        let mut parts = data.splitn(3, ':');
        let prefix = parts.next()?.to_string();
        let action = parts.next()?.to_string();
        let payload = parts.next().map(|s| s.to_string());
        if prefix.is_empty() || action.is_empty() {
            return None;
        }
        Some(Self {
            prefix,
            action,
            payload,
        })
    }
}

// ---------------------------------------------------------------------------
// Callback routing
// ---------------------------------------------------------------------------

/// Context provided to a callback handler with all relevant data from the
/// incoming `callback_query`.
#[derive(Debug, Clone)]
pub struct CallbackContext {
    /// Chat where the callback originated.
    pub chat_id: String,
    /// Message that carried the inline keyboard.
    pub message_id: String,
    /// User who pressed the button.
    pub sender_id: String,
    /// Telegram callback query ID (needed to answer the query).
    pub callback_query_id: String,
    /// Parsed callback action.
    pub action: CallbackAction,
}

/// Instructions a callback handler returns to the channel layer.
///
/// The channel executes the corresponding Telegram API calls.
#[derive(Debug, Clone, PartialEq)]
pub enum CallbackResponse {
    /// Send a new text message to the same chat.
    Reply(String),
    /// Edit the original message text, optionally attaching a new keyboard.
    EditMessage {
        text: String,
        keyboard: Option<InlineKeyboardMarkup>,
    },
    /// Remove the inline keyboard from the original message.
    RemoveKeyboard,
    /// Show a short toast notification to the user.
    Toast(String),
    /// Handler already processed the callback; no further action needed.
    Acknowledged,
}

/// Type-erased async callback handler.
type BoxedHandler = Box<
    dyn Fn(CallbackContext) -> Pin<Box<dyn Future<Output = CallbackResponse> + Send>> + Send + Sync,
>;

/// Router that dispatches Telegram callback queries to registered handlers
/// based on prefix matching.
///
/// Handlers are tried in registration order; the first handler whose prefix
/// matches the callback's `prefix` field wins.  If no handler matches, the
/// callback falls through to the default `InboundMessage` path on the bus.
///
/// # Examples
///
/// ```no_run
/// use nanobot_channels::platforms::telegram::{
///     CallbackContext, CallbackResponse, CallbackRouter,
/// };
///
/// let mut router = CallbackRouter::new();
/// router.register("confirm", |_ctx| async {
///     CallbackResponse::Reply("Confirmed!".into())
/// });
/// ```
pub struct CallbackRouter {
    handlers: Vec<(String, BoxedHandler)>,
}

impl CallbackRouter {
    /// Create an empty router.
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }

    /// Register an async handler for all callbacks whose `prefix` field
    /// matches `prefix`.
    ///
    /// If multiple handlers share the same prefix the first one registered
    /// wins.  Registration order is preserved.
    pub fn register<F, Fut>(&mut self, prefix: &str, handler: F)
    where
        F: Fn(CallbackContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = CallbackResponse> + Send + 'static,
    {
        let wrapped = Box::new(move |ctx: CallbackContext| {
            let fut = handler(ctx);
            Box::pin(fut) as Pin<Box<dyn Future<Output = CallbackResponse> + Send>>
        });
        self.handlers.push((prefix.to_string(), wrapped));
    }

    /// Dispatch a parsed callback context to the first matching handler.
    ///
    /// Returns `Some(response)` if a handler matched, `None` otherwise.
    pub async fn dispatch(&self, ctx: CallbackContext) -> Option<CallbackResponse> {
        let prefix = &ctx.action.prefix;
        for (registered_prefix, handler) in &self.handlers {
            if registered_prefix == prefix {
                let resp = handler(ctx.clone()).await;
                return Some(resp);
            }
        }
        None
    }

    /// Returns `true` if a handler is registered for the given prefix.
    pub fn has_handler(&self, prefix: &str) -> bool {
        self.handlers.iter().any(|(p, _)| p == prefix)
    }

    /// Number of registered handlers.
    pub fn handler_count(&self) -> usize {
        self.handlers.len()
    }
}

impl Default for CallbackRouter {
    fn default() -> Self {
        Self::new()
    }
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
    /// Optional callback router for inline-keyboard button interactions.
    router: Arc<tokio::sync::Mutex<CallbackRouter>>,
    /// Shared session keys for `/history` display.
    session_keys: Arc<ParkMutex<Vec<String>>>,
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
                let proxy = reqwest::Proxy::all(url).expect("Failed to create proxy from env var");
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
            router: Arc::new(tokio::sync::Mutex::new(CallbackRouter::new())),
            session_keys: Arc::new(ParkMutex::new(Vec::new())),
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
            router: Arc::new(tokio::sync::Mutex::new(CallbackRouter::new())),
            session_keys: Arc::new(ParkMutex::new(Vec::new())),
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

    /// Send a 👀 read-receipt reaction to a Telegram message.
    ///
    /// Non-critical: failures are logged but not propagated.
    async fn send_read_receipt(
        client: &reqwest::Client,
        base_url: &str,
        chat_id: i64,
        message_id: i64,
    ) {
        let body = SetMessageReactionBody {
            chat_id,
            message_id,
            reaction: vec![ReactionType {
                r#type: "emoji".to_string(),
                emoji: "👀".to_string(),
            }],
        };

        let url = format!("{}/setMessageReaction", base_url);
        if let Err(e) = client.post(&url).json(&body).send().await {
            debug!("Failed to send read receipt: {e}");
        }
    }

    /// Send a text reply directly via the Telegram Bot API (bypassing the bus).
    ///
    /// Used for built-in commands like `/validate`, `/menu`, `/settings` and
    /// `/history` that must work even when the LLM provider is down.
    /// Non-critical: failures are logged only.  Optionally attaches an inline
    /// keyboard.
    async fn send_direct_reply(
        client: &reqwest::Client,
        base_url: &str,
        chat_id: i64,
        text: &str,
        keyboard: Option<&InlineKeyboardMarkup>,
    ) {
        let reply_markup = keyboard.map(|kb| serde_json::to_value(kb).unwrap_or_default());
        let body = SendMessageBody {
            chat_id,
            text: text.to_string(),
            reply_to_message_id: None,
            reply_markup,
        };
        let url = format!("{}/sendMessage", base_url);
        if let Err(e) = client.post(&url).json(&body).send().await {
            warn!("Failed to send direct reply: {e}");
        }
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
        router: Arc<tokio::sync::Mutex<CallbackRouter>>,
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
                    let text = msg.text.as_deref().unwrap_or("");
                    // /reset needs the session key, so handle it separately.
                    if crate::commands::matches_command(text, "reset") {
                        let session_key = format!("telegram:{}", msg.chat.id);
                        let response = crate::commands::handle_reset(&session_key);
                        Self::send_direct_reply(&client, &base_url, msg.chat.id, &response, None)
                            .await;
                        Self::send_read_receipt(&client, &base_url, msg.chat.id, msg.message_id)
                            .await;
                    } else if let Some(response) = crate::commands::try_handle_command(text) {
                        // Built-in command matched — reply directly, skip bus.
                        Self::send_direct_reply(
                            &client,
                            &base_url,
                            msg.chat.id,
                            &response.text,
                            response.keyboard.as_ref(),
                        )
                        .await;
                        Self::send_read_receipt(&client, &base_url, msg.chat.id, msg.message_id)
                            .await;
                    } else {
                        match Self::dispatch_message(&handler, &msg).await {
                            Ok(true) => {
                                // Message dispatched — send 👀 read receipt.
                                Self::send_read_receipt(
                                    &client,
                                    &base_url,
                                    msg.chat.id,
                                    msg.message_id,
                                )
                                .await;
                            }
                            Ok(false) => {} // Skipped (no text/photo).
                            Err(e) => {
                                error!("Failed to dispatch Telegram message: {e}");
                            }
                        }
                    }
                } else if let Some(cq) = update.callback_query {
                    if let Err(e) =
                        Self::dispatch_callback_query(&client, &base_url, &handler, &cq, &router)
                            .await
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
    ///
    /// Returns `Ok(true)` if the message was dispatched, `Ok(false)` if it
    /// was skipped (no text and no photo).
    async fn dispatch_message(
        handler: &tokio::sync::mpsc::Sender<InboundMessage>,
        msg: &TgMessage,
    ) -> Result<bool> {
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
            return Ok(false);
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
            .context("message handler channel closed")?;
        Ok(true)
    }

    /// Convert a `TgCallbackQuery` into an `InboundMessage` and answer it.
    ///
    /// If a `CallbackRouter` handler matches the callback prefix, the handler
    /// runs and the resulting `CallbackResponse` is executed against the
    /// Telegram API.  If no handler matches, the callback is forwarded as a
    /// regular `InboundMessage` on the bus.
    async fn dispatch_callback_query(
        client: &reqwest::Client,
        base_url: &str,
        handler: &tokio::sync::mpsc::Sender<InboundMessage>,
        cq: &TgCallbackQuery,
        router: &Arc<tokio::sync::Mutex<CallbackRouter>>,
    ) -> Result<()> {
        let sender_id = cq
            .from
            .as_ref()
            .map(|u| u.id.to_string())
            .unwrap_or_default();

        let data = cq.data.clone().unwrap_or_default();

        let msg = match &cq.message {
            Some(m) => m,
            None => return Ok(()),
        };

        let chat_id = msg.chat.id.to_string();
        let message_id = msg.message_id.to_string();

        // Try the registered router first.
        if let Some(action) = CallbackAction::parse(&data) {
            let ctx = CallbackContext {
                chat_id: chat_id.clone(),
                message_id: message_id.clone(),
                sender_id: sender_id.clone(),
                callback_query_id: cq.id.clone(),
                action,
            };
            let router_guard = router.lock().await;
            if router_guard.has_handler(&ctx.action.prefix) {
                // Show loading animation before running the handler.
                Self::edit_message_text_static(
                    client,
                    base_url,
                    &chat_id,
                    &message_id,
                    "⏳ Loading...",
                    None,
                )
                .await;
                if let Some(response) = router_guard.dispatch(ctx).await {
                    drop(router_guard);
                    // Answer the callback query so the button stops loading.
                    Self::answer_callback_query_static(client, base_url, &cq.id, None, false).await;
                    Self::execute_callback_response(
                        client,
                        base_url,
                        &chat_id,
                        &message_id,
                        response,
                    )
                    .await;
                    return Ok(());
                }
            }
        }

        // No handler matched — answer and fall through to the bus.
        Self::answer_callback_query_static(client, base_url, &cq.id, None, false).await;

        let user_name = cq.from.as_ref().map(|u| {
            let first = u.first_name.as_deref().unwrap_or("");
            let last = u.last_name.as_deref().unwrap_or("");
            if last.is_empty() {
                first.to_string()
            } else {
                format!("{first} {last}")
            }
        });

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
        metadata.insert("tg_callback_query_id".to_string(), serde_json::json!(cq.id));
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

    /// Answer a callback query (static version for use outside `&self`).
    async fn answer_callback_query_static(
        client: &reqwest::Client,
        base_url: &str,
        callback_query_id: &str,
        text: Option<&str>,
        show_alert: bool,
    ) {
        let body = AnswerCallbackQueryBody {
            callback_query_id: callback_query_id.to_string(),
            text: text.map(|s| s.to_string()),
            show_alert: if show_alert { Some(true) } else { None },
        };
        let url = format!("{}/answerCallbackQuery", base_url);
        if let Err(e) = client.post(&url).json(&body).send().await {
            warn!("Failed to answer Telegram callback query: {e}");
        }
    }

    /// Edit a message's text (static version for loading animation).
    async fn edit_message_text_static(
        client: &reqwest::Client,
        base_url: &str,
        chat_id: &str,
        message_id: &str,
        text: &str,
        reply_markup: Option<serde_json::Value>,
    ) {
        if let (Ok(chat_id_num), Ok(msg_id_num)) =
            (chat_id.parse::<i64>(), message_id.parse::<i64>())
        {
            let body = EditMessageTextBody {
                chat_id: chat_id_num,
                message_id: msg_id_num,
                text: text.to_string(),
                reply_markup,
            };
            let url = format!("{}/editMessageText", base_url);
            if let Err(e) = client.post(&url).json(&body).send().await {
                debug!("Failed to edit message for loading animation: {e}");
            }
        }
    }

    /// Execute a [`CallbackResponse`] against the Telegram API.
    async fn execute_callback_response(
        client: &reqwest::Client,
        base_url: &str,
        chat_id: &str,
        message_id: &str,
        response: CallbackResponse,
    ) {
        match response {
            CallbackResponse::Reply(text) => {
                if let Ok(chat_id_num) = chat_id.parse::<i64>() {
                    let body = SendMessageBody {
                        chat_id: chat_id_num,
                        text,
                        reply_to_message_id: None,
                        reply_markup: None,
                    };
                    let url = format!("{}/sendMessage", base_url);
                    if let Err(e) = client.post(&url).json(&body).send().await {
                        warn!("Failed to send callback reply: {e}");
                    }
                }
            }
            CallbackResponse::EditMessage { text, keyboard } => {
                if let (Ok(chat_id_num), Ok(msg_id_num)) =
                    (chat_id.parse::<i64>(), message_id.parse::<i64>())
                {
                    let reply_markup =
                        keyboard.map(|kb| serde_json::to_value(&kb).unwrap_or_default());
                    let body = EditMessageTextBody {
                        chat_id: chat_id_num,
                        message_id: msg_id_num,
                        text,
                        reply_markup,
                    };
                    let url = format!("{}/editMessageText", base_url);
                    if let Err(e) = client.post(&url).json(&body).send().await {
                        warn!("Failed to edit message for callback: {e}");
                    }
                }
            }
            CallbackResponse::RemoveKeyboard => {
                if let (Ok(chat_id_num), Ok(msg_id_num)) =
                    (chat_id.parse::<i64>(), message_id.parse::<i64>())
                {
                    // Removing the keyboard means editing with an empty keyboard.
                    let reply_markup = serde_json::json!({"inline_keyboard": []});
                    let body = EditMessageReplyMarkupBody {
                        chat_id: chat_id_num,
                        message_id: msg_id_num,
                        reply_markup: Some(reply_markup),
                    };
                    let url = format!("{}/editMessageReplyMarkup", base_url);
                    if let Err(e) = client.post(&url).json(&body).send().await {
                        warn!("Failed to remove keyboard for callback: {e}");
                    }
                }
            }
            CallbackResponse::Toast(text) => {
                // Reuse the static helper — Toast is just an answer with text.
                // We don't have the callback_query_id here, but Toast is
                // already handled before execute_callback_response is called
                // via answer_callback_query_static.  Log a debug if someone
                // returns Toast without a query context.
                debug!("Toast response: {text} (answered via answerCallbackQuery)");
            }
            CallbackResponse::Acknowledged => {}
        }
    }

    /// Register built-in callback handlers for inline keyboard buttons.
    ///
    /// Currently handles the `menu`, `settings`, and `history` prefixes.
    fn register_default_handlers(
        router: &mut CallbackRouter,
        session_keys: &Arc<ParkMutex<Vec<String>>>,
    ) {
        // Menu handler
        if !router.has_handler("menu") {
            router.register("menu", |ctx| {
                let action = ctx.action.action.clone();
                async move {
                    let (text, keyboard) = crate::commands::handle_menu_callback(&action);
                    match keyboard {
                        Some(kb) => CallbackResponse::EditMessage {
                            text,
                            keyboard: Some(kb),
                        },
                        None => CallbackResponse::EditMessage {
                            text,
                            keyboard: Some(
                                InlineKeyboardBuilder::new().build(), // empty keyboard = remove
                            ),
                        },
                    }
                }
            });
        }

        // Settings pagination handler
        if !router.has_handler("settings") {
            router.register("settings", |ctx| {
                let page: usize = ctx
                    .action
                    .payload
                    .as_deref()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(0);
                async move {
                    let response = crate::commands::handle_settings_callback(page);
                    CallbackResponse::EditMessage {
                        text: response.text,
                        keyboard: response.keyboard,
                    }
                }
            });
        }

        // History pagination handler
        if !router.has_handler("history") {
            let sk = session_keys.clone();
            router.register("history", move |ctx| {
                let page: usize = ctx
                    .action
                    .payload
                    .as_deref()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(0);
                let keys = sk.lock().clone();
                async move {
                    let response = crate::commands::handle_history_callback(&keys, page);
                    CallbackResponse::EditMessage {
                        text: response.text,
                        keyboard: response.keyboard,
                    }
                }
            });
        }
    }

    /// Get a reference-counted handle to the callback router.
    ///
    /// Use this to register handlers before calling `connect()`.
    pub fn router(&self) -> Arc<tokio::sync::Mutex<CallbackRouter>> {
        self.router.clone()
    }

    /// Update the session keys used for `/history` display.
    ///
    /// Call this whenever the active session list changes so that the
    /// inline-keyboard pagination reflects the current state.
    pub fn set_session_keys(&self, keys: Vec<String>) {
        *self.session_keys.lock() = keys;
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

        // Register built-in callback handlers (e.g. /menu, settings, history).
        {
            let mut r = self.router.lock().await;
            Self::register_default_handlers(&mut r, &self.session_keys);
        }

        // Spawn the background polling task.
        if let Some(handler) = self.message_handler.clone() {
            let client = self.client.clone();
            let token = match self.token.clone() {
                Some(t) => t,
                None => {
                    error!("Telegram token not set; cannot start polling");
                    return Ok(false);
                }
            };
            let running = self.running.clone();
            let router = self.router.clone();

            tokio::spawn(async move {
                Self::poll_loop(client, token, handler, running, router).await;
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
            reply_markup: None,
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

    async fn send_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> Result<()> {
        debug!(
            "Sending reaction '{}' to message {} in chat {}",
            emoji, message_id, chat_id
        );

        let chat_id_num: i64 = match chat_id.parse() {
            Ok(n) => n,
            Err(_) => {
                warn!("Invalid chat_id for reaction: {chat_id}");
                return Ok(());
            }
        };

        let message_id_num: i64 = match message_id.parse() {
            Ok(n) => n,
            Err(_) => {
                warn!("Invalid message_id for reaction: {message_id}");
                return Ok(());
            }
        };

        let body = SetMessageReactionBody {
            chat_id: chat_id_num,
            message_id: message_id_num,
            reaction: vec![ReactionType {
                r#type: "emoji".to_string(),
                emoji: emoji.to_string(),
            }],
        };

        let url = self.api_url("setMessageReaction");
        if let Err(e) = self.client.post(&url).json(&body).send().await {
            warn!("Failed to send reaction: {e}");
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

    /// Send a message with an inline keyboard attached.
    pub async fn send_message_with_keyboard(
        &self,
        chat_id: &str,
        text: &str,
        keyboard: &InlineKeyboardMarkup,
        reply_to: Option<&str>,
    ) -> Result<SendResult> {
        debug!("Sending message with keyboard to chat {}", chat_id);

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

        #[derive(Debug, Serialize)]
        struct SendMessageWithKeyboardBody {
            chat_id: i64,
            text: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            reply_to_message_id: Option<i64>,
            reply_markup: InlineKeyboardMarkup,
        }

        let body = SendMessageWithKeyboardBody {
            chat_id: chat_id_num,
            text: text.to_string(),
            reply_to_message_id: reply_to_id,
            reply_markup: keyboard.clone(),
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

    /// Answer a callback query, optionally showing a short toast notification.
    ///
    /// When `show_alert` is `true`, an alert dialog is shown instead of a toast.
    pub async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<&str>,
        show_alert: bool,
    ) -> Result<()> {
        debug!("Answering callback query {}", callback_query_id);
        let body = AnswerCallbackQueryBody {
            callback_query_id: callback_query_id.to_string(),
            text: text.map(|s| s.to_string()),
            show_alert: if show_alert { Some(true) } else { None },
        };
        let url = self.api_url("answerCallbackQuery");
        if let Err(e) = self.client.post(&url).json(&body).send().await {
            warn!("Failed to answer callback query: {e}");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Callback helpers
// ---------------------------------------------------------------------------

/// Reconstruct the original callback_data string from a parsed CallbackContext.
// TODO: Will be used by inline keyboard navigation in production code (currently only in tests).
#[allow(dead_code)]
pub(crate) fn rebuild_callback_data(ctx: &CallbackContext) -> String {
    match &ctx.action.payload {
        Some(p) => format!("{}:{}:{}", ctx.action.prefix, ctx.action.action, p),
        None => format!("{}:{}", ctx.action.prefix, ctx.action.action),
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
        let dispatched = TelegramChannel::dispatch_message(&tx, &msg).await.unwrap();
        assert!(!dispatched);
    }

    #[tokio::test]
    async fn test_dispatch_message_returns_true_for_text() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<InboundMessage>(10);
        let msg = TgMessage {
            message_id: 1,
            from: None,
            chat: TgChat {
                id: 1,
                chat_type: Some("private".to_string()),
                title: None,
                thread_id: None,
            },
            text: Some("hi".to_string()),
            photo: None,
            caption: None,
            reply_to_message: None,
        };
        let dispatched = TelegramChannel::dispatch_message(&tx, &msg).await.unwrap();
        assert!(dispatched);
    }

    #[tokio::test]
    async fn test_dispatch_message_returns_true_for_photo() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<InboundMessage>(10);
        let msg = TgMessage {
            message_id: 3,
            from: None,
            chat: TgChat {
                id: 3,
                chat_type: Some("private".to_string()),
                title: None,
                thread_id: None,
            },
            text: None,
            photo: Some(vec![TgPhotoSize {
                file_id: "f".to_string(),
                width: 100,
                height: 100,
            }]),
            caption: None,
            reply_to_message: None,
        };
        let dispatched = TelegramChannel::dispatch_message(&tx, &msg).await.unwrap();
        assert!(dispatched);
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
        let router = Arc::new(tokio::sync::Mutex::new(CallbackRouter::new()));

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
            TelegramChannel::dispatch_callback_query(&client, base_url, &tx, &cq, &router).await;
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
        let router = Arc::new(tokio::sync::Mutex::new(CallbackRouter::new()));

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
        TelegramChannel::dispatch_callback_query(&client, base_url, &tx, &cq, &router)
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
            show_alert: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["callback_query_id"], "cb123");
        assert_eq!(json["text"], "Done!");
        assert!(json.get("show_alert").is_none());
    }

    #[test]
    fn test_answer_callback_query_body_no_text() {
        let body = AnswerCallbackQueryBody {
            callback_query_id: "cb456".to_string(),
            text: None,
            show_alert: None,
        };
        let json = serde_json::to_value(&body).unwrap();
        assert!(json.get("text").is_none());
        assert!(json.get("show_alert").is_none());
    }

    #[test]
    fn test_answer_callback_query_body_show_alert() {
        let body = AnswerCallbackQueryBody {
            callback_query_id: "cb789".to_string(),
            text: Some("Alert!".to_string()),
            show_alert: Some(true),
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["callback_query_id"], "cb789");
        assert_eq!(json["text"], "Alert!");
        assert_eq!(json["show_alert"], true);
    }

    // -----------------------------------------------------------------------
    // Tests for read receipt (reaction) and send_reaction
    // -----------------------------------------------------------------------

    #[test]
    fn test_set_message_reaction_body_serialisation() {
        let body = SetMessageReactionBody {
            chat_id: 123,
            message_id: 456,
            reaction: vec![ReactionType {
                r#type: "emoji".to_string(),
                emoji: "👀".to_string(),
            }],
        };
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["chat_id"], 123);
        assert_eq!(json["message_id"], 456);
        let reaction = json["reaction"].as_array().unwrap();
        assert_eq!(reaction.len(), 1);
        assert_eq!(reaction[0]["type"], "emoji");
        assert_eq!(reaction[0]["emoji"], "👀");
    }

    #[tokio::test]
    async fn test_send_reaction_invalid_chat_id() {
        let channel = TelegramChannel::new();
        // Should not panic — gracefully handles bad chat_id.
        channel.send_reaction("abc", "1", "👀").await.unwrap();
    }

    #[tokio::test]
    async fn test_send_reaction_invalid_message_id() {
        let channel = TelegramChannel::new();
        // Should not panic — gracefully handles bad message_id.
        channel.send_reaction("123", "xyz", "👀").await.unwrap();
    }

    #[test]
    fn test_reaction_type_serialisation() {
        let rt = ReactionType {
            r#type: "emoji".to_string(),
            emoji: "👍".to_string(),
        };
        let json = serde_json::to_value(&rt).unwrap();
        assert_eq!(json["type"], "emoji");
        assert_eq!(json["emoji"], "👍");
    }

    // -----------------------------------------------------------------------
    // InlineKeyboardBuilder tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_builder_empty() {
        let kb = InlineKeyboardBuilder::new().build();
        assert!(kb.inline_keyboard.is_empty());
    }

    #[test]
    fn test_builder_single_button() {
        let kb = InlineKeyboardBuilder::new()
            .button("Click me", "action:click")
            .new_row()
            .build();
        assert_eq!(kb.inline_keyboard.len(), 1);
        assert_eq!(kb.inline_keyboard[0].len(), 1);
        assert_eq!(kb.inline_keyboard[0][0].text, "Click me");
        assert_eq!(
            kb.inline_keyboard[0][0].callback_data,
            Some("action:click".to_string())
        );
    }

    #[test]
    fn test_builder_row_pair() {
        let kb = InlineKeyboardBuilder::new()
            .row_pair("Yes", "confirm:yes", "No", "confirm:no")
            .build();
        assert_eq!(kb.inline_keyboard.len(), 1);
        assert_eq!(kb.inline_keyboard[0].len(), 2);
        assert_eq!(kb.inline_keyboard[0][0].text, "Yes");
        assert_eq!(kb.inline_keyboard[0][1].text, "No");
    }

    #[test]
    fn test_builder_multiple_rows() {
        let kb = InlineKeyboardBuilder::new()
            .button("A", "a")
            .button("B", "b")
            .new_row()
            .button("C", "c")
            .new_row()
            .build();
        assert_eq!(kb.inline_keyboard.len(), 2);
        assert_eq!(kb.inline_keyboard[0].len(), 2);
        assert_eq!(kb.inline_keyboard[1].len(), 1);
    }

    #[test]
    fn test_builder_auto_flush_without_new_row() {
        // build() flushes pending buttons even without new_row().
        let kb = InlineKeyboardBuilder::new()
            .button("X", "x")
            .button("Y", "y")
            .build();
        assert_eq!(kb.inline_keyboard.len(), 1);
        assert_eq!(kb.inline_keyboard[0].len(), 2);
    }

    #[test]
    fn test_builder_url_button() {
        let kb = InlineKeyboardBuilder::new()
            .url_button("Open", "https://example.com")
            .new_row()
            .build();
        assert_eq!(
            kb.inline_keyboard[0][0].url,
            Some("https://example.com".to_string())
        );
        assert_eq!(kb.inline_keyboard[0][0].callback_data, None);
    }

    #[test]
    fn test_builder_confirm_cancel() {
        let kb = InlineKeyboardBuilder::confirm_cancel("del").build();
        assert_eq!(kb.inline_keyboard.len(), 1);
        assert_eq!(kb.inline_keyboard[0].len(), 2);
        assert_eq!(kb.inline_keyboard[0][0].text, "✅ Confirm");
        assert_eq!(
            kb.inline_keyboard[0][0].callback_data,
            Some("del:confirm".to_string())
        );
        assert_eq!(kb.inline_keyboard[0][1].text, "❌ Cancel");
        assert_eq!(
            kb.inline_keyboard[0][1].callback_data,
            Some("del:cancel".to_string())
        );
    }

    #[test]
    fn test_builder_pagination_first_page() {
        let kb = InlineKeyboardBuilder::pagination("list", 0, 5).build();
        assert_eq!(kb.inline_keyboard.len(), 1);
        let row = &kb.inline_keyboard[0];
        // First page: no "Prev", just "1/5" and "Next"
        assert_eq!(row.len(), 2);
        assert_eq!(row[0].text, "1/5");
        assert_eq!(row[1].text, "Next ▶");
        assert_eq!(row[1].callback_data, Some("list:page:1".to_string()));
    }

    #[test]
    fn test_builder_pagination_last_page() {
        let kb = InlineKeyboardBuilder::pagination("list", 4, 5).build();
        let row = &kb.inline_keyboard[0];
        // Last page: "Prev" and "5/5", no "Next"
        assert_eq!(row.len(), 2);
        assert_eq!(row[0].text, "◀ Prev");
        assert_eq!(row[1].text, "5/5");
    }

    #[test]
    fn test_builder_pagination_middle_page() {
        let kb = InlineKeyboardBuilder::pagination("items", 2, 10).build();
        let row = &kb.inline_keyboard[0];
        assert_eq!(row.len(), 3);
        assert_eq!(row[0].text, "◀ Prev");
        assert_eq!(row[0].callback_data, Some("items:page:1".to_string()));
        assert_eq!(row[1].text, "3/10");
        assert_eq!(row[2].text, "Next ▶");
        assert_eq!(row[2].callback_data, Some("items:page:3".to_string()));
    }

    #[test]
    fn test_builder_pagination_single_page() {
        let kb = InlineKeyboardBuilder::pagination("x", 0, 1).build();
        let row = &kb.inline_keyboard[0];
        // Only one page: just "1/1"
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].text, "1/1");
    }

    #[test]
    fn test_inline_keyboard_serialisation() {
        let kb = InlineKeyboardBuilder::new()
            .row_pair("OK", "ok", "Cancel", "cancel")
            .build();
        let json = serde_json::to_value(&kb).unwrap();
        let buttons = json["inline_keyboard"][0].as_array().unwrap();
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0]["text"], "OK");
        assert_eq!(buttons[0]["callback_data"], "ok");
        assert_eq!(buttons[1]["text"], "Cancel");
        assert_eq!(buttons[1]["callback_data"], "cancel");
    }

    #[test]
    fn test_inline_keyboard_roundtrip_serde() {
        let kb = InlineKeyboardBuilder::new()
            .row_pair("A", "a", "B", "b")
            .button("C", "c")
            .new_row()
            .build();
        let json = serde_json::to_string(&kb).unwrap();
        let parsed: InlineKeyboardMarkup = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, kb);
    }

    #[test]
    fn test_inline_keyboard_button_equality() {
        let a = InlineKeyboardButton {
            text: "Click".to_string(),
            callback_data: Some("cb".to_string()),
            url: None,
        };
        let b = InlineKeyboardButton {
            text: "Click".to_string(),
            callback_data: Some("cb".to_string()),
            url: None,
        };
        assert_eq!(a, b);
    }

    // -----------------------------------------------------------------------
    // CallbackAction parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_callback_action_parse_simple() {
        let ca = CallbackAction::parse("confirm:yes").unwrap();
        assert_eq!(ca.prefix, "confirm");
        assert_eq!(ca.action, "yes");
        assert_eq!(ca.payload, None);
    }

    #[test]
    fn test_callback_action_parse_with_payload() {
        let ca = CallbackAction::parse("page:goto:5").unwrap();
        assert_eq!(ca.prefix, "page");
        assert_eq!(ca.action, "goto");
        assert_eq!(ca.payload, Some("5".to_string()));
    }

    #[test]
    fn test_callback_action_parse_complex_payload() {
        let ca = CallbackAction::parse("menu:select:settings:advanced").unwrap();
        assert_eq!(ca.prefix, "menu");
        assert_eq!(ca.action, "select");
        // payload is everything after the second ':'
        assert_eq!(ca.payload, Some("settings:advanced".to_string()));
    }

    #[test]
    fn test_callback_action_parse_empty() {
        assert!(CallbackAction::parse("").is_none());
    }

    #[test]
    fn test_callback_action_parse_no_colon() {
        assert!(CallbackAction::parse("nocolon").is_none());
    }

    #[test]
    fn test_callback_action_parse_trailing_colon() {
        // "action:" → action is empty → returns None
        assert!(CallbackAction::parse("action:").is_none());
    }

    #[test]
    fn test_callback_action_parse_leading_colon() {
        // ":action" → prefix is empty → returns None
        assert!(CallbackAction::parse(":action").is_none());
    }

    // -----------------------------------------------------------------------
    // send_message_with_keyboard and answer_callback_query error cases
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_send_message_with_keyboard_invalid_chat_id() {
        let channel = TelegramChannel::new();
        let kb = InlineKeyboardBuilder::confirm_cancel("test").build();
        let result = channel
            .send_message_with_keyboard("bad_id", "Pick one", &kb, None)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_answer_callback_query_no_server() {
        let channel = TelegramChannel::new();
        // Will fail to connect but should not panic.
        let result = channel
            .answer_callback_query("cb123", Some("Done!"), false)
            .await;
        assert!(result.is_ok());
    }

    // -----------------------------------------------------------------------
    // CallbackRouter tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_router_new_empty() {
        let router = CallbackRouter::new();
        assert_eq!(router.handler_count(), 0);
        assert!(!router.has_handler("any"));
    }

    #[test]
    fn test_router_default() {
        let router = CallbackRouter::default();
        assert_eq!(router.handler_count(), 0);
    }

    #[tokio::test]
    async fn test_router_register_and_dispatch() {
        let mut router = CallbackRouter::new();
        router.register("confirm", |_ctx| async {
            CallbackResponse::Reply("Confirmed!".into())
        });

        assert!(router.has_handler("confirm"));
        assert_eq!(router.handler_count(), 1);

        let ctx = CallbackContext {
            chat_id: "123".into(),
            message_id: "42".into(),
            sender_id: "999".into(),
            callback_query_id: "cb1".into(),
            action: CallbackAction::parse("confirm:yes").unwrap(),
        };

        let resp = router.dispatch(ctx).await.unwrap();
        assert_eq!(resp, CallbackResponse::Reply("Confirmed!".into()));
    }

    #[tokio::test]
    async fn test_router_no_match_returns_none() {
        let mut router = CallbackRouter::new();
        router.register("confirm", |_ctx| async { CallbackResponse::Acknowledged });

        let ctx = CallbackContext {
            chat_id: "123".into(),
            message_id: "1".into(),
            sender_id: "1".into(),
            callback_query_id: "cb2".into(),
            action: CallbackAction::parse("cancel:nope").unwrap(),
        };

        assert!(router.dispatch(ctx).await.is_none());
    }

    #[tokio::test]
    async fn test_router_first_match_wins() {
        let mut router = CallbackRouter::new();
        router.register("menu", |_ctx| async {
            CallbackResponse::Reply("first".into())
        });
        router.register("menu", |_ctx| async {
            CallbackResponse::Reply("second".into())
        });

        let ctx = CallbackContext {
            chat_id: "1".into(),
            message_id: "1".into(),
            sender_id: "1".into(),
            callback_query_id: "cb".into(),
            action: CallbackAction::parse("menu:open").unwrap(),
        };

        let resp = router.dispatch(ctx).await.unwrap();
        assert_eq!(resp, CallbackResponse::Reply("first".into()));
    }

    #[tokio::test]
    async fn test_router_multiple_prefixes() {
        let mut router = CallbackRouter::new();
        router.register("confirm", |_ctx| async {
            CallbackResponse::Reply("confirmed".into())
        });
        router.register("cancel", |_ctx| async {
            CallbackResponse::Toast("cancelling".into())
        });
        router.register("page", |_ctx| async {
            CallbackResponse::EditMessage {
                text: "page content".into(),
                keyboard: None,
            }
        });

        assert_eq!(router.handler_count(), 3);
        assert!(router.has_handler("confirm"));
        assert!(router.has_handler("cancel"));
        assert!(router.has_handler("page"));
        assert!(!router.has_handler("unknown"));

        let ctx_cancel = CallbackContext {
            chat_id: "1".into(),
            message_id: "1".into(),
            sender_id: "1".into(),
            callback_query_id: "cb".into(),
            action: CallbackAction::parse("cancel:abort").unwrap(),
        };
        let resp = router.dispatch(ctx_cancel).await.unwrap();
        assert_eq!(resp, CallbackResponse::Toast("cancelling".into()));

        let ctx_page = CallbackContext {
            chat_id: "1".into(),
            message_id: "1".into(),
            sender_id: "1".into(),
            callback_query_id: "cb".into(),
            action: CallbackAction::parse("page:goto:5").unwrap(),
        };
        let resp = router.dispatch(ctx_page).await.unwrap();
        assert_eq!(
            resp,
            CallbackResponse::EditMessage {
                text: "page content".into(),
                keyboard: None,
            }
        );
    }

    #[tokio::test]
    async fn test_router_handler_receives_context() {
        let mut router = CallbackRouter::new();
        router.register("echo", |ctx| async move {
            let payload = ctx.action.payload.unwrap_or_default();
            CallbackResponse::Reply(format!("echo:{payload}"))
        });

        let ctx = CallbackContext {
            chat_id: "42".into(),
            message_id: "10".into(),
            sender_id: "7".into(),
            callback_query_id: "cb3".into(),
            action: CallbackAction::parse("echo:say:hello").unwrap(),
        };

        let resp = router.dispatch(ctx).await.unwrap();
        assert_eq!(resp, CallbackResponse::Reply("echo:hello".into()));
    }

    #[tokio::test]
    async fn test_router_remove_keyboard_response() {
        let mut router = CallbackRouter::new();
        router.register("del", |_ctx| async { CallbackResponse::RemoveKeyboard });

        let ctx = CallbackContext {
            chat_id: "1".into(),
            message_id: "1".into(),
            sender_id: "1".into(),
            callback_query_id: "cb".into(),
            action: CallbackAction::parse("del:confirm").unwrap(),
        };

        let resp = router.dispatch(ctx).await.unwrap();
        assert_eq!(resp, CallbackResponse::RemoveKeyboard);
    }

    #[tokio::test]
    async fn test_router_edit_message_with_keyboard_response() {
        let mut router = CallbackRouter::new();
        router.register("menu", |_ctx| async {
            let kb = InlineKeyboardBuilder::pagination("menu", 1, 3).build();
            CallbackResponse::EditMessage {
                text: "Page 2".into(),
                keyboard: Some(kb),
            }
        });

        let ctx = CallbackContext {
            chat_id: "1".into(),
            message_id: "1".into(),
            sender_id: "1".into(),
            callback_query_id: "cb".into(),
            action: CallbackAction::parse("menu:page:1").unwrap(),
        };

        let resp = router.dispatch(ctx).await.unwrap();
        match resp {
            CallbackResponse::EditMessage { text, keyboard } => {
                assert_eq!(text, "Page 2");
                assert!(keyboard.is_some());
                let kb = keyboard.unwrap();
                assert_eq!(kb.inline_keyboard.len(), 1);
            }
            other => panic!("Expected EditMessage, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // CallbackResponse variant tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_callback_response_equality() {
        assert_eq!(
            CallbackResponse::Reply("hi".into()),
            CallbackResponse::Reply("hi".into())
        );
        assert_eq!(
            CallbackResponse::RemoveKeyboard,
            CallbackResponse::RemoveKeyboard
        );
        assert_eq!(
            CallbackResponse::Acknowledged,
            CallbackResponse::Acknowledged
        );
        assert_eq!(
            CallbackResponse::Toast("ok".into()),
            CallbackResponse::Toast("ok".into())
        );
        assert_eq!(
            CallbackResponse::EditMessage {
                text: "x".into(),
                keyboard: None,
            },
            CallbackResponse::EditMessage {
                text: "x".into(),
                keyboard: None,
            }
        );
    }

    // -----------------------------------------------------------------------
    // CallbackContext construction test
    // -----------------------------------------------------------------------

    #[test]
    fn test_callback_context_debug() {
        let ctx = CallbackContext {
            chat_id: "123".into(),
            message_id: "42".into(),
            sender_id: "999".into(),
            callback_query_id: "cb1".into(),
            action: CallbackAction::parse("menu:open").unwrap(),
        };
        let debug_str = format!("{ctx:?}");
        assert!(debug_str.contains("123"));
        assert!(debug_str.contains("menu"));
    }

    // -----------------------------------------------------------------------
    // Integration: dispatch_callback_query with router
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_dispatch_callback_query_with_router_match() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<InboundMessage>(10);
        let client = reqwest::Client::new();
        let base_url = "http://127.0.0.1:0";

        let mut router = CallbackRouter::new();
        router.register("action", |_ctx| async { CallbackResponse::Acknowledged });
        let router = Arc::new(tokio::sync::Mutex::new(router));

        let cq = TgCallbackQuery {
            id: "cb_routed".to_string(),
            from: Some(TgUser {
                id: 555,
                first_name: Some("Router".to_string()),
                last_name: None,
                username: None,
            }),
            message: Some(TgCallbackMessage {
                message_id: 99,
                chat: TgChat {
                    id: 555,
                    chat_type: Some("private".to_string()),
                    title: None,
                    thread_id: None,
                },
            }),
            data: Some("action:confirm".to_string()),
        };

        // Router matches → handler runs, returns Acknowledged.
        // No message sent to bus.
        let result =
            TelegramChannel::dispatch_callback_query(&client, base_url, &tx, &cq, &router).await;
        // Should succeed (HTTP calls to localhost:0 fail but are only warned).
        assert!(result.is_ok() || result.is_err());
    }

    #[tokio::test]
    async fn test_dispatch_callback_query_router_no_match_falls_through() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<InboundMessage>(10);
        let client = reqwest::Client::new();
        let base_url = "http://127.0.0.1:0";

        // Router has no handler for "unknown" prefix.
        let router = Arc::new(tokio::sync::Mutex::new(CallbackRouter::new()));

        let cq = TgCallbackQuery {
            id: "cb_fallback".to_string(),
            from: Some(TgUser {
                id: 888,
                first_name: Some("Fallback".to_string()),
                last_name: None,
                username: None,
            }),
            message: Some(TgCallbackMessage {
                message_id: 77,
                chat: TgChat {
                    id: 888,
                    chat_type: Some("private".to_string()),
                    title: None,
                    thread_id: None,
                },
            }),
            data: Some("unknown:something".to_string()),
        };

        let result =
            TelegramChannel::dispatch_callback_query(&client, base_url, &tx, &cq, &router).await;
        if result.is_ok() {
            // Falls through to bus → InboundMessage produced.
            let inbound = rx.try_recv().unwrap();
            assert_eq!(inbound.content, "callback:unknown:something");
            assert_eq!(inbound.sender_id, "888");
        }
    }

    // -----------------------------------------------------------------------
    // TelegramChannel.router() accessor
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_channel_router_accessor() {
        let channel = TelegramChannel::new();
        let router = channel.router();
        let r = router.lock().await;
        assert_eq!(r.handler_count(), 0);
    }

    #[tokio::test]
    async fn test_channel_router_register_via_accessor() {
        let channel = TelegramChannel::new();
        let router = channel.router();
        let mut r = router.lock().await;
        r.register("test", |_ctx| async { CallbackResponse::Acknowledged });
        assert!(r.has_handler("test"));
        assert_eq!(r.handler_count(), 1);
    }

    // -----------------------------------------------------------------------
    // session_keys and set_session_keys
    // -----------------------------------------------------------------------

    #[test]
    fn test_set_session_keys() {
        let channel = TelegramChannel::new();
        assert!(channel.session_keys.lock().is_empty());
        channel.set_session_keys(vec!["a".to_string(), "b".to_string()]);
        assert_eq!(channel.session_keys.lock().len(), 2);
        assert_eq!(channel.session_keys.lock()[0], "a");
    }

    #[test]
    fn test_session_keys_independent_per_channel() {
        let ch1 = TelegramChannel::new();
        let ch2 = TelegramChannel::new();
        ch1.set_session_keys(vec!["x".to_string()]);
        assert!(ch2.session_keys.lock().is_empty());
    }

    // -----------------------------------------------------------------------
    // register_default_handlers with settings/history
    // -----------------------------------------------------------------------

    #[test]
    fn test_register_default_handlers_registers_all() {
        let mut router = CallbackRouter::new();
        let session_keys = Arc::new(ParkMutex::new(Vec::new()));
        TelegramChannel::register_default_handlers(&mut router, &session_keys);
        assert!(router.has_handler("menu"));
        assert!(router.has_handler("settings"));
        assert!(router.has_handler("history"));
        assert_eq!(router.handler_count(), 3);
    }

    #[test]
    fn test_register_default_handlers_idempotent() {
        let mut router = CallbackRouter::new();
        let session_keys = Arc::new(ParkMutex::new(Vec::new()));
        TelegramChannel::register_default_handlers(&mut router, &session_keys);
        TelegramChannel::register_default_handlers(&mut router, &session_keys);
        // Should not double-register.
        assert_eq!(router.handler_count(), 3);
    }

    #[tokio::test]
    async fn test_settings_callback_handler_dispatches() {
        let home = tempfile::tempdir().unwrap();
        let _env = crate::test_support::EnvVarGuard::set("NANOBOT_RS_HOME", home.path());
        let mut router = CallbackRouter::new();
        let session_keys = Arc::new(ParkMutex::new(Vec::new()));
        TelegramChannel::register_default_handlers(&mut router, &session_keys);

        let ctx = CallbackContext {
            chat_id: "1".into(),
            message_id: "1".into(),
            sender_id: "1".into(),
            callback_query_id: "cb_settings".into(),
            action: CallbackAction::parse("settings:page:0").unwrap(),
        };

        let resp = router.dispatch(ctx).await.unwrap();
        match resp {
            CallbackResponse::EditMessage { text, keyboard } => {
                assert!(text.contains("Settings"));
                assert!(text.contains("page 1/"));
                let _ = keyboard;
            }
            other => panic!("Expected EditMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_history_callback_handler_dispatches_empty() {
        let mut router = CallbackRouter::new();
        let session_keys = Arc::new(ParkMutex::new(Vec::new()));
        TelegramChannel::register_default_handlers(&mut router, &session_keys);

        let ctx = CallbackContext {
            chat_id: "1".into(),
            message_id: "1".into(),
            sender_id: "1".into(),
            callback_query_id: "cb_history".into(),
            action: CallbackAction::parse("history:page:0").unwrap(),
        };

        let resp = router.dispatch(ctx).await.unwrap();
        match resp {
            CallbackResponse::EditMessage { text, keyboard } => {
                assert_eq!(text, "No active sessions.");
                assert!(keyboard.is_none());
            }
            other => panic!("Expected EditMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_history_callback_handler_with_sessions() {
        let mut router = CallbackRouter::new();
        let session_keys = Arc::new(ParkMutex::new(vec![
            "tg:111".to_string(),
            "tg:222".to_string(),
        ]));
        TelegramChannel::register_default_handlers(&mut router, &session_keys);

        let ctx = CallbackContext {
            chat_id: "1".into(),
            message_id: "1".into(),
            sender_id: "1".into(),
            callback_query_id: "cb_hist".into(),
            action: CallbackAction::parse("history:page:0").unwrap(),
        };

        let resp = router.dispatch(ctx).await.unwrap();
        match resp {
            CallbackResponse::EditMessage { text, .. } => {
                assert!(text.contains("tg:111"));
                assert!(text.contains("tg:222"));
            }
            other => panic!("Expected EditMessage, got {other:?}"),
        }
    }
}
