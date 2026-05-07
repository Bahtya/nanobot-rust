//! WeChat (Weixin) iLink Bot API channel adapter.
//!
//! Connects kestrel to WeChat personal accounts via Tencent's iLink Bot API.
//!
//! ## Design notes (ported from hermes-agent weixin.py)
//! - Long-poll `getupdates` drives inbound delivery.
//! - Every outbound reply must echo the latest `context_token` for the peer.
//! - Media files move through an AES-128-ECB encrypted CDN protocol (optional).
//!
//! ## Configuration
//! ```yaml
//! channels:
//!   weixin:
//!     enabled: true
//!     account_id: "wxid_xxx@im.bot"
//!     bot_token: "..."
//! ```

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Local;
use parking_lot::Mutex as ParkMutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use kestrel_bus::events::InboundMessage;
use kestrel_core::{MessageType, Platform, SessionSource};

use crate::base::{BaseChannel, SendResult};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const ILINK_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const WEIXIN_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";
const ILINK_APP_ID: &str = "bot";
const CHANNEL_VERSION: &str = "2.2.0";
const ILINK_APP_CLIENT_VERSION: u32 = (2 << 16) | (2 << 8);

const EP_GET_UPDATES: &str = "ilink/bot/getupdates";
const EP_SEND_MESSAGE: &str = "ilink/bot/sendmessage";
const EP_SEND_TYPING: &str = "ilink/bot/sendtyping";
const EP_GET_CONFIG: &str = "ilink/bot/getconfig";

const LONG_POLL_TIMEOUT_MS: u64 = 35_000;
const _API_TIMEOUT_MS: u64 = 15_000;
const _CONFIG_TIMEOUT_MS: u64 = 10_000;

const MAX_CONSECUTIVE_FAILURES: u32 = 3;
const RETRY_DELAY_SECONDS: u64 = 2;
const BACKOFF_DELAY_SECONDS: u64 = 30;
const SESSION_EXPIRED_ERRCODE: i64 = -14;
const RATE_LIMIT_ERRCODE: i64 = -2;
const MESSAGE_DEDUP_TTL_SECONDS: u64 = 300;

const MAX_MESSAGE_LENGTH: usize = 2000;

// ---------------------------------------------------------------------------
// Item / message type constants
// ---------------------------------------------------------------------------

const ITEM_TEXT: i32 = 1;
const _ITEM_IMAGE: i32 = 2;
const _ITEM_VOICE: i32 = 3;
const _ITEM_FILE: i32 = 4;
const _ITEM_VIDEO: i32 = 5;

const MSG_TYPE_BOT: i32 = 2;
const MSG_STATE_FINISH: i32 = 2;

const TYPING_START: i32 = 1;
const _TYPING_STOP: i32 = 2;

// ---------------------------------------------------------------------------
// Request/response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct BaseInfo {
    channel_version: String,
}

fn base_info() -> BaseInfo {
    BaseInfo {
        channel_version: CHANNEL_VERSION.to_string(),
    }
}

#[derive(Debug, Serialize)]
struct GetUpdatesPayload {
    #[serde(rename = "get_updates_buf")]
    get_updates_buf: String,
    base_info: BaseInfo,
}

#[derive(Debug, Serialize)]
struct SendMessagePayload {
    msg: ILinkMessage,
    base_info: BaseInfo,
}

#[derive(Debug, Serialize)]
struct ILinkMessage {
    #[serde(skip_serializing_if = "String::is_empty")]
    from_user_id: String,
    to_user_id: String,
    client_id: String,
    message_type: i32,
    message_state: i32,
    item_list: Vec<MessageItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_token: Option<String>,
}

#[derive(Debug, Serialize)]
struct MessageItem {
    #[serde(rename = "type")]
    item_type: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    text_item: Option<TextItem>,
}

#[derive(Debug, Serialize)]
struct TextItem {
    text: String,
}

#[derive(Debug, Serialize)]
struct SendTypingPayload {
    ilink_user_id: String,
    typing_ticket: String,
    status: i32,
    base_info: BaseInfo,
}

