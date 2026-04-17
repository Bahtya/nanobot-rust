//! Discord channel adapter — REST API + Gateway WebSocket.
//!
//! Uses the Discord REST API (v10) for sending messages, typing indicators,
//! and images. Receives inbound messages via the Gateway WebSocket with
//! proper HELLO / HEARTBEAT / IDENTIFY / RESUME handshake.
//!
//! ## Reconnection and RESUME
//!
//! When the gateway disconnects (opcode 7 RECONNECT, connection drop, or
//! opcode 9 INVALID_SESSION with `d: true`), the adapter preserves its
//! `session_id` and `last_seq` and sends a RESUME (opcode 6) payload on
//! reconnection instead of a fresh IDENTIFY. This allows Discord to replay
//! missed events and avoids the bot appearing offline briefly.
//!
//! On INVALID_SESSION with `d: false`, session state is cleared and a fresh
//! IDENTIFY is sent on the next connection attempt.

use anyhow::{Context, Result};
use async_trait::async_trait;
use kestrel_bus::events::{AgentEvent, InboundMessage};
use kestrel_core::{MessageType, Platform, SessionSource};
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

/// Maximum number of reconnection attempts before giving up.
const MAX_RECONNECT_ATTEMPTS: u32 = 100;
/// Maximum exponential backoff in seconds.
const MAX_BACKOFF_SECS: u64 = 60;

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
mod opcodes {
    pub const DISPATCH: i64 = 0;
    pub const HEARTBEAT: i64 = 1;
    pub const IDENTIFY: i64 = 2;
    pub const RESUME: i64 = 6;
    pub const RECONNECT: i64 = 7;
    pub const INVALID_SESSION: i64 = 9;
    pub const HELLO: i64 = 10;
    pub const HEARTBEAT_ACK: i64 = 11;
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
    pub const TEXT_BOT: i64 = GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT;
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

/// The READY dispatch data — we only need `session_id`.
#[derive(Debug, Deserialize)]
struct ReadyData {
    session_id: String,
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
// Gateway session state
// ---------------------------------------------------------------------------

/// Tracks the resumable gateway session state across reconnections.
///
/// After a successful IDENTIFY, the READY event provides a `session_id`.
/// Every dispatch event increments the sequence number (`s` field).  When
/// the connection drops, we preserve these values and send a RESUME payload
/// instead of IDENTIFY on the next attempt, allowing Discord to replay any
/// missed events.
#[derive(Debug, Clone, Default)]
struct GatewaySessionState {
    session_id: Option<String>,
    last_seq: Option<i64>,
}

impl GatewaySessionState {
    /// Whether we have a session that can be resumed.
    fn has_session(&self) -> bool {
        self.session_id.is_some()
    }

    /// Clear all session state (used after non-resumable INVALID_SESSION).
    fn clear(&mut self) {
        self.session_id = None;
        self.last_seq = None;
    }

    /// Save the session ID from a READY dispatch.
    fn save_from_ready(&mut self, session_id: String) {
        self.session_id = Some(session_id);
    }

    /// Update the tracked sequence number (only when a new one is provided).
    fn update_seq(&mut self, seq: Option<i64>) {
        if seq.is_some() {
            self.last_seq = seq;
        }
    }
}

// ---------------------------------------------------------------------------
// Session outcome
// ---------------------------------------------------------------------------

/// Result of a single gateway session attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionOutcome {
    /// Disconnected (or RECONNECT op 7) — can resume with saved state.
    ResumableDisconnect,
    /// INVALID_SESSION with `d: true` — keep session state and try RESUME.
    InvalidSessionResumable,
    /// INVALID_SESSION with `d: false` — clear state and IDENTIFY fresh.
    InvalidSessionNotResumable,
    /// Clean shutdown requested (`running` flag cleared).
    Shutdown,
    /// Fatal protocol error — don't reconnect.
    Fatal,
}

// ---------------------------------------------------------------------------
// Payload builders (pure functions, easy to test)
// ---------------------------------------------------------------------------

/// Build a RESUME (opcode 6) payload.
fn build_resume_payload(token: &str, session_id: &str, seq: i64) -> serde_json::Value {
    serde_json::json!({
        "op": opcodes::RESUME,
        "d": {
            "token": token,
            "session_id": session_id,
            "seq": seq
        }
    })
}

/// Build an IDENTIFY (opcode 2) payload.
fn build_identify_payload(token: &str) -> serde_json::Value {
    serde_json::json!({
        "op": opcodes::IDENTIFY,
        "d": {
            "token": token,
            "intents": intents::TEXT_BOT,
            "properties": {
                "os": "linux",
                "browser": "kestrel",
                "device": "kestrel"
            }
        }
    })
}

/// Calculate the next backoff duration in seconds.
///
/// Doubles the current backoff, capped at `MAX_BACKOFF_SECS`.
fn next_backoff(current_secs: u64) -> u64 {
    (current_secs * 2).min(MAX_BACKOFF_SECS)
}

/// Percent-encode an emoji string for use in a Discord API URL path.
///
/// Discord requires the emoji in reaction URLs to be percent-encoded.
/// For example `✅` (UTF-8: `E2 9C 85`) becomes `%E2%9C%85`.
fn percent_encode_emoji(emoji: &str) -> String {
    let mut encoded = String::with_capacity(emoji.len() * 3);
    for byte in emoji.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    encoded
}

// ---------------------------------------------------------------------------
// DiscordChannel
// ---------------------------------------------------------------------------

/// REST API context passed into the gateway session for read-receipt calls.
struct RestContext {
    client: reqwest::Client,
    api_base: String,
    auth_header: String,
}

/// Discord channel implementation using REST API + Gateway WebSocket.
pub struct DiscordChannel {
    token: Option<String>,
    connected: bool,
    message_handler: Option<tokio::sync::mpsc::Sender<InboundMessage>>,
    event_tx: Option<tokio::sync::broadcast::Sender<AgentEvent>>,
    client: reqwest::Client,
    /// Override base URL for testing.
    base_url_override: Option<String>,
    /// Override gateway URL for testing.
    gateway_url_override: Option<String>,
    /// Running flag for the gateway listener.
    running: Arc<AtomicBool>,
    /// Proxy URL stored for future client rebuild support.
    #[allow(dead_code)]
    proxy_config: Option<String>,
}

impl DiscordChannel {
    /// Build a reqwest client with config-driven proxy support.
    ///
    /// Priority: `proxy_config` > env vars (`HTTPS_PROXY`, `ALL_PROXY`, etc.) > direct.
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

