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

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Local;
use parking_lot::Mutex as ParkMutex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tracing::{debug, error, info, warn};

use kestrel_bus::events::InboundMessage;
use kestrel_core::{MediaAttachment, MessageType, Platform, SessionSource};

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
const EP_GET_UPLOAD_URL: &str = "ilink/bot/getuploadurl";

const LONG_POLL_TIMEOUT_MS: u64 = 35_000;
const _API_TIMEOUT_MS: u64 = 15_000;
const _CONFIG_TIMEOUT_MS: u64 = 10_000;

const UPLOAD_CHUNK_SIZE: usize = 512 * 1024;
const MAX_IMAGE_SIZE: usize = 10 * 1024 * 1024;
const MAX_FILE_SIZE: usize = 20 * 1024 * 1024;
const MAX_VIDEO_SIZE: usize = 10 * 1024 * 1024;

const CDN_ALLOWED_DOMAINS: &[&str] = &["novac2c.cdn.weixin.qq.com", "ilinkai.weixin.qq.com"];

const MAX_CONSECUTIVE_FAILURES: u32 = 3;
const RETRY_DELAY_SECONDS: u64 = 2;
const BACKOFF_DELAY_SECONDS: u64 = 30;
const SESSION_EXPIRED_ERRCODE: i64 = -14;
const RATE_LIMIT_ERRCODE: i64 = -2;
const SESSION_EXPIRED_PAUSE_SECS: u64 = 600;
const MESSAGE_DEDUP_TTL_SECONDS: u64 = 300;

const MAX_MESSAGE_LENGTH: usize = 2000;

// ---------------------------------------------------------------------------
// Item / message type constants
// ---------------------------------------------------------------------------

const ITEM_TEXT: i32 = 1;
const ITEM_IMAGE: i32 = 2;
const ITEM_VOICE: i32 = 3;
const ITEM_FILE: i32 = 4;
const ITEM_VIDEO: i32 = 5;

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
    #[serde(default, deserialize_with = "deserialize_opt_i64_from_any")]
    ret: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_opt_i64_from_any")]
    errcode: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    errmsg: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    get_updates_buf: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    sync_buf: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_u64_from_any")]
    longpolling_timeout_ms: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_opt_vec_from_any")]
    msgs: Option<Vec<ILinkMsg>>,
}