#[derive(Debug, Serialize)]
struct GetConfigPayload {
    ilink_user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_token: Option<String>,
    base_info: BaseInfo,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GetUpdatesResponse {
    ret: Option<i64>,
    errcode: Option<i64>,
    errmsg: Option<String>,
    #[serde(rename = "get_updates_buf")]
    get_updates_buf: Option<String>,
    #[serde(rename = "longpolling_timeout_ms")]
    longpolling_timeout_ms: Option<u64>,
    msgs: Option<Vec<ILinkMsg>>,
}

#[derive(Debug, Deserialize)]
struct ILinkMsg {
    message_id: Option<String>,
    from_user_id: Option<String>,
    to_user_id: Option<String>,
    room_id: Option<String>,
    chat_room_id: Option<String>,
    msg_type: Option<i32>,
    context_token: Option<String>,
    item_list: Option<Vec<ILinkItem>>,
}

#[derive(Debug, Deserialize)]
struct ILinkItem {
    #[serde(rename = "type")]
    item_type: Option<i32>,
    text_item: Option<ILinkTextItem>,
}

#[derive(Debug, Deserialize)]
struct ILinkTextItem {
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SendMessageResponse {
    ret: Option<i64>,
    errcode: Option<i64>,
    errmsg: Option<String>,
    msg: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetConfigResponse {
    typing_ticket: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn random_wechat_uin() -> String {
    let value = rand::random::<u32>();
    base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        value.to_string(),
    )
}

fn api_headers(token: Option<&str>, body: &str) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert("Content-Type".to_string(), "application/json".to_string());
    headers.insert(
        "AuthorizationType".to_string(),
        "ilink_bot_token".to_string(),
    );
    headers.insert("Content-Length".to_string(), body.len().to_string());
    headers.insert("X-WECHAT-UIN".to_string(), random_wechat_uin());
    headers.insert("iLink-App-Id".to_string(), ILINK_APP_ID.to_string());
    headers.insert(
        "iLink-App-ClientVersion".to_string(),
        ILINK_APP_CLIENT_VERSION.to_string(),
    );
    if let Some(t) = token {
        headers.insert("Authorization".to_string(), format!("Bearer {}", t));
    }
    headers
}

fn is_stale_session_ret(ret: Option<i64>, errcode: Option<i64>, errmsg: Option<&str>) -> bool {
    if ret != Some(RATE_LIMIT_ERRCODE) && errcode != Some(RATE_LIMIT_ERRCODE) {
        return false;
    }
    errmsg
        .map(|s| s.to_lowercase() == "unknown error")
        .unwrap_or(false)
}

fn extract_text(item_list: &[ILinkItem]) -> String {
    for item in item_list {
        if item.item_type == Some(ITEM_TEXT) {
            if let Some(ref ti) = item.text_item {
                return ti.text.clone().unwrap_or_default();
            }
        }
    }
    String::new()
}

fn guess_chat_type<'a>(message: &'a ILinkMsg, account_id: &'a str) -> (&'a str, String) {
    let room_id = message
        .room_id
        .as_deref()
        .or(message.chat_room_id.as_deref())
        .unwrap_or("")
        .trim();
    let to_user_id = message.to_user_id.as_deref().unwrap_or("").trim();
    let from_user_id = message.from_user_id.as_deref().unwrap_or("").trim();

    let is_group = !room_id.is_empty()
        || (!to_user_id.is_empty()
            && !account_id.is_empty()
            && to_user_id != account_id
            && message.msg_type == Some(1));

    if is_group {
        (
            "group",
            if !room_id.is_empty() {
                room_id.to_string()
            } else if !to_user_id.is_empty() {
                to_user_id.to_string()
            } else {
                from_user_id.to_string()
            },
        )
    } else {
        ("dm", from_user_id.to_string())
    }
}

fn split_text_for_weixin(content: &str) -> Vec<String> {
    if content.is_empty() {
        return vec![];
    }
    if content.len() <= MAX_MESSAGE_LENGTH {
        return vec![content.to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in content.lines() {
        if current.len() + line.len() + 1 > MAX_MESSAGE_LENGTH {
            if !current.is_empty() {
                chunks.push(current.trim().to_string());
            }
            current = line.to_string();
        } else {
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        chunks.push(current.trim().to_string());
    }
    if chunks.is_empty() {
        chunks.push(content.to_string());
    }
    chunks
}

fn safe_id(value: Option<&str>, keep: usize) -> String {
    let raw = value.unwrap_or("").trim();
    if raw.is_empty() {
        return "?".to_string();
    }
    if raw.len() <= keep {
        raw.to_string()
    } else {
        raw[..keep].to_string()
    }
}

// ---------------------------------------------------------------------------
// Typing ticket cache
// ---------------------------------------------------------------------------

struct TypingTicketCache {
    ttl_seconds: f64,
    cache: ParkMutex<HashMap<String, (String, std::time::Instant)>>,
}

impl TypingTicketCache {
    fn new(ttl_seconds: f64) -> Self {
        Self {
            ttl_seconds,
            cache: ParkMutex::new(HashMap::new()),
        }
    }

    fn get(&self, user_id: &str) -> Option<String> {
        let cache = self.cache.lock();
        let entry = cache.get(user_id)?;
        if entry.1.elapsed().as_secs_f64() >= self.ttl_seconds {
            drop(cache);
            self.cache.lock().remove(user_id);
            return None;
        }
        Some(entry.0.clone())
    }

    fn set(&self, user_id: &str, ticket: &str) {
        self.cache.lock().insert(
            user_id.to_string(),
            (ticket.to_string(), std::time::Instant::now()),
        );
    }
}

// ---------------------------------------------------------------------------
// Context token store (disk-backed)
// ---------------------------------------------------------------------------

struct ContextTokenStore {
    root: PathBuf,
    cache: ParkMutex<HashMap<String, String>>,
}

impl ContextTokenStore {
    fn new() -> Self {
        let root = kestrel_config::paths::get_kestrel_home()
            .map(|h| h.join("weixin"))
            .unwrap_or_else(|_| std::env::temp_dir().join("kestrel-weixin"));
        let _ = std::fs::create_dir_all(&root);
        Self {
            root,
            cache: ParkMutex::new(HashMap::new()),
        }
    }

    fn path(&self, account_id: &str) -> PathBuf {
        self.root.join(format!("{}.context-tokens.json", account_id))
    }

    fn make_key(account_id: &str, user_id: &str) -> String {
        format!("{}:{}", account_id, user_id)
    }

    fn restore(&self, account_id: &str) {
        let path = self.path(account_id);
        if !path.exists() {
            return;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                warn!("[weixin] failed to read context tokens for {}: {}", safe_id(Some(account_id), 8), e);
                return;
            }
        };
        let data: HashMap<String, String> = match serde_json::from_str(&text) {
            Ok(d) => d,
            Err(e) => {
                warn!("[weixin] failed to parse context tokens for {}: {}", safe_id(Some(account_id), 8), e);
                return;
            }
        };
        let mut cache = self.cache.lock();
        let prefix = format!("{}:", account_id);
        cache.retain(|k, _| !k.starts_with(&prefix));
        let mut restored = 0;
        for (user_id, token) in data {
            if !token.is_empty() {
                cache.insert(Self::make_key(account_id, &user_id), token);
                restored += 1;
            }
        }
        if restored > 0 {
            info!("[weixin] restored {} context token(s) for {}", restored, safe_id(Some(account_id), 8));
        }
    }

    fn get(&self, account_id: &str, user_id: &str) -> Option<String> {
        self.cache
            .lock()
            .get(&Self::make_key(account_id, user_id))
            .cloned()
    }

    fn set(&self, account_id: &str, user_id: &str, token: &str) {
        self.cache
            .lock()
            .insert(Self::make_key(account_id, user_id), token.to_string());
        self.persist(account_id);
    }

    fn persist(&self, account_id: &str) {
        let path = self.path(account_id);
        let prefix = format!("{}:", account_id);
        let payload: HashMap<String, String> = {
            let cache = self.cache.lock();
            cache
                .iter()
                .filter(|(k, _)| k.starts_with(&prefix))
                .map(|(k, v)| (k[prefix.len()..].to_string(), v.clone()))
                .collect()
        };
        let text = match serde_json::to_string_pretty(&payload) {
            Ok(t) => t,
            Err(e) => {
                warn!("[weixin] failed to serialize context tokens: {}", e);
                return;
            }
        };
        // Atomic write via temp file + rename
        let tmp = path.with_extension("tmp");
        if let Err(e) = (|| -> std::io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, &path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o600);
                std::fs::set_permissions(&path, perms)?;
            }
            Ok(())
        })() {
            warn!("[weixin] failed to persist context tokens: {}", e);
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

// ---------------------------------------------------------------------------
// Message deduplicator
// ---------------------------------------------------------------------------

struct MessageDedup {
    ttl_seconds: u64,
    seen: ParkMutex<HashMap<String, std::time::Instant>>,
}

impl MessageDedup {
    fn new(ttl_seconds: u64) -> Self {
        Self {
            ttl_seconds,
            seen: ParkMutex::new(HashMap::new()),
        }
    }

    fn is_duplicate(&self, key: &str) -> bool {
        let mut seen = self.seen.lock();
        let now = std::time::Instant::now();
        // Clean old entries periodically (simple heuristic: every 100 inserts)
        #[allow(clippy::manual_is_multiple_of)]
        if seen.len() % 100 == 0 {
            let ttl = std::time::Duration::from_secs(self.ttl_seconds);
            seen.retain(|_, t| now.duration_since(*t) < ttl);
        }
        if seen.contains_key(key) {
            return true;
        }
        seen.insert(key.to_string(), now);
        false
    }
}

// ---------------------------------------------------------------------------
// WeixinChannel
// ---------------------------------------------------------------------------

/// WeChat iLink Bot API channel adapter.
pub struct WeixinChannel {
    account_id: Option<String>,
    token: Option<String>,
    base_url: String,
    #[allow(dead_code)]
    cdn_base_url: String,
    dm_policy: String,
    group_policy: String,
    allow_from: Vec<String>,
    group_allow_from: Vec<String>,
    connected: bool,
    running: Arc<AtomicBool>,
    message_handler: Option<tokio::sync::mpsc::Sender<InboundMessage>>,
    client: reqwest::Client,
    token_store: Arc<ContextTokenStore>,
    typing_cache: Arc<TypingTicketCache>,
    dedup: Arc<MessageDedup>,
    sync_buf: Arc<ParkMutex<String>>,
}

impl WeixinChannel {
    /// Create a new WeixinChannel reading credentials from environment.
    pub fn new() -> Self {
        Self::from_env_or_config(None)
    }

    /// Create from a WeixinConfig.
    pub fn new_with_config(config: &kestrel_config::schema::WeixinConfig) -> Self {
        Self::from_env_or_config(Some(config))
    }

    fn from_env_or_config(config: Option<&kestrel_config::schema::WeixinConfig>) -> Self {
        let account_id = config
            .and_then(|c| c.account_id.clone())
            .or_else(|| std::env::var("WEIXIN_ACCOUNT_ID").ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let token = config
            .and_then(|c| c.bot_token.clone())
            .or_else(|| std::env::var("WEIXIN_TOKEN").ok())
            .or_else(|| config.and_then(|c| c.token.clone()))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let base_url = config
            .and_then(|c| c.base_url.clone())
            .or_else(|| std::env::var("WEIXIN_BASE_URL").ok())
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| ILINK_BASE_URL.to_string());

        let cdn_base_url = config
            .and_then(|c| c.cdn_base_url.clone())
            .or_else(|| std::env::var("WEIXIN_CDN_BASE_URL").ok())
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| WEIXIN_CDN_BASE_URL.to_string());

        let dm_policy = config
            .map(|c| c.dm_policy.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "open".to_string());

        let group_policy = config
            .map(|c| c.group_policy.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "disabled".to_string());

        let allow_from = config.map(|c| c.allowed_users.clone()).unwrap_or_default();

        let group_allow_from = config
            .map(|c| c.group_allowed_users.clone())
            .unwrap_or_default();

        Self {
            account_id,
            token,
            base_url,
            cdn_base_url,
            dm_policy,
            group_policy,
            allow_from,
            group_allow_from,
            connected: false,
            running: Arc::new(AtomicBool::new(false)),
            message_handler: None,
            client: reqwest::Client::new(),
            token_store: Arc::new(ContextTokenStore::new()),
            typing_cache: Arc::new(TypingTicketCache::new(600.0)),
            dedup: Arc::new(MessageDedup::new(MESSAGE_DEDUP_TTL_SECONDS)),
            sync_buf: Arc::new(ParkMutex::new(String::new())),
        }
    }

    #[allow(dead_code)]
    fn is_dm_allowed(&self, sender_id: &str) -> bool {
        match self.dm_policy.as_str() {
            "disabled" => false,
            "allowlist" => self.allow_from.contains(&sender_id.to_string()),
            _ => true,
        }
    }

    #[allow(dead_code)]
    fn is_group_allowed(&self, chat_id: &str) -> bool {
        match self.group_policy.as_str() {
            "disabled" => false,
            "allowlist" => self.group_allow_from.contains(&chat_id.to_string()),
            _ => true,
        }
    }

    // -----------------------------------------------------------------------
    // HTTP helpers
    // -----------------------------------------------------------------------

    async fn api_post<T: Serialize>(
        &self,
        endpoint: &str,
        payload: T,
    ) -> Result<reqwest::Response> {
        let body = serde_json::to_string(&payload).context("serialize payload")?;
        let url = format!("{}/{}", self.base_url, endpoint);
        let headers = api_headers(self.token.as_deref(), &body);

        let mut req = self.client.post(&url).body(body);
        for (k, v) in headers {
            req = req.header(k, v);
        }

        req.send().await.context(format!("POST {}", endpoint))
    }

    async fn get_updates(&self, sync_buf: &str, _timeout_ms: u64) -> Result<GetUpdatesResponse> {
        let payload = GetUpdatesPayload {
            get_updates_buf: sync_buf.to_string(),
            base_info: base_info(),
        };
        let resp = self
            .api_post(EP_GET_UPDATES, payload)
            .await
            .context("getUpdates request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "getUpdates HTTP {}: {}",
                status,
                &text[..text.len().min(200)]
            );
        }

        resp.json().await.context("parse getUpdates response")
    }

    async fn send_message_api(
        &self,
        to: &str,
        text: &str,
        context_token: Option<&str>,
        client_id: &str,
    ) -> Result<SendMessageResponse> {
        let payload = SendMessagePayload {
            msg: ILinkMessage {
                from_user_id: String::new(),
                to_user_id: to.to_string(),
                client_id: client_id.to_string(),
                message_type: MSG_TYPE_BOT,
                message_state: MSG_STATE_FINISH,
                item_list: vec![MessageItem {
                    item_type: ITEM_TEXT,
                    text_item: Some(TextItem {
                        text: text.to_string(),
                    }),
                }],
                context_token: context_token.map(|s| s.to_string()),
            },
            base_info: base_info(),
        };
        let resp = self
            .api_post(EP_SEND_MESSAGE, payload)
            .await
            .context("sendMessage request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "sendMessage HTTP {}: {}",
                status,
                &text[..text.len().min(200)]
            );
        }

        resp.json().await.context("parse sendMessage response")
    }