        match proxy_url {
            Some(ref url) if url.starts_with("socks5") => {
                info!("Discord HTTP client using SOCKS5 proxy: {}", url);
                let proxy =
                    reqwest::Proxy::all(url).expect("Failed to create SOCKS5 proxy from config");
                reqwest::Client::builder()
                    .proxy(proxy)
                    .build()
                    .expect("Failed to build HTTP client with SOCKS5 proxy")
            }
            Some(ref url) if url.starts_with("http") => {
                info!("Discord HTTP client using HTTP proxy: {}", url);
                let http_proxy =
                    reqwest::Proxy::http(url).expect("Failed to create HTTP proxy from config");
                let https_proxy =
                    reqwest::Proxy::https(url).expect("Failed to create HTTPS proxy from config");
                reqwest::Client::builder()
                    .proxy(http_proxy)
                    .proxy(https_proxy)
                    .build()
                    .expect("Failed to build HTTP client with HTTP proxy")
            }
            Some(ref url) => {
                info!(
                    "Discord HTTP client: unsupported proxy scheme in '{}', falling back to direct",
                    url
                );
                reqwest::Client::new()
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
            event_tx: None,
            client: Self::build_client(None),
            base_url_override: None,
            gateway_url_override: None,
            running: Arc::new(AtomicBool::new(false)),
            proxy_config: None,
        }
    }

    /// Create a new `DiscordChannel` using a Discord config for token and proxy.
    ///
    /// Reads the bot token from the config (falling back to the
    /// `DISCORD_BOT_TOKEN` env var) and configures the HTTP client proxy
    /// accordingly.
    pub fn new_with_config(config: &kestrel_config::schema::DiscordConfig) -> Self {
        let token = if config.token.is_empty() {
            std::env::var("DISCORD_BOT_TOKEN").ok()
        } else {
            Some(config.token.clone())
        };
        let proxy = config.proxy.as_deref().filter(|s| !s.is_empty());
        Self {
            token,
            connected: false,
            message_handler: None,
            event_tx: None,
            client: Self::build_client(proxy),
            base_url_override: None,
            gateway_url_override: None,
            running: Arc::new(AtomicBool::new(false)),
            proxy_config: proxy.map(|s| s.to_string()),
        }
    }

    /// Create with a custom token and base URL (for testing).
    /// Skips proxy since the base URL is typically localhost in tests.
    pub fn with_token_and_url(token: String, base_url: String) -> Self {
        Self {
            token: Some(token),
            connected: false,
            message_handler: None,
            event_tx: None,
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base_url_override: Some(base_url),
            gateway_url_override: None,
            running: Arc::new(AtomicBool::new(false)),
            proxy_config: None,
        }
    }

    /// Set the event sender for gateway lifecycle events.
    pub fn set_event_sender(&mut self, tx: tokio::sync::broadcast::Sender<AgentEvent>) {
        self.event_tx = Some(tx);
    }

    /// Return the effective API base URL.
    fn api_base(&self) -> &str {
        self.base_url_override
            .as_deref()
            .unwrap_or(DISCORD_API_BASE)
    }