#[derive(Debug, Deserialize)]
struct ILinkMsg {
    #[allow(dead_code)]
    #[serde(default, deserialize_with = "deserialize_opt_i64_from_any")]
    seq: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    message_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    from_user_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    to_user_id: Option<String>,
    #[allow(dead_code)]
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    client_id: Option<String>,
    #[allow(dead_code)]
    #[serde(default, deserialize_with = "deserialize_opt_i64_from_any")]
    create_time_ms: Option<i64>,
    #[serde(
        default,
        alias = "group_id",
        deserialize_with = "deserialize_opt_string_from_any"
    )]
    room_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    chat_room_id: Option<String>,
    #[serde(
        default,
        alias = "message_type",
        deserialize_with = "deserialize_opt_i32_from_any"
    )]
    msg_type: Option<i32>,
    #[allow(dead_code)]
    #[serde(default, deserialize_with = "deserialize_opt_i32_from_any")]
    message_state: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    context_token: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_vec_from_any")]
    item_list: Option<Vec<ILinkItem>>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ILinkItem {
    #[serde(rename = "type")]
    #[serde(default, deserialize_with = "deserialize_opt_i32_from_any")]
    item_type: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_opt_i64_from_any")]
    seq: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    item_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    item_key: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    mime_type: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    file_name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_i64_from_any")]
    size: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_opt_i64_from_any")]
    create_time_ms: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_opt_text_item_from_any")]
    text_item: Option<ILinkTextItem>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    cdn_url: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    aes_key: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    encrypted_query_param: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    thumb_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ILinkTextItem {
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    #[serde(alias = "content")]
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

#[derive(Debug, Serialize)]
struct GetUploadUrlPayload {
    file_type: i32,
    file_size: i64,
    file_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    to_user_id: Option<String>,
    base_info: BaseInfo,
}

#[derive(Debug, Deserialize)]
struct GetUploadUrlResponse {
    #[serde(default, deserialize_with = "deserialize_opt_i64_from_any")]
    ret: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_opt_i64_from_any")]
    errcode: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    errmsg: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    upload_url: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    aes_key: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    #[allow(dead_code)]
    media_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_from_any")]
    #[allow(dead_code)]
    cdn_url: Option<String>,
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

// ---------------------------------------------------------------------------
// AES-128-ECB helpers
// ---------------------------------------------------------------------------

fn pkcs7_pad(data: &[u8], block_size: usize) -> Vec<u8> {
    let padding = block_size - (data.len() % block_size);
    let mut out = data.to_vec();
    out.extend(std::vec![padding as u8; padding]);
    out
}

fn pkcs7_unpad(data: &[u8]) -> Result<Vec<u8>> {
    if data.is_empty() {
        anyhow::bail!("empty data");
    }
    let pad_len = *data.last().unwrap() as usize;
    if pad_len == 0 || pad_len > data.len() || pad_len > 16 {
        anyhow::bail!("invalid PKCS7 padding (pad_len={})", pad_len);
    }
    if data[data.len() - pad_len..]
        .iter()
        .any(|&b| b as usize != pad_len)
    {
        anyhow::bail!("inconsistent PKCS7 padding");
    }
    Ok(data[..data.len() - pad_len].to_vec())
}

fn aes_encrypt_ecb(key: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let cipher = aes::Aes128::new(GenericArray::from_slice(key));
    let padded = pkcs7_pad(plaintext, 16);
    let mut buf = padded;
    for chunk in buf.chunks_exact_mut(16) {
        let block = GenericArray::from_mut_slice(chunk);
        cipher.encrypt_block(block);
    }
    buf
}

fn aes_decrypt_ecb(key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    if !ciphertext.len().is_multiple_of(16) {
        anyhow::bail!("ciphertext length not a multiple of 16");
    }
    let cipher = aes::Aes128::new(GenericArray::from_slice(key));
    let mut buf = ciphertext.to_vec();
    for chunk in buf.chunks_exact_mut(16) {
        let block = GenericArray::from_mut_slice(chunk);
        cipher.decrypt_block(block);
    }
    pkcs7_unpad(&buf)
}

fn decode_aes_key(raw: &str) -> Result<Vec<u8>> {
    if raw.is_empty() {
        anyhow::bail!("empty AES key");
    }
    if let Ok(bytes) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, raw) {
        if bytes.len() == 16 {
            return Ok(bytes);
        }
    }
    let bytes = hex::decode(raw.trim()).context("hex decode AES key")?;
    if bytes.len() != 16 {
        anyhow::bail!("AES key must be 16 bytes, got {}", bytes.len());
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// CDN URL whitelist (SSRF protection)
// ---------------------------------------------------------------------------

fn is_cdn_url_allowed(url: &str) -> bool {
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return false,
    };
    let host = match parsed.host_str() {
        Some(h) => h.to_lowercase(),
        None => return false,
    };
    CDN_ALLOWED_DOMAINS
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{}", d)))
}

fn resolve_aes_key(item: &ILinkItem) -> Option<String> {
    item.aes_key
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            item.encrypted_query_param
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
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

fn value_to_string(value: Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        other => Some(other.to_string()),
    }
}

fn value_to_i32(value: Value) -> Option<i32> {
    match value {
        Value::Null => None,
        Value::Number(n) => n.as_i64().and_then(|n| i32::try_from(n).ok()),
        Value::String(s) => s.trim().parse::<i32>().ok(),
        Value::Bool(b) => Some(i32::from(b)),
        _ => None,
    }
}

fn value_to_i64(value: Value) -> Option<i64> {
    match value {
        Value::Null => None,
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Bool(b) => Some(i64::from(b)),
        _ => None,
    }
}

fn value_to_u64(value: Value) -> Option<u64> {
    match value {
        Value::Null => None,
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        Value::Bool(b) => Some(u64::from(b)),
        _ => None,
    }
}

fn deserialize_opt_string_from_any<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(value_to_string))
}

fn deserialize_opt_i32_from_any<'de, D>(deserializer: D) -> Result<Option<i32>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(value_to_i32))
}

fn deserialize_opt_i64_from_any<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(value_to_i64))
}

fn deserialize_opt_u64_from_any<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(value_to_u64))
}

fn deserialize_opt_vec_from_any<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(match value {
        None | Some(Value::Null) => None,
        Some(Value::Array(values)) => {
            let items = values
                .into_iter()
                .filter_map(|value| serde_json::from_value::<T>(value).ok())
                .collect::<Vec<_>>();
            Some(items)
        }
        Some(other) => serde_json::from_value::<T>(other)
            .ok()
            .map(|item| vec![item]),
    })
}

fn deserialize_opt_text_item_from_any<'de, D>(
    deserializer: D,
) -> Result<Option<ILinkTextItem>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(match value {
        None | Some(Value::Null) => None,
        Some(Value::Object(mut map)) => {
            let text = map
                .remove("text")
                .or_else(|| map.remove("content"))
                .or_else(|| map.remove("value"))
                .and_then(value_to_string);
            Some(ILinkTextItem { text })
        }
        Some(other) => Some(ILinkTextItem {
            text: value_to_string(other),
        }),
    })
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

fn item_type_to_message_type(item_type: i32) -> MessageType {
    match item_type {
        ITEM_IMAGE => MessageType::Photo,
        ITEM_VIDEO => MessageType::Video,
        ITEM_VOICE => MessageType::Voice,
        ITEM_FILE => MessageType::Document,
        _ => MessageType::Text,
    }
}