    async fn send_typing_api(&self, user_id: &str, ticket: &str, status: i32) -> Result<()> {
        let payload = SendTypingPayload {
            ilink_user_id: user_id.to_string(),
            typing_ticket: ticket.to_string(),
            status,
            base_info: base_info(),
        };
        let resp = self
            .api_post(EP_SEND_TYPING, payload)
            .await
            .context("sendTyping request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "sendTyping HTTP {}: {}",
                status,
                &text[..text.len().min(200)]
            );
        }
        Ok(())
    }

    async fn get_config_api(
        &self,
        user_id: &str,
        context_token: Option<&str>,
    ) -> Result<GetConfigResponse> {
        let payload = GetConfigPayload {
            ilink_user_id: user_id.to_string(),
            context_token: context_token.map(|s| s.to_string()),
            base_info: base_info(),
        };
        let resp = self
            .api_post(EP_GET_CONFIG, payload)
            .await
            .context("getConfig request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "getUpdates HTTP {}: {}",
                status,
                &text[..text.len().min(200)]
            );
        }

        resp.json().await.context("parse getConfig response")
    }

    // -----------------------------------------------------------------------
    // Polling
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    async fn poll_loop(
        client: reqwest::Client,
        base_url: String,
        token: String,
        account_id: String,
        handler: tokio::sync::mpsc::Sender<InboundMessage>,
        running: Arc<AtomicBool>,
        token_store: Arc<ContextTokenStore>,
        typing_cache: Arc<TypingTicketCache>,
        dedup: Arc<MessageDedup>,
        sync_buf: Arc<ParkMutex<String>>,
        dm_policy: String,
        group_policy: String,
        allow_from: Vec<String>,
        group_allow_from: Vec<String>,
    ) {
        let channel = WeixinChannel {
            account_id: Some(account_id.clone()),
            token: Some(token.clone()),
            base_url: base_url.clone(),
            cdn_base_url: String::new(),
            dm_policy,
            group_policy,
            allow_from,
            group_allow_from,
            connected: false,
            running: running.clone(),
            message_handler: None,
            client,
            token_store: token_store.clone(),
            typing_cache: typing_cache.clone(),
            dedup: dedup.clone(),
            sync_buf: sync_buf.clone(),
        };

        let mut timeout_ms = LONG_POLL_TIMEOUT_MS;
        let mut consecutive_failures: u32 = 0;

        info!(
            "[weixin] Polling task started account={}",
            safe_id(Some(&account_id), 8)
        );

        while running.load(Ordering::Relaxed) {
            let sync_buf_val = sync_buf.lock().clone();
            match channel.get_updates(&sync_buf_val, timeout_ms).await {
                Ok(response) => {
                    // Adjust timeout if server suggests one.
                    if let Some(suggested) = response.longpolling_timeout_ms {
                        if suggested > 0 {
                            timeout_ms = suggested;
                        }
                    }

                    let ret = response.ret.unwrap_or(0);
                    let errcode = response.errcode.unwrap_or(0);

                    if ret != 0 || errcode != 0 {
                        if ret == SESSION_EXPIRED_ERRCODE
                            || errcode == SESSION_EXPIRED_ERRCODE
                            || is_stale_session_ret(
                                Some(ret),
                                Some(errcode),
                                response.errmsg.as_deref(),
                            )
                        {
                            error!("[weixin] Session expired; pausing for 10 minutes");
                            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                            consecutive_failures = 0;
                            continue;
                        }
                        consecutive_failures += 1;
                        warn!(
                            "[weixin] getUpdates failed ret={} errcode={} errmsg={:?} ({}/{})",
                            ret,
                            errcode,
                            response.errmsg,
                            consecutive_failures,
                            MAX_CONSECUTIVE_FAILURES
                        );
                        let delay = if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                            consecutive_failures = 0;
                            BACKOFF_DELAY_SECONDS
                        } else {
                            RETRY_DELAY_SECONDS
                        };
                        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                        continue;
                    }

                    consecutive_failures = 0;

                    if let Some(new_buf) = response.get_updates_buf {
                        if !new_buf.is_empty() {
                            *sync_buf.lock() = new_buf;
                        }
                    }

                    if let Some(msgs) = response.msgs {
                        for msg in msgs {
                            let handler = handler.clone();
                            let token_store = token_store.clone();
                            let typing_cache = typing_cache.clone();
                            let dedup = dedup.clone();
                            let account_id = account_id.clone();
                            let dm_policy = channel.dm_policy.clone();
                            let group_policy = channel.group_policy.clone();
                            let allow_from = channel.allow_from.clone();
                            let group_allow_from = channel.group_allow_from.clone();
                            tokio::spawn(async move {
                                if let Err(e) = Self::process_message(
                                    msg,
                                    handler,
                                    &account_id,
                                    token_store,
                                    typing_cache,
                                    dedup,
                                    &dm_policy,
                                    &group_policy,
                                    &allow_from,
                                    &group_allow_from,
                                )
                                .await
                                {
                                    error!("[weixin] process_message error: {}", e);
                                }
                            });
                        }
                    }
                }
                Err(e) => {
                    consecutive_failures += 1;
                    error!(
                        "[weixin] poll error ({}/{}): {}",
                        consecutive_failures, MAX_CONSECUTIVE_FAILURES, e
                    );
                    let delay = if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        consecutive_failures = 0;
                        BACKOFF_DELAY_SECONDS
                    } else {
                        RETRY_DELAY_SECONDS
                    };
                    tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                }
            }
        }

        info!("[weixin] Polling task stopped");
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_message(
        message: ILinkMsg,
        handler: tokio::sync::mpsc::Sender<InboundMessage>,
        account_id: &str,
        token_store: Arc<ContextTokenStore>,
        _typing_cache: Arc<TypingTicketCache>,
        dedup: Arc<MessageDedup>,
        dm_policy: &str,
        group_policy: &str,
        allow_from: &[String],
        group_allow_from: &[String],
    ) -> Result<()> {
        let sender_id = message.from_user_id.as_deref().unwrap_or("").trim();
        if sender_id.is_empty() {
            return Ok(());
        }
        if sender_id == account_id {
            return Ok(());
        }

        let message_id = message.message_id.as_deref().unwrap_or("").trim();
        if !message_id.is_empty() && dedup.is_duplicate(message_id) {
            return Ok(());
        }

        let item_list = message.item_list.as_deref().unwrap_or(&[]);
        let text = extract_text(item_list);

        // Content-based dedup for text messages.
        if !text.is_empty() {
            let content_key = format!("content:{}:{}", sender_id, &text[..text.len().min(200)]);
            if dedup.is_duplicate(&content_key) {
                debug!(
                    "[weixin] Content-dedup: skipping duplicate from {}",
                    sender_id
                );
                return Ok(());
            }
        }

        let (chat_type, effective_chat_id) = guess_chat_type(&message, account_id);

        if chat_type == "group" {
            if group_policy == "disabled" {
                return Ok(());
            }
            if group_policy == "allowlist" && !group_allow_from.contains(&effective_chat_id) {
                return Ok(());
            }
        } else {
            match dm_policy {
                "disabled" => return Ok(()),
                "allowlist" if !allow_from.contains(&sender_id.to_string()) => return Ok(()),
                _ => {}
            }
        }

        let context_token = message.context_token.as_deref().unwrap_or("").trim();
        if !context_token.is_empty() {
            token_store.set(account_id, sender_id, context_token);
        }

        if text.is_empty() {
            // TODO: handle media items
            return Ok(());
        }

        let source = SessionSource {
            platform: Platform::Weixin,
            chat_id: effective_chat_id.clone(),
            chat_name: None,
            chat_type: chat_type.to_string(),
            user_id: Some(sender_id.to_string()),
            user_name: Some(sender_id.to_string()),
            thread_id: None,
            chat_topic: None,
        };

        let inbound = InboundMessage {
            channel: Platform::Weixin,
            sender_id: sender_id.to_string(),
            chat_id: effective_chat_id,
            content: text,
            media: vec![],
            metadata: HashMap::new(),
            source: Some(source),
            message_type: MessageType::Text,
            message_id: if message_id.is_empty() {
                None
            } else {
                Some(message_id.to_string())
            },
            trace_id: None,
            reply_to: None,
            timestamp: Local::now(),
        };

        handler
            .send(inbound)
            .await
            .context("handler channel closed")?;
        Ok(())
    }
}

