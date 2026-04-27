//! Stream consumer — bridges streaming chunks to platform message editing.
//!
//! Subscribes to `StreamChunk` broadcast, accumulates text, rate-limits edits,
//! and calls `edit_message` on the platform adapter. Ported from Hermes
//! `gateway/stream_consumer.py`.

use std::sync::Arc;

use kestrel_bus::events::{AgentEvent, StreamChunk};
use kestrel_config::schema::StreamingConfig;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::base::BaseChannel;

const TG_MAX_MESSAGE_LENGTH: usize = 4096;
const MAX_FLOOD_STRIKES: u32 = 3;

const OPEN_THINK_TAGS: &[&str] = &[
    "<REASONING_SCRATCHPAD>",
    "\u{1f9e0}",
    "<reasoning>",
    "<THINKING>",
    "<thinking>",
    "<thought>",
];

const CLOSE_THINK_TAGS: &[&str] = &[
    "</REASONING_SCRATCHPAD>",
    "\u{1fae0}",
    "</reasoning>",
    "</THINKING>",
    "</thinking>",
    "</thought>",
];

/// Manages progressive editing of a single platform message during streaming.
pub struct StreamConsumer {
    channel: Arc<dyn BaseChannel>,
    chat_id: String,
    session_key: String,
    cfg: StreamingConfig,
    stream_rx: broadcast::Receiver<StreamChunk>,
    event_rx: broadcast::Receiver<AgentEvent>,
    accumulated: String,
    message_id: Option<String>,
    last_sent_text: String,
    last_edit_time: std::time::Instant,
    edit_supported: bool,
    flood_strikes: u32,
    current_edit_interval: f64,
    in_think_block: bool,
    think_buffer: String,
}

impl StreamConsumer {
    /// Create a new stream consumer.
    pub fn new(
        channel: Arc<dyn BaseChannel>,
        chat_id: String,
        session_key: String,
        cfg: StreamingConfig,
        stream_rx: broadcast::Receiver<StreamChunk>,
        event_rx: broadcast::Receiver<AgentEvent>,
    ) -> Self {
        let current_edit_interval = cfg.edit_interval;
        Self {
            channel,
            chat_id,
            session_key,
            cfg,
            stream_rx,
            event_rx,
            accumulated: String::new(),
            message_id: None,
            last_sent_text: String::new(),
            last_edit_time: std::time::Instant::now() - std::time::Duration::from_secs(3600),
            edit_supported: true,
            flood_strikes: 0,
            current_edit_interval,
            in_think_block: false,
            think_buffer: String::new(),
        }
    }