fn item_type_to_mime(item_type: i32) -> &'static str {
    match item_type {
        ITEM_IMAGE => "image/jpeg",
        ITEM_VIDEO => "video/mp4",
        ITEM_VOICE => "audio/amr",
        ITEM_FILE => "application/octet-stream",
        _ => "application/octet-stream",
    }
}

fn guess_mime_from_filename(filename: &str) -> Option<&'static str> {
    let lower = filename.to_lowercase();
    if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        Some("image/jpeg")
    } else if lower.ends_with(".png") {
        Some("image/png")
    } else if lower.ends_with(".gif") {
        Some("image/gif")
    } else if lower.ends_with(".mp4") {
        Some("video/mp4")
    } else if lower.ends_with(".amr") {
        Some("audio/amr")
    } else if lower.ends_with(".mp3") {
        Some("audio/mpeg")
    } else if lower.ends_with(".pdf") {
        Some("application/pdf")
    } else if lower.ends_with(".doc") || lower.ends_with(".docx") {
        Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document")
    } else {
        None
    }
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
        let mut end = keep;
        while end > 0 && !raw.is_char_boundary(end) {
            end -= 1;
        }
        raw[..end].to_string()
    }
}

fn parse_text_item_value(value: Option<&Value>) -> Option<ILinkTextItem> {
    match value.cloned() {
        None | Some(Value::Null) => None,
        Some(Value::Object(mut map)) => {
            let text = map
                .remove("text")
                .or_else(|| map.remove("content"))
                .or_else(|| map.remove("value"))
                .and_then(value_to_string);
            Some(ILinkTextItem { text })
        }
        Some(other) => Some(ILinkTextItem {
            text: value_to_string(other),
        }),
    }
}

fn parse_item_list_value(value: Option<&Value>) -> Option<Vec<ILinkItem>> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::Array(values)) => {
            let items = values
                .iter()
                .filter_map(parse_ilink_item_value)
                .collect::<Vec<_>>();
            Some(items)
        }
        Some(other) => parse_ilink_item_value(other).map(|item| vec![item]),
    }
}

fn parse_ilink_item_value(value: &Value) -> Option<ILinkItem> {
    let map = value.as_object()?;
    Some(ILinkItem {
        item_type: map.get("type").cloned().and_then(value_to_i32),
        seq: map.get("seq").cloned().and_then(value_to_i64),
        item_id: map.get("item_id").cloned().and_then(value_to_string),
        item_key: map.get("item_key").cloned().and_then(value_to_string),
        mime_type: map.get("mime_type").cloned().and_then(value_to_string),
        file_name: map.get("file_name").cloned().and_then(value_to_string),
        size: map.get("size").cloned().and_then(value_to_i64),
        create_time_ms: map.get("create_time_ms").cloned().and_then(value_to_i64),
        text_item: parse_text_item_value(map.get("text_item")),
        cdn_url: map.get("cdn_url").cloned().and_then(value_to_string),
        aes_key: map.get("aes_key").cloned().and_then(value_to_string),
        encrypted_query_param: map
            .get("encrypted_query_param")
            .cloned()
            .and_then(value_to_string),
        thumb_url: map.get("thumb_url").cloned().and_then(value_to_string),
    })
}

fn parse_ilink_msg_value(value: &Value) -> Option<ILinkMsg> {
    let map = value.as_object()?;
    Some(ILinkMsg {
        seq: map.get("seq").cloned().and_then(value_to_i64),
        message_id: map.get("message_id").cloned().and_then(value_to_string),
        from_user_id: map.get("from_user_id").cloned().and_then(value_to_string),
        to_user_id: map.get("to_user_id").cloned().and_then(value_to_string),
        client_id: map.get("client_id").cloned().and_then(value_to_string),
        create_time_ms: map.get("create_time_ms").cloned().and_then(value_to_i64),
        room_id: map
            .get("room_id")
            .or_else(|| map.get("group_id"))
            .cloned()
            .and_then(value_to_string),
        chat_room_id: map
            .get("chat_room_id")
            .or_else(|| map.get("session_id"))
            .cloned()
            .and_then(value_to_string),
        msg_type: map
            .get("msg_type")
            .or_else(|| map.get("message_type"))
            .cloned()
            .and_then(value_to_i32),
        message_state: map.get("message_state").cloned().and_then(value_to_i32),
        context_token: map.get("context_token").cloned().and_then(value_to_string),
        item_list: parse_item_list_value(map.get("item_list")),
    })
}