impl Default for WeixinChannel {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BaseChannel for WeixinChannel {
    fn name(&self) -> &str {
        "weixin"
    }

    fn platform(&self) -> Platform {
        Platform::Weixin
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    async fn connect(&mut self) -> Result<bool> {
        let token = match self.token.as_deref() {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => {
                warn!(
                    "[weixin] Token not configured (set WEIXIN_TOKEN or channels.weixin.bot_token)"
                );
                return Ok(false);
            }
        };

        let account_id = match self.account_id.as_deref() {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => {
                warn!(
                    "[weixin] Account ID not configured (set WEIXIN_ACCOUNT_ID or channels.weixin.account_id)"
                );
                return Ok(false);
            }
        };

        // Restore persisted context tokens for this account.
        self.token_store.restore(&account_id);

        self.running.store(true, Ordering::Relaxed);
        self.connected = true;

        if let Some(handler) = self.message_handler.clone() {
            let client = self.client.clone();
            let base_url = self.base_url.clone();
            let running = self.running.clone();
            let token_store = self.token_store.clone();
            let typing_cache = self.typing_cache.clone();
            let dedup = self.dedup.clone();
            let sync_buf = self.sync_buf.clone();
            let dm_policy = self.dm_policy.clone();
            let group_policy = self.group_policy.clone();
            let allow_from = self.allow_from.clone();
            let group_allow_from = self.group_allow_from.clone();

            tokio::spawn(async move {
                WeixinChannel::poll_loop(
                    client,
                    base_url,
                    token,
                    account_id,
                    handler,
                    running,
                    token_store,
                    typing_cache,
                    dedup,
                    sync_buf,
                    dm_policy,
                    group_policy,
                    allow_from,
                    group_allow_from,
                )
                .await;
            });

            info!("[weixin] Channel connected — polling started");
        } else {
            warn!("[weixin] Connected but no message_handler set; polling not started");
        }

        Ok(true)
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.running.store(false, Ordering::Relaxed);
        self.connected = false;
        info!("[weixin] Channel disconnected");
        Ok(())
    }

    async fn send_message(
        &self,
        chat_id: &str,
        content: &str,
        _reply_to: Option<&str>,
    ) -> Result<SendResult> {
        if !self.connected {
            return Ok(SendResult {
                success: false,
                message_id: None,
                error: Some("Not connected".to_string()),
                retryable: false,
            });
        }

        let account_id = match self.account_id.as_deref() {
            Some(a) => a,
            None => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some("Account ID not available".to_string()),
                    retryable: false,
                });
            }
        };