    /// Run the consumer until the stream completes.
    ///
    /// Returns the final accumulated text and the message_id of the last
    /// edited/sent message (used by the caller to suppress duplicate sends).
    pub async fn run(mut self) -> (String, Option<String>) {
        let safe_limit = TG_MAX_MESSAGE_LENGTH
            .saturating_sub(self.cfg.cursor.len())
            .saturating_sub(100)
            .max(500);

        loop {
            // Drain all available chunks, filtering by session_key
            let mut got_done = false;
            loop {
                match self.stream_rx.try_recv() {
                    Ok(chunk) => {
                        // Drop chunks from other sessions
                        if chunk.session_key != self.session_key {
                            continue;
                        }
                        if chunk.done {
                            got_done = true;
                        } else {
                            self.filter_and_accumulate(&chunk.content);
                        }
                    }
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(broadcast::error::TryRecvError::Lagged(n)) => {
                        debug!("Stream consumer lagged by {n} chunks");
                        break;
                    }
                    Err(broadcast::error::TryRecvError::Closed) => {
                        got_done = true;
                        break;
                    }
                }
            }

            if got_done {
                self.flush_think_buffer();
            }

            // Check for tool call events (segment break)
            let mut tool_break = false;
            let mut tool_name_opt = None;
            let mut completed_tools: Vec<(String, u64)> = Vec::new();
            loop {
                match self.event_rx.try_recv() {
                    Ok(AgentEvent::ToolCall {
                        session_key,
                        tool_name,
                        ..
                    }) if session_key == self.session_key => {
                        tool_break = true;
                        tool_name_opt = Some(tool_name);
                    }
                    Ok(AgentEvent::ToolResult {
                        session_key,
                        tool_name,
                        duration_ms,
                        ..
                    }) if session_key == self.session_key => {
                        completed_tools.push((tool_name, duration_ms));
                    }
                    _ => break,
                }
            }

            // Batch-render completed tools into a single message.
            if !completed_tools.is_empty() {
                let mut lines = Vec::with_capacity(completed_tools.len());
                for (name, duration_ms) in &completed_tools {
                    let duration_str = if *duration_ms >= 1000 {
                        format!("{:.1}s", *duration_ms as f64 / 1000.0)
                    } else {
                        format!("{}ms", duration_ms)
                    };
                    lines.push(format!("\u{2705} `{}` done ({})", name, duration_str));
                }
                let reply_to = self.message_id.as_deref();
                let done_msg = lines.join("\n");
                let _ = self
                    .channel
                    .send_message(&self.chat_id, &done_msg, reply_to)
                    .await;
            }

            let elapsed = self.last_edit_time.elapsed().as_secs_f64();
            let should_edit = got_done
                || tool_break
                || (elapsed >= self.current_edit_interval && !self.accumulated.is_empty())
                || self.accumulated.len() >= self.cfg.buffer_threshold;

            if should_edit && !self.accumulated.is_empty() {
                // Handle oversized messages: split into chunks
                while self.accumulated.len() > safe_limit && self.edit_supported {
                    let limit = safe_limit.min(self.accumulated.len());
                    let split_at = self.accumulated[..limit].rfind('\n').unwrap_or(limit);
                    let chunk = self.accumulated[..split_at].to_string();
                    let ok = self.send_or_edit(&chunk, false).await;
                    if !ok {
                        warn!(
                            "Stream chunk split-and-send failed ({} bytes remaining), dropping rest",
                            self.accumulated.len()
                        );
                        break;
                    }
                    self.accumulated = self.accumulated[split_at..]
                        .trim_start_matches('\n')
                        .to_string();
                    self.message_id = None;
                    self.last_sent_text = String::new();
                }

                let mut display_text = self.accumulated.clone();
                if !got_done && !tool_break {
                    display_text.push_str(&self.cfg.cursor);
                }

                self.send_or_edit(&display_text, got_done || tool_break)
                    .await;
                self.last_edit_time = std::time::Instant::now();
            }

            // Handle tool break: send tool progress message, reset for next segment
            if tool_break {
                if let Some(tn) = tool_name_opt {
                    let reply_to = self.message_id.as_deref();
                    let tool_msg = format!("Using `{}`...", tn);
                    let _ = self
                        .channel
                        .send_message(&self.chat_id, &tool_msg, reply_to)
                        .await;
                }
                self.accumulated.clear();
                self.last_sent_text.clear();
                self.message_id = None;
            }

            if got_done {
                let msg_id = self.message_id.clone();
                return (self.accumulated.clone(), msg_id);
            }

            // Wait for the next chunk or the edit interval
            let interval = std::time::Duration::from_millis(50);
            tokio::time::sleep(interval).await;
        }
    }

    /// Get the current accumulated text.
    pub fn accumulated(&self) -> &str {
        &self.accumulated
    }

    /// Send or edit the message on the platform. Returns true on success.
    async fn send_or_edit(&mut self, text: &str, _finalize: bool) -> bool {
        let visible_without_cursor = text.replace(&self.cfg.cursor, "");
        if visible_without_cursor.trim().is_empty() {
            return true;
        }

        // Guard: don't create a brand-new message when the only visible content
        // is a handful of characters alongside the cursor.
        if self.message_id.is_none()
            && self.cfg.cursor.contains('\u{2589}')
            && text.contains(&self.cfg.cursor)
            && visible_without_cursor.trim().len() < 4
        {
            return true;
        }

        if let Some(ref mid) = self.message_id {
            if self.edit_supported {
                if text == self.last_sent_text {
                    return true;
                }
                let result = self.channel.edit_message(&self.chat_id, mid, text).await;

                match result {
                    Ok(r) if r.success => {
                        self.last_sent_text = text.to_string();
                        self.flood_strikes = 0;
                        true
                    }
                    Ok(r) => {
                        let is_flood = r
                            .error
                            .as_deref()
                            .map(|e| {
                                let e = e.to_lowercase();
                                e.contains("flood") || e.contains("retry after")
                            })
                            .unwrap_or(false);

                        if is_flood {
                            self.flood_strikes += 1;
                            self.current_edit_interval =
                                (self.current_edit_interval * 2.0).min(10.0);
                            debug!(
                                "Flood control on edit (strike {}/{}), backoff → {:.1}s",
                                self.flood_strikes, MAX_FLOOD_STRIKES, self.current_edit_interval
                            );
                            if self.flood_strikes < MAX_FLOOD_STRIKES {
                                self.last_edit_time = std::time::Instant::now();
                                return false;
                            }
                        }

                        // Non-flood or strikes exhausted: enter fallback mode
                        debug!(
                            "Edit failed (strikes={}), entering fallback mode",
                            self.flood_strikes
                        );
                        self.edit_supported = false;
                        false
                    }
                    Err(e) => {
                        warn!("Edit message error: {e}");
                        self.edit_supported = false;
                        false
                    }
                }
            } else {
                false
            }
        } else {
            // First message — send new
            let result = self.channel.send_message(&self.chat_id, text, None).await;

            match result {
                Ok(r) if r.success => {
                    if let Some(mid) = &r.message_id {
                        self.message_id = Some(mid.clone());
                    } else {
                        self.edit_supported = false;
                    }
                    self.last_sent_text = text.to_string();
                    true
                }
                Ok(_) => {
                    self.edit_supported = false;
                    false
                }
                Err(e) => {
                    warn!("Stream send error: {e}");
                    self.edit_supported = false;
                    false
                }
            }
        }
    }