fn parse_get_updates_response(text: &str) -> Result<GetUpdatesResponse, serde_json::Error> {
    let root: Value = serde_json::from_str(text)?;
    let map = root.as_object().ok_or_else(|| {
        serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "getUpdates response must be a JSON object",
        ))
    })?;

    let sync_buf = map.get("sync_buf").cloned().and_then(value_to_string);
    let get_updates_buf = map
        .get("get_updates_buf")
        .cloned()
        .and_then(value_to_string)
        .or_else(|| sync_buf.clone());

    Ok(GetUpdatesResponse {
        ret: map.get("ret").cloned().and_then(value_to_i64),
        errcode: map.get("errcode").cloned().and_then(value_to_i64),
        errmsg: map.get("errmsg").cloned().and_then(value_to_string),
        get_updates_buf,
        sync_buf,
        longpolling_timeout_ms: map
            .get("longpolling_timeout_ms")
            .cloned()
            .and_then(value_to_u64),
        msgs: match map.get("msgs") {
            None | Some(Value::Null) => None,
            Some(Value::Array(values)) => Some(
                values
                    .iter()
                    .filter_map(parse_ilink_msg_value)
                    .collect::<Vec<_>>(),
            ),
            Some(other) => parse_ilink_msg_value(other).map(|msg| vec![msg]),
        },
    })
}

fn next_get_updates_buf(response: &GetUpdatesResponse) -> Option<String> {
    response
        .sync_buf
        .as_deref()
        .filter(|buf| !buf.is_empty())
        .map(|buf| buf.to_string())
        .or_else(|| {
            response
                .get_updates_buf
                .as_deref()
                .filter(|buf| !buf.is_empty())
                .map(|buf| buf.to_string())
        })
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
        self.root
            .join(format!("{}.context-tokens.json", account_id))
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
                warn!(
                    "[weixin] failed to read context tokens for {}: {}",
                    safe_id(Some(account_id), 8),
                    e
                );
                return;
            }
        };
        let data: HashMap<String, String> = match serde_json::from_str(&text) {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    "[weixin] failed to parse context tokens for {}: {}",
                    safe_id(Some(account_id), 8),
                    e
                );
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
            info!(
                "[weixin] restored {} context token(s) for {}",
                restored,
                safe_id(Some(account_id), 8)
            );
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
    last_prune: ParkMutex<std::time::Instant>,
}

impl MessageDedup {
    fn new(ttl_seconds: u64) -> Self {
        Self {
            ttl_seconds,
            seen: ParkMutex::new(HashMap::new()),
            last_prune: ParkMutex::new(std::time::Instant::now()),
        }
    }

    fn is_duplicate(&self, key: &str) -> bool {
        let mut seen = self.seen.lock();
        let now = std::time::Instant::now();
        if now.duration_since(*self.last_prune.lock()) > std::time::Duration::from_secs(60) {
            let ttl = std::time::Duration::from_secs(self.ttl_seconds);
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

fn is_dm_allowed(dm_policy: &str, allow_from: &[String], sender_id: &str) -> bool {
    match dm_policy {
        "disabled" => false,
        "allowlist" => allow_from.iter().any(|s| s == sender_id),
        _ => true,
    }
}

fn is_group_allowed(group_policy: &str, group_allow_from: &[String], chat_id: &str) -> bool {
    match group_policy {
        "disabled" => false,
        "allowlist" => group_allow_from.iter().any(|s| s == chat_id),
        _ => true,
    }
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

        let text = resp.text().await.context("read getUpdates response body")?;
        parse_get_updates_response(&text).with_context(|| {
            format!(
                "parse getUpdates response body={}",
                &text[..text.len().min(300)]
            )
        })
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
                "getConfig HTTP {}: {}",
                status,
                &text[..text.len().min(200)]
            );
        }

        resp.json().await.context("parse getConfig response")
    }

    // -----------------------------------------------------------------------
    // CDN media upload
    // -----------------------------------------------------------------------

    async fn get_upload_url(
        &self,
        file_type: i32,
        file_size: i64,
        file_name: &str,
        to_user_id: &str,
    ) -> Result<GetUploadUrlResponse> {
        let payload = GetUploadUrlPayload {
            file_type,
            file_size,
            file_name: file_name.to_string(),
            to_user_id: Some(to_user_id.to_string()),
            base_info: base_info(),
        };
        let resp = self
            .api_post(EP_GET_UPLOAD_URL, payload)
            .await
            .context("getUploadUrl request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "getUploadUrl HTTP {}: {}",
                status,
                &text[..text.len().min(200)]
            );
        }