        let context_token = self.token_store.get(account_id, chat_id);
        let chunks = split_text_for_weixin(content);
        let mut last_message_id: Option<String> = None;

        for chunk in chunks {
            let client_id = format!("kestrel-weixin-{}", uuid::Uuid::new_v4());
            match self
                .send_message_api(chat_id, &chunk, context_token.as_deref(), &client_id)
                .await
            {
                Ok(resp) => {
                    let ret = resp.ret.unwrap_or(0);
                    let errcode = resp.errcode.unwrap_or(0);
                    if ret != 0 || errcode != 0 {
                        let is_session_expired = ret == SESSION_EXPIRED_ERRCODE
                            || errcode == SESSION_EXPIRED_ERRCODE
                            || is_stale_session_ret(
                                Some(ret),
                                Some(errcode),
                                resp.errmsg.as_deref(),
                            );

                        if is_session_expired && context_token.is_some() {
                            // Retry without context_token once.
                            warn!(
                                "[weixin] session expired for {}; retrying without context_token",
                                safe_id(Some(chat_id), 8)
                            );
                            match self
                                .send_message_api(chat_id, &chunk, None, &client_id)
                                .await
                            {
                                Ok(retry_resp) => {
                                    let rret = retry_resp.ret.unwrap_or(0);
                                    let rcode = retry_resp.errcode.unwrap_or(0);
                                    if rret != 0 || rcode != 0 {
                                        let errmsg = retry_resp
                                            .errmsg
                                            .or(retry_resp.msg)
                                            .unwrap_or_else(|| "unknown error".to_string());
                                        return Ok(SendResult {
                                            success: false,
                                            message_id: None,
                                            error: Some(format!(
                                                "iLink sendmessage error: ret={} errcode={} errmsg={}",
                                                rret, rcode, errmsg
                                            )),
                                            retryable: false,
                                        });
                                    }
                                    last_message_id = Some(client_id);
                                    continue;
                                }
                                Err(e) => {
                                    return Ok(SendResult {
                                        success: false,
                                        message_id: None,
                                        error: Some(format!("sendMessage retry failed: {}", e)),
                                        retryable: true,
                                    });
                                }
                            }
                        }

                        let errmsg = resp
                            .errmsg
                            .or(resp.msg)
                            .unwrap_or_else(|| "unknown error".to_string());
                        return Ok(SendResult {
                            success: false,
                            message_id: None,
                            error: Some(format!(
                                "iLink sendmessage error: ret={} errcode={} errmsg={}",
                                ret, errcode, errmsg
                            )),
                            retryable: false,
                        });
                    }
                    last_message_id = Some(client_id);
                }
                Err(e) => {
                    return Ok(SendResult {
                        success: false,
                        message_id: None,
                        error: Some(format!("sendMessage failed: {}", e)),
                        retryable: true,
                    });
                }
            }
        }