    /// Filter think/reasoning blocks from the text and accumulate.
    fn filter_and_accumulate(&mut self, text: &str) {
        let buf = std::mem::take(&mut self.think_buffer) + text;

        let mut remaining = buf.as_str();
        while !remaining.is_empty() {
            if self.in_think_block {
                let (idx, len) = find_earliest_tag(remaining, CLOSE_THINK_TAGS);
                if len > 0 {
                    self.in_think_block = false;
                    remaining = &remaining[idx + len..];
                } else {
                    let max_tag = max_tag_len(CLOSE_THINK_TAGS);
                    if remaining.len() > max_tag {
                        self.think_buffer = remaining[remaining.len() - max_tag..].to_string();
                    } else {
                        self.think_buffer = remaining.to_string();
                    }
                    return;
                }
            } else {
                let (idx, len) = find_earliest_tag(remaining, OPEN_THINK_TAGS);
                if len > 0 {
                    // Emit text before the tag
                    self.accumulated.push_str(&remaining[..idx]);
                    self.in_think_block = true;
                    remaining = &remaining[idx + len..];
                } else {
                    // Check for partial tag at the tail
                    let held_back = find_partial_tag_suffix(remaining, OPEN_THINK_TAGS);
                    if held_back > 0 {
                        self.accumulated
                            .push_str(&remaining[..remaining.len() - held_back]);
                        self.think_buffer = remaining[remaining.len() - held_back..].to_string();
                    } else {
                        self.accumulated.push_str(remaining);
                    }
                    return;
                }
            }
        }
    }

    /// Flush any held-back partial tag buffer.
    fn flush_think_buffer(&mut self) {
        if !self.think_buffer.is_empty() && !self.in_think_block {
            self.accumulated.push_str(&self.think_buffer);
            self.think_buffer.clear();
        }
    }
}

fn find_earliest_tag(text: &str, tags: &[&str]) -> (usize, usize) {
    let mut best_idx = usize::MAX;
    let mut best_len = 0;
    for tag in tags {
        if let Some(idx) = text.find(tag) {
            if idx < best_idx {
                best_idx = idx;
                best_len = tag.len();
            }
        }
    }
    if best_len > 0 {
        (best_idx, best_len)
    } else {
        (0, 0)
    }
}

fn max_tag_len(tags: &[&str]) -> usize {
    tags.iter().map(|t| t.len()).max().unwrap_or(0)
}

fn find_partial_tag_suffix(text: &str, tags: &[&str]) -> usize {
    let mut held = 0;
    for tag in tags {
        for (i, _) in tag.char_indices() {
            if i == 0 || i > text.len() {
                continue;
            }
            if text.ends_with(&tag[..i]) && i > held {
                held = i;
            }
        }
    }
    held
}

/// Split text into chunks that fit within `limit` characters, respecting
/// newline boundaries.
pub fn split_message(text: &str, limit: usize) -> Vec<String> {
    if text.len() <= limit {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;
    while remaining.len() > limit {
        let cap = limit.min(remaining.len());
        let split_at = remaining[..cap].rfind('\n').unwrap_or(cap);
        chunks.push(remaining[..split_at].to_string());
        remaining = remaining[split_at..].trim_start_matches('\n');
    }
    if !remaining.is_empty() {
        chunks.push(remaining.to_string());
    }
    chunks
}