        resp.json().await.context("parse getUploadUrl response")
    }

    async fn upload_media_bytes(
        &self,
        data: &[u8],
        aes_key: &[u8],
        upload_url: &str,
    ) -> Result<()> {
        let encrypted = aes_encrypt_ecb(aes_key, data);

        for (idx, chunk) in encrypted.chunks(UPLOAD_CHUNK_SIZE).enumerate() {
            let resp = self
                .client
                .put(upload_url)
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", chunk.len().to_string())
                .body(chunk.to_vec())
                .send()
                .await
                .with_context(|| format!("upload chunk {} failed", idx))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!(
                    "upload chunk {} HTTP {}: {}",
                    idx,
                    status,
                    &text[..text.len().min(200)]
                );
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    async fn download_and_decrypt_media(
        &self,
        cdn_url: &str,
        aes_key_raw: &str,
    ) -> Result<Vec<u8>> {
        let aes_key = decode_aes_key(aes_key_raw)?;

        let resp = self
            .client
            .get(cdn_url)
            .send()
            .await
            .context("download media from CDN")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "CDN download HTTP {}: {}",
                status,
                &text[..text.len().min(200)]
            );
        }

        let encrypted = resp.bytes().await.context("read CDN response body")?;

        aes_decrypt_ecb(&aes_key, &encrypted)
    }

    // -----------------------------------------------------------------------
    // Polling
    // -----------------------------------------------------------------------

    fn build_poll_context(
        &self,
        handler: tokio::sync::mpsc::Sender<InboundMessage>,
        account_id: String,
    ) -> PollContext {
        PollContext {
            channel: WeixinChannel {
                account_id: Some(account_id.clone()),
                token: self.token.clone(),
                base_url: self.base_url.clone(),
                cdn_base_url: String::new(),
                dm_policy: self.dm_policy.clone(),
                group_policy: self.group_policy.clone(),
                allow_from: self.allow_from.clone(),
                group_allow_from: self.group_allow_from.clone(),
                connected: false,
                running: self.running.clone(),
                message_handler: None,
                client: self.client.clone(),
                token_store: self.token_store.clone(),
                typing_cache: self.typing_cache.clone(),
                dedup: self.dedup.clone(),
                sync_buf: self.sync_buf.clone(),
            },
            handler,
            account_id,
        }
    }
}

// ---------------------------------------------------------------------------
// PollContext — owned state for the polling task
// ---------------------------------------------------------------------------

struct PollContext {
    channel: WeixinChannel,
    handler: tokio::sync::mpsc::Sender<InboundMessage>,
    account_id: String,
}