        Ok(SendResult {
            success: true,
            message_id: last_message_id,
            error: None,
            retryable: false,
        })
    }

    async fn send_typing(&self, chat_id: &str, _trace_id: Option<&str>) -> Result<()> {
        if !self.connected {
            return Ok(());
        }

        let ticket = match self.typing_cache.get(chat_id) {
            Some(t) => t,
            None => {
                let account_id = match self.account_id.as_deref() {
                    Some(a) => a,
                    None => return Ok(()),
                };
                let context_token = self.token_store.get(account_id, chat_id);
                match self.get_config_api(chat_id, context_token.as_deref()).await {
                    Ok(cfg) => {
                        if let Some(ticket) = cfg.typing_ticket {
                            self.typing_cache.set(chat_id, &ticket);
                            ticket
                        } else {
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        debug!("[weixin] getConfig failed for typing ticket: {}", e);
                        return Ok(());
                    }
                }
            }
        };

        if let Err(e) = self.send_typing_api(chat_id, &ticket, TYPING_START).await {
            debug!(
                "[weixin] typing start failed for {}: {}",
                safe_id(Some(chat_id), 8),
                e
            );
        }
        Ok(())
    }

    async fn send_image(
        &self,
        _chat_id: &str,
        _image_url: &str,
        _caption: Option<&str>,
    ) -> Result<SendResult> {
        // TODO: implement media upload via AES-ECB encrypted CDN
        Ok(SendResult {
            success: false,
            message_id: None,
            error: Some("Weixin image sending not yet implemented".to_string()),
            retryable: false,
        })
    }

    fn set_message_handler(&mut self, handler: tokio::sync::mpsc::Sender<InboundMessage>) {
        self.message_handler = Some(handler);
    }
}