    /// Return the effective gateway WebSocket URL.
    fn gateway_url(&self) -> &str {
        self.gateway_url_override
            .as_deref()
            .unwrap_or(DISCORD_GATEWAY_URL)
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

    // -- Gateway lifecycle -----------------------------------------------------

    /// Send a ✅ read-receipt reaction to a Discord message.
    ///
    /// Uses `PUT /channels/{channel_id}/messages/{message_id}/reactions/%E2%9C%85/@me`.
    /// Non-critical: failures are logged but not propagated.
    async fn send_read_receipt(
        client: &reqwest::Client,
        api_base: &str,
        auth_header: &str,
        channel_id: &str,
        message_id: &str,
    ) {
        let path = format!(
            "/channels/{}/messages/{}/reactions/%E2%9C%85/@me",
            channel_id, message_id
        );
        let url = format!("{}{}", api_base, path);
        match client
            .put(&url)
            .header("Authorization", auth_header)
            .send()
            .await
        {
            Ok(r) if r.status() == StatusCode::NO_CONTENT => {
                debug!("Discord read receipt sent");
            }
            Ok(r) => {
                debug!("Discord read receipt non-204 status: {}", r.status());
            }
            Err(e) => {
                debug!("Discord read receipt request failed: {e}");
            }
        }
    }

    /// Send a text reply directly via the Discord REST API (bypassing the bus).
    ///
    /// Used for built-in commands like `/validate` that must work even when
    /// the LLM provider is down.  Non-critical: failures are logged only.
    async fn send_direct_reply(
        client: &reqwest::Client,
        api_base: &str,
        auth_header: &str,
        channel_id: &str,
        text: &str,
    ) {
        let path = format!("/channels/{channel_id}/messages");
        let url = format!("{}{}", api_base, path);
        let body = CreateMessageBody {
            content: text.to_string(),
            message_reference: None,
            embed: None,
        };
        match client
            .post(&url)
            .header("Authorization", auth_header)
            .json(&body)
            .send()
            .await
        {
            Ok(_) => debug!("Discord direct reply sent"),
            Err(e) => warn!("Failed to send direct reply: {e}"),
        }
    }

    /// Emit a gateway lifecycle event (if an event sender is configured).
    fn emit_gateway_event(
        event_tx: &Option<tokio::sync::broadcast::Sender<AgentEvent>>,
        event: AgentEvent,
    ) {
        if let Some(tx) = event_tx {
            let _ = tx.send(event);
        }
    }

    /// Run the Gateway WebSocket listener with automatic reconnection and RESUME.
    ///
    /// On disconnect or RECONNECT request, reconnects with exponential
    /// backoff (1s → 2s → 4s → … → 60s max). Resets backoff on successful
    /// READY or RESUMED.
    ///
    /// If a valid `session_id` is available, sends RESUME instead of IDENTIFY
    /// to preserve the session and replay missed events.
    async fn run_gateway(
        token: String,
        handler: tokio::sync::mpsc::Sender<InboundMessage>,
        running: Arc<AtomicBool>,
        event_tx: Option<tokio::sync::broadcast::Sender<AgentEvent>>,
        gateway_url: String,
        rest: RestContext,
    ) {
        let mut state = GatewaySessionState::default();
        let mut backoff_secs: u64 = 1;
        let mut attempt: u32 = 0;

        while running.load(Ordering::Relaxed) && attempt < MAX_RECONNECT_ATTEMPTS {
            let outcome = Self::run_gateway_session(
                &token,
                &handler,
                &running,
                &mut state,
                &event_tx,
                &gateway_url,
                &rest,
            )
            .await;

            if !running.load(Ordering::Relaxed) {
                break;
            }

            match outcome {
                SessionOutcome::ResumableDisconnect => {
                    attempt += 1;
                    Self::emit_gateway_event(
                        &event_tx,
                        AgentEvent::GatewayReconnecting {
                            platform: "discord".to_string(),
                            attempt,
                            resumable: state.has_session(),
                        },
                    );
                    warn!(
                        "Discord Gateway disconnected — reconnecting in {}s (attempt {})",
                        backoff_secs, attempt
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = next_backoff(backoff_secs);
                }
                SessionOutcome::InvalidSessionResumable => {
                    attempt += 1;
                    Self::emit_gateway_event(
                        &event_tx,
                        AgentEvent::GatewayReconnecting {
                            platform: "discord".to_string(),
                            attempt,
                            resumable: true,
                        },
                    );
                    // Discord docs: wait 1-5 seconds before resuming
                    let delay = 1 + ((attempt as u64) % 5);
                    warn!(
                        "Discord Gateway INVALID_SESSION (resumable) — retrying in {}s",
                        delay
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                }
                SessionOutcome::InvalidSessionNotResumable => {
                    state.clear();
                    attempt += 1;
                    Self::emit_gateway_event(
                        &event_tx,
                        AgentEvent::GatewayReconnecting {
                            platform: "discord".to_string(),
                            attempt,
                            resumable: false,
                        },
                    );
                    warn!(
                        "Discord Gateway INVALID_SESSION (not resumable) — fresh identify in {}s",
                        backoff_secs
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                    backoff_secs = next_backoff(backoff_secs);
                }
                SessionOutcome::Shutdown | SessionOutcome::Fatal => break,
            }
        }

        info!("Discord Gateway listener stopped ({} attempts)", attempt);
    }

    /// Run a single Gateway session (connect → handshake → process → disconnect).
    ///
    /// Returns a [`SessionOutcome`] indicating what the outer loop should do next.
    async fn run_gateway_session(
        token: &str,
        handler: &tokio::sync::mpsc::Sender<InboundMessage>,
        running: &Arc<AtomicBool>,
        state: &mut GatewaySessionState,
        event_tx: &Option<tokio::sync::broadcast::Sender<AgentEvent>>,
        gateway_url: &str,
        rest: &RestContext,
    ) -> SessionOutcome {
        use futures::SinkExt;
        use futures::StreamExt;
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        info!("Discord Gateway connecting...");

        let connect_result = tokio_tungstenite::connect_async(gateway_url).await;
        let (mut ws, _) = match connect_result {
            Ok(c) => c,
            Err(e) => {
                error!("Discord Gateway WebSocket connect failed: {}", e);
                return SessionOutcome::ResumableDisconnect;
            }
        };

        // ── Wait for HELLO ──────────────────────────────────────────────
        let heartbeat_interval = loop {
            let msg = match ws.next().await {
                Some(Ok(WsMessage::Text(text))) => text,
                Some(Ok(WsMessage::Close(_))) => {
                    info!("Discord Gateway closed during HELLO");
                    return SessionOutcome::ResumableDisconnect;
                }
                Some(Err(e)) => {
                    error!("Discord Gateway read error during HELLO: {}", e);
                    return SessionOutcome::ResumableDisconnect;
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
                        return SessionOutcome::Fatal;
                    }
                };
                break hello.heartbeat_interval;
            }
        };

        info!(
            "Discord Gateway HELLO received (heartbeat: {}ms)",
            heartbeat_interval
        );

        // ── Send RESUME or IDENTIFY ─────────────────────────────────────
        if state.has_session() {
            let session_id = match state.session_id.as_ref() {
                Some(id) => id,
                None => {
                    error!("Discord Gateway: has_session() is true but session_id is None");
                    return SessionOutcome::ResumableDisconnect;
                }
            };
            let seq = state.last_seq.unwrap_or(0);
            let resume = build_resume_payload(token, session_id, seq);
            if let Err(e) = ws.send(WsMessage::Text(resume.to_string().into())).await {
                error!("Failed to send RESUME: {}", e);
                return SessionOutcome::ResumableDisconnect;
            }
            info!(
                "Discord Gateway RESUME sent (session_id: {}, seq: {})",
                session_id, seq
            );
        } else {
            let identify = build_identify_payload(token);
            if let Err(e) = ws.send(WsMessage::Text(identify.to_string().into())).await {
                error!("Failed to send IDENTIFY: {}", e);
                return SessionOutcome::ResumableDisconnect;
            }
            info!(
                "Discord Gateway IDENTIFY sent (intents: {})",
                intents::TEXT_BOT
            );
            Self::emit_gateway_event(
                event_tx,
                AgentEvent::GatewayReidentify {
                    platform: "discord".to_string(),
                },
            );
        }

        // ── Process events ──────────────────────────────────────────────
        let mut heartbeat_interval_tick =
            tokio::time::interval(std::time::Duration::from_millis(heartbeat_interval));

        while running.load(Ordering::Relaxed) {
            tokio::select! {
                _ = heartbeat_interval_tick.tick() => {
                    let hb = serde_json::json!({
                        "op": opcodes::HEARTBEAT,
                        "d": state.last_seq
                    });
                    if let Err(e) = ws.send(WsMessage::Text(hb.to_string().into())).await {
                        error!("Discord heartbeat send failed: {}", e);
                        break;
                    }
                    debug!("Discord heartbeat sent (seq: {:?})", state.last_seq);
                }
                msg = ws.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
                            let payload: GatewayPayload = match serde_json::from_str(&text) {
                                Ok(p) => p,
                                Err(_) => continue,
                            };

                            state.update_seq(payload.s);

                            match payload.op {
                                opcodes::DISPATCH => {
                                    match payload.t.as_deref() {
                                        Some("READY") => {
                                            if let Ok(ready) =
                                                serde_json::from_value::<ReadyData>(payload.d)
                                            {
                                                info!(
                                                    "Discord Gateway READY (session_id: {})",
                                                    ready.session_id
                                                );
                                                state.save_from_ready(ready.session_id);
                                            }
                                        }
                                        Some("RESUMED") => {
                                            Self::emit_gateway_event(
                                                event_tx,
                                                AgentEvent::GatewayResumed {
                                                    platform: "discord".to_string(),
                                                    session_id: state
                                                        .session_id
                                                        .clone()
                                                        .unwrap_or_default(),
                                                },
                                            );
                                            info!("Discord Gateway RESUMED");
                                        }
                                        Some("MESSAGE_CREATE") | Some("MESSAGE_UPDATE") => {
                                            if let Ok(msg_data) =
                                                serde_json::from_value::<GatewayMessage>(payload.d)
                                            {
                                                // /reset needs the session key, so handle it separately.
                                                if crate::commands::matches_command(
                                                    &msg_data.content,
                                                    "reset",
                                                ) {
                                                    let session_key =
                                                        format!("discord:{}", msg_data.channel_id);
                                                    let response =
                                                        crate::commands::handle_reset(&session_key);
                                                    Self::send_direct_reply(
                                                        &rest.client,
                                                        &rest.api_base,
                                                        &rest.auth_header,
                                                        &msg_data.channel_id,
                                                        &response,
                                                    )
                                                    .await;
                                                    Self::send_read_receipt(
                                                        &rest.client,
                                                        &rest.api_base,
                                                        &rest.auth_header,
                                                        &msg_data.channel_id,
                                                        &msg_data.id,
                                                    )
                                                    .await;
                                                } else if let Some(response) =
                                                    crate::commands::try_handle_command(
                                                        &msg_data.content,
                                                    )
                                                    .await
                                                {
                                                    Self::send_direct_reply(
                                                        &rest.client,
                                                        &rest.api_base,
                                                        &rest.auth_header,
                                                        &msg_data.channel_id,
                                                        &response.text,
                                                    )
                                                    .await;
                                                    Self::send_read_receipt(
                                                        &rest.client,
                                                        &rest.api_base,
                                                        &rest.auth_header,
                                                        &msg_data.channel_id,
                                                        &msg_data.id,
                                                    )
                                                    .await;
                                                } else {
                                                    let dispatched =
                                                        Self::dispatch_message(handler, &msg_data)
                                                            .await;
                                                    if dispatched {
                                                        Self::send_read_receipt(
                                                            &rest.client,
                                                            &rest.api_base,
                                                            &rest.auth_header,
                                                            &msg_data.channel_id,
                                                            &msg_data.id,
                                                        )
                                                        .await;
                                                    }
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                opcodes::HEARTBEAT_ACK => {
                                    debug!("Discord heartbeat ACK");
                                }
                                opcodes::INVALID_SESSION => {
                                    let resumable = payload.d.as_bool().unwrap_or(false);
                                    if resumable {
                                        warn!(
                                            "Discord Gateway INVALID_SESSION — resumable"
                                        );
                                        return SessionOutcome::InvalidSessionResumable;
                                    } else {
                                        warn!(
                                            "Discord Gateway INVALID_SESSION — not resumable"
                                        );
                                        return SessionOutcome::InvalidSessionNotResumable;
                                    }
                                }
                                opcodes::RECONNECT => {
                                    warn!("Discord Gateway RECONNECT requested");
                                    return SessionOutcome::ResumableDisconnect;
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

        // Loop ended — if still running, try to resume
        if running.load(Ordering::Relaxed) {
            SessionOutcome::ResumableDisconnect
        } else {
            SessionOutcome::Shutdown
        }
    }

    /// Convert a Gateway message to an InboundMessage and send it.
    ///
    /// Returns `true` if the message was dispatched, `false` if skipped
    /// (empty content).
    async fn dispatch_message(
        handler: &tokio::sync::mpsc::Sender<InboundMessage>,
        msg: &GatewayMessage,
    ) -> bool {
        // Skip empty messages
        if msg.content.is_empty() {
            return false;
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
        true
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
            let token = match self.token.clone() {
                Some(t) => t,
                None => {
                    error!("Discord token not set; cannot start gateway listener");
                    return Ok(false);
                }
            };
            let running = self.running.clone();
            let event_tx = self.event_tx.clone();
            let gateway_url = self.gateway_url().to_string();
            let rest = RestContext {
                client: self.client.clone(),
                api_base: self.api_base().to_string(),
                auth_header: format!("Bot {token}"),
            };
            tokio::spawn(async move {
                Self::run_gateway(token, handler, running, event_tx, gateway_url, rest).await;
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

    async fn send_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> Result<()> {
        let encoded = percent_encode_emoji(emoji);
        let path = format!("/channels/{chat_id}/messages/{message_id}/reactions/{encoded}/@me");
        let url = format!("{}{}", self.api_base(), path);
        let mut req = self.client.put(&url);
        if let Some(auth) = self.auth_header() {
            req = req.header("Authorization", auth);
        }

        let resp = req
            .send()
            .await
            .context("Failed to send Discord reaction")?;

        let status = resp.status();
        if status != StatusCode::NO_CONTENT {
            let text = resp.text().await.unwrap_or_default();
            warn!(
                status = %status,
                body = %text,
                "Discord reaction failed (non-fatal)"
            );
        } else {
            debug!("Discord reaction '{}' sent", emoji);
        }

        Ok(())
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
    pub async fn delete_message(&self, channel_id: &str, message_id: &str) -> Result<SendResult> {
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

    // -----------------------------------------------------------------------
    // Existing tests (unchanged)
    // -----------------------------------------------------------------------

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
        assert_eq!(opcodes::RESUME, 6);
        assert_eq!(opcodes::RECONNECT, 7);
        assert_eq!(opcodes::INVALID_SESSION, 9);
        assert_eq!(opcodes::HELLO, 10);
        assert_eq!(opcodes::HEARTBEAT_ACK, 11);
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
        assert_eq!(
            json["embed"]["image"]["url"],
            "https://example.com/new_img.png"
        );
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
        assert_eq!(payload.d.as_bool(), Some(true));
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
        let result =
            DiscordChannel::make_error_result(StatusCode::SERVICE_UNAVAILABLE, "overloaded");
        assert!(!result.success);
        assert!(result.retryable);
    }

    // ===================================================================
    // GatewaySessionState tests
    // ===================================================================

    #[test]
    fn test_session_state_default_empty() {
        let state = GatewaySessionState::default();
        assert!(state.session_id.is_none());
        assert!(state.last_seq.is_none());
        assert!(!state.has_session());
    }

    #[test]
    fn test_session_state_save_from_ready() {
        let mut state = GatewaySessionState::default();
        state.save_from_ready("sess_abc123".to_string());
        assert!(state.has_session());
        assert_eq!(state.session_id.as_deref(), Some("sess_abc123"));
    }

    #[test]
    fn test_session_state_update_seq() {
        let mut state = GatewaySessionState::default();
        state.update_seq(Some(42));
        assert_eq!(state.last_seq, Some(42));
        state.update_seq(Some(43));
        assert_eq!(state.last_seq, Some(43));
    }

    #[test]
    fn test_session_state_update_seq_ignores_none() {
        let mut state = GatewaySessionState::default();
        state.update_seq(Some(10));
        state.update_seq(None);
        assert_eq!(state.last_seq, Some(10));
    }

    #[test]
    fn test_session_state_clear() {
        let mut state = GatewaySessionState::default();
        state.save_from_ready("sess_xyz".to_string());
        state.update_seq(Some(99));
        assert!(state.has_session());

        state.clear();
        assert!(!state.has_session());
        assert!(state.session_id.is_none());
        assert!(state.last_seq.is_none());
    }

    #[test]
    fn test_session_state_has_session_requires_id() {
        let mut state = GatewaySessionState::default();
        state.update_seq(Some(1));
        assert!(!state.has_session()); // no session_id yet

        state.save_from_ready("sess_1".to_string());
        assert!(state.has_session());
    }

    // ===================================================================
    // Payload builder tests
    // ===================================================================

    #[test]
    fn test_build_identify_payload() {
        let payload = build_identify_payload("my-bot-token");
        assert_eq!(payload["op"], opcodes::IDENTIFY);
        assert_eq!(payload["d"]["token"], "my-bot-token");
        assert_eq!(payload["d"]["intents"], intents::TEXT_BOT);
        assert_eq!(payload["d"]["properties"]["os"], "linux");
        assert_eq!(payload["d"]["properties"]["browser"], "kestrel");
        assert_eq!(payload["d"]["properties"]["device"], "kestrel");
    }

    #[test]
    fn test_build_resume_payload() {
        let payload = build_resume_payload("my-bot-token", "sess_abc", 42);
        assert_eq!(payload["op"], opcodes::RESUME);
        assert_eq!(payload["d"]["token"], "my-bot-token");
        assert_eq!(payload["d"]["session_id"], "sess_abc");
        assert_eq!(payload["d"]["seq"], 42);
    }

    #[test]
    fn test_resume_payload_is_valid_json() {
        let payload = build_resume_payload("tok", "sid", 0);
        let serialized = payload.to_string();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed["op"], 6);
        assert_eq!(parsed["d"]["token"], "tok");
    }

    // ===================================================================
    // Backoff calculation tests
    // ===================================================================

    #[test]
    fn test_backoff_doubles() {
        assert_eq!(next_backoff(1), 2);
        assert_eq!(next_backoff(2), 4);
        assert_eq!(next_backoff(4), 8);
        assert_eq!(next_backoff(8), 16);
        assert_eq!(next_backoff(16), 32);
        assert_eq!(next_backoff(32), 60); // capped
    }

    #[test]
    fn test_backoff_capped_at_max() {
        assert_eq!(next_backoff(30), 60);
        assert_eq!(next_backoff(60), 60);
        assert_eq!(next_backoff(120), 60);
    }

    #[test]
    fn test_backoff_sequence() {
        let mut backoff = 1u64;
        let sequence: Vec<u64> = (0..10)
            .map(|_| {
                let current = backoff;
                backoff = next_backoff(backoff);
                current
            })
            .collect();
        assert_eq!(sequence, vec![1, 2, 4, 8, 16, 32, 60, 60, 60, 60]);
    }

    // ===================================================================
    // SessionOutcome tests
    // ===================================================================

    #[test]
    fn test_session_outcome_variants() {
        // Verify all variants exist and can be compared
        assert_eq!(
            SessionOutcome::ResumableDisconnect,
            SessionOutcome::ResumableDisconnect
        );
        assert_eq!(
            SessionOutcome::InvalidSessionResumable,
            SessionOutcome::InvalidSessionResumable
        );
        assert_eq!(
            SessionOutcome::InvalidSessionNotResumable,
            SessionOutcome::InvalidSessionNotResumable
        );
        assert_eq!(SessionOutcome::Shutdown, SessionOutcome::Shutdown);
        assert_eq!(SessionOutcome::Fatal, SessionOutcome::Fatal);

        assert_ne!(SessionOutcome::ResumableDisconnect, SessionOutcome::Fatal);
    }

    #[test]
    fn test_session_outcome_invalid_session_data_true() {
        let json = r#"{"op":9,"d":true}"#;
        let payload: GatewayPayload = serde_json::from_str(json).unwrap();
        let resumable = payload.d.as_bool().unwrap_or(false);
        assert!(resumable);
        // Should map to InvalidSessionResumable
    }

    #[test]
    fn test_session_outcome_invalid_session_data_false() {
        let json = r#"{"op":9,"d":false}"#;
        let payload: GatewayPayload = serde_json::from_str(json).unwrap();
        let resumable = payload.d.as_bool().unwrap_or(false);
        assert!(!resumable);
        // Should map to InvalidSessionNotResumable
    }

    #[test]
    fn test_session_outcome_invalid_session_data_null() {
        let json = r#"{"op":9,"d":null}"#;
        let payload: GatewayPayload = serde_json::from_str(json).unwrap();
        let resumable = payload.d.as_bool().unwrap_or(false);
        assert!(!resumable);
        // null → default to not resumable
    }

    // ===================================================================
    // Ready dispatch parsing
    // ===================================================================

    #[test]
    fn test_ready_data_parse() {
        let json = r#"{"session_id":"abc123def456","user":{"id":"111"},"guilds":[]}"#;
        let ready: ReadyData = serde_json::from_str(json).unwrap();
        assert_eq!(ready.session_id, "abc123def456");
    }

    #[test]
    fn test_ready_dispatch_full_payload() {
        let json = r#"{
            "op":0,
            "s":1,
            "t":"READY",
            "d":{"session_id":"sess_ready_1","user":{"id":"bot1"},"guilds":[]}
        }"#;
        let payload: GatewayPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.op, opcodes::DISPATCH);
        assert_eq!(payload.t.as_deref(), Some("READY"));

        let ready: ReadyData = serde_json::from_value(payload.d).unwrap();
        assert_eq!(ready.session_id, "sess_ready_1");
    }

    #[test]
    fn test_resumed_dispatch_payload() {
        let json = r#"{"op":0,"s":42,"t":"RESUMED","d":{}}"#;
        let payload: GatewayPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.op, opcodes::DISPATCH);
        assert_eq!(payload.t.as_deref(), Some("RESUMED"));
        assert_eq!(payload.s, Some(42));
    }

    // ===================================================================
    // Event emission tests
    // ===================================================================

    #[test]
    fn test_emit_gateway_event_with_sender() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let event_tx = Some(tx);

        DiscordChannel::emit_gateway_event(
            &event_tx,
            AgentEvent::GatewayReconnecting {
                platform: "discord".to_string(),
                attempt: 1,
                resumable: true,
            },
        );

        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::GatewayReconnecting {
                platform,
                attempt,
                resumable,
            } => {
                assert_eq!(platform, "discord");
                assert_eq!(attempt, 1);
                assert!(resumable);
            }
            _ => panic!("Expected GatewayReconnecting event"),
        }
    }

    #[test]
    fn test_emit_gateway_event_without_sender() {
        let event_tx: Option<tokio::sync::broadcast::Sender<AgentEvent>> = None;
        // Should not panic
        DiscordChannel::emit_gateway_event(
            &event_tx,
            AgentEvent::GatewayResumed {
                platform: "discord".to_string(),
                session_id: "test".to_string(),
            },
        );
    }

    #[test]
    fn test_emit_gateway_event_resumed() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);

        DiscordChannel::emit_gateway_event(
            &Some(tx),
            AgentEvent::GatewayResumed {
                platform: "discord".to_string(),
                session_id: "sess_xyz".to_string(),
            },
        );

        let event = rx.try_recv().unwrap();
        match event {
            AgentEvent::GatewayResumed {
                platform,
                session_id,
            } => {
                assert_eq!(platform, "discord");
                assert_eq!(session_id, "sess_xyz");
            }
            _ => panic!("Expected GatewayResumed event"),
        }
    }

    #[test]
    fn test_emit_gateway_event_reidentify() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);

        DiscordChannel::emit_gateway_event(
            &Some(tx),
            AgentEvent::GatewayReidentify {
                platform: "discord".to_string(),
            },
        );

        let event = rx.try_recv().unwrap();
        assert!(matches!(event, AgentEvent::GatewayReidentify { .. }));
    }

    // ===================================================================
    // Max reconnect attempts test
    // ===================================================================

    #[test]
    fn test_max_reconnect_attempts_constant() {
        assert_eq!(MAX_RECONNECT_ATTEMPTS, 100);
    }

    #[test]
    fn test_max_backoff_constant() {
        assert_eq!(MAX_BACKOFF_SECS, 60);
    }

    // ===================================================================
    // DiscordChannel set_event_sender test
    // ===================================================================

    #[test]
    fn test_set_event_sender() {
        let mut channel = DiscordChannel::new();
        assert!(channel.event_tx.is_none());

        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        channel.set_event_sender(tx);
        assert!(channel.event_tx.is_some());
    }

    // ===================================================================
    // Gateway URL override test
    // ===================================================================

    #[test]
    fn test_gateway_url_default() {
        let channel = DiscordChannel::new();
        assert_eq!(channel.gateway_url(), DISCORD_GATEWAY_URL);
    }

    #[test]
    fn test_gateway_url_override() {
        let mut channel = DiscordChannel::new();
        channel.gateway_url_override = Some("ws://localhost:9999".to_string());
        assert_eq!(channel.gateway_url(), "ws://localhost:9999");
    }

    // ===================================================================
    // Full RESUME scenario: simulate session state transitions
    // ===================================================================

    #[test]
    fn test_full_resume_scenario_state_transitions() {
        // 1. Start with no session
        let mut state = GatewaySessionState::default();
        assert!(!state.has_session());

        // 2. First connection: IDENTIFY → READY
        state.save_from_ready("sess_initial".to_string());
        state.update_seq(Some(1));
        state.update_seq(Some(2));
        state.update_seq(Some(3));
        assert!(state.has_session());
        assert_eq!(state.session_id.as_deref(), Some("sess_initial"));
        assert_eq!(state.last_seq, Some(3));

        // 3. Disconnect (ResumableDisconnect) — state preserved
        // 4. Reconnect with RESUME (using saved session_id + seq)
        let resume = build_resume_payload(
            "token",
            state.session_id.as_ref().unwrap(),
            state.last_seq.unwrap(),
        );
        assert_eq!(resume["op"], opcodes::RESUME);
        assert_eq!(resume["d"]["session_id"], "sess_initial");
        assert_eq!(resume["d"]["seq"], 3);

        // 5. RESUMED event received — session continues
        state.update_seq(Some(4));
        assert_eq!(state.last_seq, Some(4));

        // 6. INVALID_SESSION d:false — clear state
        state.clear();
        assert!(!state.has_session());

        // 7. Reconnect with IDENTIFY (no session state)
        let identify = build_identify_payload("token");
        assert_eq!(identify["op"], opcodes::IDENTIFY);
    }

    #[test]
    fn test_invalid_session_resumable_preserves_state() {
        let mut state = GatewaySessionState::default();
        state.save_from_ready("sess_abc".to_string());
        state.update_seq(Some(50));

        // INVALID_SESSION d:true — do NOT clear state
        // (The outer loop checks SessionOutcome::InvalidSessionResumable
        // and does NOT call state.clear())
        assert!(state.has_session());
        assert_eq!(state.last_seq, Some(50));

        // Next connection will send RESUME with saved state
        let resume = build_resume_payload(
            "token",
            state.session_id.as_ref().unwrap(),
            state.last_seq.unwrap(),
        );
        assert_eq!(resume["op"], opcodes::RESUME);
        assert_eq!(resume["d"]["session_id"], "sess_abc");
        assert_eq!(resume["d"]["seq"], 50);
    }

    #[test]
    fn test_invalid_session_not_resumable_clears_state() {
        let mut state = GatewaySessionState::default();
        state.save_from_ready("sess_old".to_string());
        state.update_seq(Some(99));

        // INVALID_SESSION d:false — clear state before reconnecting
        state.clear();
        assert!(!state.has_session());

        // Next connection will send IDENTIFY
        let identify = build_identify_payload("token");
        assert_eq!(identify["op"], opcodes::IDENTIFY);
    }

    // ===================================================================
    // Reaction / read-receipt tests
    // ===================================================================

    #[test]
    fn test_percent_encode_emoji_checkmark() {
        // ✅ = UTF-8 bytes E2 9C 85
        assert_eq!(percent_encode_emoji("✅"), "%E2%9C%85");
    }

    #[test]
    fn test_percent_encode_emoji_thumbsup() {
        // 👍 = UTF-8 bytes F0 9F 91 8D
        assert_eq!(percent_encode_emoji("👍"), "%F0%9F%91%8D");
    }

    #[test]
    fn test_percent_encode_emoji_eyes() {
        // 👀 = UTF-8 bytes F0 9F 91 80
        assert_eq!(percent_encode_emoji("👀"), "%F0%9F%91%80");
    }

    #[test]
    fn test_percent_encode_emoji_ascii_pass_through() {
        // Unreserved ASCII characters pass through unchanged.
        assert_eq!(percent_encode_emoji("abc123"), "abc123");
        assert_eq!(percent_encode_emoji("A-Z_.~"), "A-Z_.~");
    }

    #[tokio::test]
    async fn test_dispatch_message_returns_true_for_content() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<InboundMessage>(10);
        let msg = GatewayMessage {
            id: "1".to_string(),
            content: "hello".to_string(),
            channel_id: "2".to_string(),
            author: Some(GatewayAuthor {
                id: "3".to_string(),
                username: "user".to_string(),
            }),
            guild_id: None,
        };
        assert!(DiscordChannel::dispatch_message(&tx, &msg).await);
    }

    #[tokio::test]
    async fn test_dispatch_message_returns_false_for_empty() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<InboundMessage>(10);
        let msg = GatewayMessage {
            id: "2".to_string(),
            content: String::new(),
            channel_id: "3".to_string(),
            author: None,
            guild_id: None,
        };
        assert!(!DiscordChannel::dispatch_message(&tx, &msg).await);
    }

    #[tokio::test]
    async fn test_send_reaction_no_auth() {
        // No token → no auth header → request will fail, but should not panic.
        let channel = DiscordChannel::new();
        let _ = channel.send_reaction("123", "456", "✅").await;
    }

    #[test]
    fn test_read_receipt_url_format() {
        // Verify the path format matches Discord's API expectations.
        let channel_id = "123456789";
        let message_id = "987654321";
        let path = format!(
            "/channels/{}/messages/{}/reactions/%E2%9C%85/@me",
            channel_id, message_id
        );
        assert_eq!(
            path,
            "/channels/123456789/messages/987654321/reactions/%E2%9C%85/@me"
        );
    }
}