impl PollContext {
    async fn run(self) {
        let mut timeout_ms = LONG_POLL_TIMEOUT_MS;
        let mut consecutive_failures: u32 = 0;
        let running = self.channel.running.clone();
        let sync_buf = self.channel.sync_buf.clone();
        let account_id = self.account_id.clone();
        let token_store = self.channel.token_store.clone();
        let dedup = self.channel.dedup.clone();

        info!(
            "[weixin] Polling task started account={}",
            safe_id(Some(&account_id), 8)
        );

        while running.load(Ordering::Relaxed) {
            let sync_buf_val = sync_buf.lock().clone();
            match self.channel.get_updates(&sync_buf_val, timeout_ms).await {
                Ok(response) => {
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
                            error!(
                                "[weixin] Session expired; pausing for {}s",
                                SESSION_EXPIRED_PAUSE_SECS
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(
                                SESSION_EXPIRED_PAUSE_SECS,
                            ))
                            .await;
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

                    if let Some(new_buf) = next_get_updates_buf(&response) {
                        *sync_buf.lock() = new_buf;
                    }

                    if let Some(msgs) = response.msgs {
                        for msg in msgs {
                            let handler = self.handler.clone();
                            let ts = token_store.clone();
                            let ded = dedup.clone();
                            let aid = account_id.clone();
                            let dm = self.channel.dm_policy.clone();
                            let gp = self.channel.group_policy.clone();
                            let af = self.channel.allow_from.clone();
                            let gaf = self.channel.group_allow_from.clone();
                            tokio::spawn(async move {
                                if let Err(e) = process_message(
                                    msg, handler, &aid, ts, ded, &dm, &gp, &af, &gaf,
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
}

#[allow(clippy::too_many_arguments)]
async fn process_message(
    message: ILinkMsg,
    handler: tokio::sync::mpsc::Sender<InboundMessage>,
    account_id: &str,
    token_store: Arc<ContextTokenStore>,
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
        if !is_group_allowed(group_policy, group_allow_from, &effective_chat_id) {
            return Ok(());
        }
    } else if !is_dm_allowed(dm_policy, allow_from, sender_id) {
        return Ok(());
    }

    let context_token = message.context_token.as_deref().unwrap_or("").trim();
    if !context_token.is_empty() {
        token_store.set(account_id, sender_id, context_token);
    }

    let media_items: Vec<&ILinkItem> = item_list
        .iter()
        .filter(|item| {
            matches!(
                item.item_type,
                Some(ITEM_IMAGE) | Some(ITEM_VIDEO) | Some(ITEM_VOICE) | Some(ITEM_FILE)
            )
        })
        .collect();

    if text.is_empty() && media_items.is_empty() {
        return Ok(());
    }

    let mut media = Vec::new();
    let mut primary_type = MessageType::Text;
    let mut aes_key_raw: Option<String> = None;

    for media_item in &media_items {
        let item_type = media_item.item_type.unwrap_or(ITEM_FILE);

        let cdn_url = match media_item.cdn_url.as_deref() {
            Some(u) if !u.is_empty() => u.to_string(),
            _ => {
                debug!(
                    "[weixin] media item type={} has no cdn_url, skipping",
                    item_type
                );
                continue;
            }
        };

        if !is_cdn_url_allowed(&cdn_url) {
            warn!(
                "[weixin] CDN URL domain not whitelisted, skipping: {}",
                &cdn_url[..cdn_url.len().min(80)]
            );
            continue;
        }

        let key_for_item = resolve_aes_key(media_item);
        if aes_key_raw.is_none() {
            aes_key_raw = key_for_item;
        }
        let file_name = media_item
            .file_name
            .clone()
            .unwrap_or_else(|| format!("media_{}", item_type));

        let mime = media_item
            .mime_type
            .as_deref()
            .and_then(|m| if m.is_empty() { None } else { Some(m) })
            .or_else(|| guess_mime_from_filename(&file_name))
            .unwrap_or_else(|| item_type_to_mime(item_type));

        let file_size = media_item
            .size
            .and_then(|s| if s > 0 { Some(s as u64) } else { None });

        let attachment = MediaAttachment {
            url: cdn_url,
            mime_type: Some(mime.to_string()),
            caption: None,
            file_name: Some(file_name),
            file_size,
        };

        media.push(attachment);

        if primary_type == MessageType::Text {
            primary_type = item_type_to_message_type(item_type);
        }
    }

    if primary_type != MessageType::Text {
        debug!(
            "[weixin] processing {} media attachment(s) from {}",
            media.len(),
            sender_id
        );
    }

    let content = if text.is_empty() && !media.is_empty() {
        format!(
            "[{}]",
            media
                .iter()
                .filter_map(|m| m.file_name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        text
    };

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
        content,
        media,
        metadata: {
            let mut m = HashMap::new();
            if let Some(raw_key) = aes_key_raw.as_ref() {
                m.insert("aes_key".to_string(), Value::String(raw_key.clone()));
            }
            m
        },
        source: Some(source),
        message_type: primary_type,
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

impl WeixinChannel {
    async fn send_media(
        &self,
        chat_id: &str,
        data: &[u8],
        file_name: &str,
        item_type: i32,
        caption: Option<&str>,
    ) -> Result<SendResult> {
        if !self.connected {
            return Ok(SendResult {
                success: false,
                message_id: None,
                error: Some("Not connected".to_string()),
                retryable: false,
            });
        }

        let max_size = match item_type {
            ITEM_IMAGE => MAX_IMAGE_SIZE,
            ITEM_VIDEO => MAX_VIDEO_SIZE,
            _ => MAX_FILE_SIZE,
        };
        if data.len() > max_size {
            return Ok(SendResult {
                success: false,
                message_id: None,
                error: Some(format!(
                    "File too large: {} bytes (max {} bytes)",
                    data.len(),
                    max_size
                )),
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

        let upload_resp = self
            .get_upload_url(item_type, data.len() as i64, file_name, chat_id)
            .await?;

        let ret = upload_resp.ret.unwrap_or(0);
        let errcode = upload_resp.errcode.unwrap_or(0);
        if ret != 0 || errcode != 0 {
            let errmsg = upload_resp
                .errmsg
                .unwrap_or_else(|| "unknown error".to_string());
            return Ok(SendResult {
                success: false,
                message_id: None,
                error: Some(format!(
                    "getUploadUrl error: ret={} errcode={} errmsg={}",
                    ret, errcode, errmsg
                )),
                retryable: false,
            });
        }

        let upload_url = match upload_resp.upload_url.as_deref() {
            Some(u) if !u.is_empty() => u.to_string(),
            _ => {
                return Ok(SendResult {
                    success: false,
                    message_id: None,
                    error: Some("getUploadUrl returned no upload_url".to_string()),
                    retryable: false,
                });
            }
        };

        let aes_key_raw = upload_resp.aes_key.as_deref().unwrap_or("AAAAAAAAAAAAAAAA");
        let aes_key = decode_aes_key(aes_key_raw).unwrap_or_else(|_| vec![0u8; 16]);

        if let Err(e) = self.upload_media_bytes(data, &aes_key, &upload_url).await {
            return Ok(SendResult {
                success: false,
                message_id: None,
                error: Some(format!("CDN upload failed: {}", e)),
                retryable: true,
            });
        }

        let client_id = format!("kestrel-weixin-{}", uuid::Uuid::new_v4());

        let mut item_list = vec![MessageItem {
            item_type,
            text_item: None,
        }];

        if let Some(caption_text) = caption {
            if !caption_text.is_empty() {
                item_list.push(MessageItem {
                    item_type: ITEM_TEXT,
                    text_item: Some(TextItem {
                        text: caption_text.to_string(),
                    }),
                });
            }
        }

        let payload = SendMessagePayload {
            msg: ILinkMessage {
                from_user_id: String::new(),
                to_user_id: chat_id.to_string(),
                client_id: client_id.clone(),
                message_type: MSG_TYPE_BOT,
                message_state: MSG_STATE_FINISH,
                item_list,
                context_token: context_token.map(|s| s.to_string()),
            },
            base_info: base_info(),
        };

        let resp = self
            .api_post(EP_SEND_MESSAGE, payload)
            .await
            .context("sendMessage (media) request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Ok(SendResult {
                success: false,
                message_id: None,
                error: Some(format!(
                    "sendMessage (media) HTTP {}: {}",
                    status,
                    &text[..text.len().min(200)]
                )),
                retryable: true,
            });
        }

        let send_resp: SendMessageResponse =
            resp.json().await.context("parse sendMessage response")?;
        let sret = send_resp.ret.unwrap_or(0);
        let serrcode = send_resp.errcode.unwrap_or(0);
        if sret != 0 || serrcode != 0 {
            let errmsg = send_resp
                .errmsg
                .or(send_resp.msg)
                .unwrap_or_else(|| "unknown error".to_string());
            return Ok(SendResult {
                success: false,
                message_id: None,
                error: Some(format!(
                    "sendMessage (media) error: ret={} errcode={} errmsg={}",
                    sret, serrcode, errmsg
                )),
                retryable: false,
            });
        }

        Ok(SendResult {
            success: true,
            message_id: Some(client_id),
            error: None,
            retryable: false,
        })
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
        let _token = match self.token.as_deref() {
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
                    "[weixin] Account ID not configured \
                     (set WEIXIN_ACCOUNT_ID or channels.weixin.account_id)"
                );
                return Ok(false);
            }
        };

        // Restore persisted context tokens for this account.
        self.token_store.restore(&account_id);

        self.running.store(true, Ordering::Relaxed);
        self.connected = true;

        if let Some(handler) = self.message_handler.clone() {
            let ctx = self.build_poll_context(handler, account_id);
            tokio::spawn(async move {
                ctx.run().await;
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
        chat_id: &str,
        image_url: &str,
        caption: Option<&str>,
    ) -> Result<SendResult> {
        if !self.connected {
            return Ok(SendResult {
                success: false,
                message_id: None,
                error: Some("Not connected".to_string()),
                retryable: false,
            });
        }

        let resp = self
            .client
            .get(image_url)
            .send()
            .await
            .context("download image for upload")?;

        if !resp.status().is_success() {
            return Ok(SendResult {
                success: false,
                message_id: None,
                error: Some(format!("Failed to download image: HTTP {}", resp.status())),
                retryable: true,
            });
        }

        let data = resp.bytes().await.context("read image bytes")?;
        let file_name = image_url
            .rsplit('/')
            .next()
            .unwrap_or("image.jpg")
            .to_string();

        self.send_media(chat_id, &data, &file_name, ITEM_IMAGE, caption)
            .await
    }

    fn set_message_handler(&mut self, handler: tokio::sync::mpsc::Sender<InboundMessage>) {
        self.message_handler = Some(handler);
    }
}

#[cfg(test)]
mod tests {
    use super::{next_get_updates_buf, GetUpdatesResponse, ITEM_TEXT};

    #[test]
    fn get_updates_response_accepts_sync_buf_alias() {
        let raw = r#"{
            "msgs": [],
            "sync_buf": "cursor-123",
            "longpolling_timeout_ms": "35000"
        }"#;

        let resp = super::parse_get_updates_response(raw).unwrap();

        assert_eq!(resp.get_updates_buf.as_deref(), Some("cursor-123"));
        assert_eq!(resp.sync_buf.as_deref(), Some("cursor-123"));
        assert_eq!(resp.longpolling_timeout_ms, Some(35_000));
    }

    #[test]
    fn get_updates_response_accepts_numeric_and_string_message_fields() {
        let raw = r#"{
            "ret": "0",
            "errcode": 0,
            "msgs": [{
                "message_id": 123456,
                "from_user_id": 987654,
                "to_user_id": "c0b055833755@im.bot",
                "msg_type": "1",
                "context_token": 42,
                "item_list": [{
                    "type": "1",
                    "text_item": { "text": 1001 }
                }]
            }]
        }"#;

        let resp: GetUpdatesResponse = serde_json::from_str(raw).unwrap();
        let msg = &resp.msgs.unwrap()[0];
        let item = &msg.item_list.as_ref().unwrap()[0];

        assert_eq!(resp.ret, Some(0));
        assert_eq!(resp.errcode, Some(0));
        assert_eq!(msg.message_id.as_deref(), Some("123456"));
        assert_eq!(msg.from_user_id.as_deref(), Some("987654"));
        assert_eq!(msg.msg_type, Some(1));
        assert_eq!(msg.context_token.as_deref(), Some("42"));
        assert_eq!(item.item_type, Some(ITEM_TEXT));
        assert_eq!(
            item.text_item
                .as_ref()
                .and_then(|text| text.text.as_deref()),
            Some("1001")
        );
    }

    #[test]
    fn get_updates_response_accepts_non_empty_msgs_with_drifted_fields() {
        let raw = r#"{
            "msgs": [{
                "seq": 3,
                "message_id": 7458333664558825224,
                "from_user_id": "o9cq803GbFB5tqE6gkXb5LGOTz3c@im.wechat",
                "to_user_id": "c0b055833755@im.bot",
                "client_id": "mmassistant_bypmsg_inbox_6222fbb41657740b5495ba1ac525ae73mmo9cq8029gO6b0ZdRInNgxlTlBtLk@weclaw282374_1778205313",
                "create_time_ms": 1778205314701,
                "message_type": "1",
                "message_state": "2",
                "context_token": 42,
                "item_list": [{
                    "type": 1,
                    "seq": 9,
                    "text_item": "hello from weixin"
                }]
            }],
            "sync_buf": "cursor-456"
        }"#;

        let resp = super::parse_get_updates_response(raw).unwrap();
        let msg = &resp.msgs.as_ref().unwrap()[0];
        let item = &msg.item_list.as_ref().unwrap()[0];

        assert_eq!(resp.get_updates_buf.as_deref(), Some("cursor-456"));
        assert_eq!(resp.sync_buf.as_deref(), Some("cursor-456"));
        assert_eq!(msg.seq, Some(3));
        assert_eq!(msg.message_id.as_deref(), Some("7458333664558825224"));
        assert_eq!(msg.client_id.as_deref(), Some("mmassistant_bypmsg_inbox_6222fbb41657740b5495ba1ac525ae73mmo9cq8029gO6b0ZdRInNgxlTlBtLk@weclaw282374_1778205313"));
        assert_eq!(msg.create_time_ms, Some(1_778_205_314_701));
        assert_eq!(msg.msg_type, Some(1));
        assert_eq!(msg.message_state, Some(2));
        assert_eq!(item.seq, Some(9));
        assert_eq!(
            item.text_item
                .as_ref()
                .and_then(|text| text.text.as_deref()),
            Some("hello from weixin")
        );
    }

    #[test]
    fn get_updates_response_accepts_live_replayed_payload_shape() {
        let raw = r#"{
            "msgs": [
                {
                    "seq": 3,
                    "message_id": 7458333664558825224,
                    "from_user_id": "o9cq803GbFB5tqE6gkXb5LGOTz3c@im.wechat",
                    "to_user_id": "c0b055833755@im.bot",
                    "client_id": "mmassistant_bypmsg_inbox_6222fbb41657740b5495ba1ac525ae73mmo9cq8029gO6b0ZdRInNgxlTlBtLk@weclaw282374_1778205313",
                    "create_time_ms": 1778205314701,
                    "update_time_ms": 1778205314800,
                    "delete_time_ms": 0,
                    "session_id": "",
                    "group_id": "",
                    "message_type": 1,
                    "message_state": 2,
                    "item_list": [
                        {
                            "type": 1,
                            "create_time_ms": 1778205314701,
                            "update_time_ms": 1778205314701,
                            "is_completed": true,
                            "button_item_list": [],
                            "text_item": {
                                "text": "你好"
                            }
                        }
                    ],
                    "context_token": "token-a"
                }
            ],
            "sync_buf": "live-sync",
            "get_updates_buf": "live-get"
        }"#;

        let resp = super::parse_get_updates_response(raw).unwrap();
        let msg = &resp.msgs.as_ref().unwrap()[0];
        let item = &msg.item_list.as_ref().unwrap()[0];

        assert_eq!(resp.sync_buf.as_deref(), Some("live-sync"));
        assert_eq!(resp.get_updates_buf.as_deref(), Some("live-get"));
        assert_eq!(next_get_updates_buf(&resp).as_deref(), Some("live-sync"));
        assert_eq!(msg.room_id.as_deref(), Some(""));
        assert_eq!(msg.chat_room_id.as_deref(), Some(""));
        assert_eq!(msg.msg_type, Some(1));
        assert_eq!(
            item.text_item
                .as_ref()
                .and_then(|text| text.text.as_deref()),
            Some("你好")
        );
    }

    #[test]
    fn next_get_updates_buf_falls_back_to_get_updates_buf() {
        let raw = r#"{
            "msgs": [],
            "get_updates_buf": "cursor-fallback"
        }"#;

        let resp = super::parse_get_updates_response(raw).unwrap();
        assert_eq!(
            next_get_updates_buf(&resp).as_deref(),
            Some("cursor-fallback")
        );
    }
}
