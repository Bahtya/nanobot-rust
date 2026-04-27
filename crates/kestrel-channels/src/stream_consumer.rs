//! Stream consumer — progressively edits a platform message with streamed tokens.
//!
//! Subscribes to the `StreamChunk` broadcast channel, buffers incoming deltas,
//! and calls `edit_message` on the platform adapter at a configurable rate.
//! Handles flood control with adaptive backoff and graceful fallback when
//! editing is no longer viable.

use std::sync::Arc;
use std::time::{Duration, Instant};

use kestrel_bus::events::{AgentEvent, StreamChunk};
use kestrel_config::schema::StreamDisplayConfig;
use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::base::BaseChannel;

/// Maximum consecutive flood-control failures before disabling edits.
const MAX_FLOOD_STRIKES: u32 = 3;

/// Maximum Telegram message length (leaving room for cursor + overhead).
const TG_SAFE_LIMIT: usize = 3900;

/// Stream consumer that progressively edits a platform message.
///
/// Created per-session when streaming is active. Subscribes to the
/// `StreamChunk` broadcast channel, accumulates text deltas, and calls
/// `edit_message` on the channel adapter at a rate-limited interval.
pub struct StreamConsumer {
    session_key: String,
    chat_id: String,
    reply_to: Option<String>,
    cfg: StreamDisplayConfig,
    channel: Arc<dyn BaseChannel>,
    stream_rx: broadcast::Receiver<StreamChunk>,
    event_rx: broadcast::Receiver<AgentEvent>,
}

impl StreamConsumer {
    /// Create a new stream consumer for a session.
    pub fn new(
        session_key: String,
        chat_id: String,
        reply_to: Option<String>,
        cfg: StreamDisplayConfig,
        channel: Arc<dyn BaseChannel>,
        stream_rx: broadcast::Receiver<StreamChunk>,
        event_rx: broadcast::Receiver<AgentEvent>,
    ) -> Self {
        Self {
            session_key,
            chat_id,
            reply_to,
            cfg,
            channel,
            stream_rx,
            event_rx,
        }
    }

    /// Run the stream consumer loop until the stream completes.
    ///
    /// Returns the message_id of the final edited message (if any).
    pub async fn run(mut self) -> Option<String> {
        let mut accumulated = String::new();
        let mut message_id: Option<String> = None;
        let mut last_edit = Instant::now();
        let mut last_sent_text = String::new();
        let mut edit_interval = Duration::from_secs_f64(self.cfg.edit_interval_secs);
        let mut flood_strikes: u32 = 0;
        let mut edit_supported = true;

        // Send initial placeholder message
        let initial_result = self
            .channel
            .send_message(
                &self.chat_id,
                &format!("{}{}", self.cfg.cursor, ""),
                self.reply_to.as_deref(),
            )
            .await;

        if initial_result.success {
            message_id = initial_result.message_id;
        }

        loop {
            // Drain available stream chunks
            let mut got_done = false;
            let mut tool_break = false;
            let mut tool_name = None;

            loop {
                match self.stream_rx.try_recv() {
                    Ok(chunk) => {
                        if chunk.session_key != self.session_key {
                            continue;
                        }
                        if chunk.done {
                            got_done = true;
                            break;
                        }
                        if !chunk.content.is_empty() {
                            accumulated.push_str(&chunk.content);
                        }
                    }
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(broadcast::error::TryRecvError::Lagged(n)) => {
                        debug!("Stream consumer lagged by {n} chunks");
                        continue;
                    }
                    Err(broadcast::error::TryRecvError::Closed) => {
                        got_done = true;
                        break;
                    }
                }
            }

            // Check for tool call events (segment break)
            loop {
                match self.event_rx.try_recv() {
                    Ok(AgentEvent::ToolCall {
                        session_key,
                        tool_name: tn,
                        ..
                    }) if session_key == self.session_key => {
                        tool_break = true;
                        tool_name = Some(tn);
                    }
                    _ => break,
                }
            }

            // Trim accumulated to safe limit
            if accumulated.len() > TG_SAFE_LIMIT {
                accumulated.truncate(TG_SAFE_LIMIT);
                accumulated.push_str("\n\n(truncated)");
            }

            // Decide whether to flush an edit
            let elapsed = last_edit.elapsed();
            let should_edit = got_done
                || tool_break
                || (elapsed >= edit_interval && !accumulated.is_empty())
                || accumulated.len() >= self.cfg.buffer_threshold;

            if should_edit && !accumulated.is_empty() {
                let mut display_text = accumulated.clone();
                if !got_done && !tool_break {
                    display_text.push_str(&self.cfg.cursor);
                }

                let delivered = if let Some(ref mid) = message_id {
                    if edit_supported {
                        match self
                            .channel
                            .edit_message(&self.chat_id, mid, &display_text)
                            .await
                        {
                            Ok(result) if result.success => {
                                last_sent_text = display_text.clone();
                                flood_strikes = 0;
                                edit_interval =
                                    Duration::from_secs_f64(self.cfg.edit_interval_secs);
                                true
                            }
                            Ok(result) => {
                                let err = result.error.unwrap_or_default();
                                let is_flood = err.to_lowercase().contains("flood")
                                    || err.to_lowercase().contains("retry after");

                                if is_flood {
                                    flood_strikes += 1;
                                    edit_interval =
                                        (edit_interval * 2).min(Duration::from_secs(10));
                                    debug!(
                                        "Flood control strike {flood_strikes}/{MAX_FLOOD_STRIKES}"
                                    );
                                    if flood_strikes >= MAX_FLOOD_STRIKES {
                                        edit_supported = false;
                                    }
                                }
                                false
                            }
                            Err(e) => {
                                warn!("edit_message error: {e}");
                                false
                            }
                        }
                    } else {
                        false
                    }
                } else if message_id.is_none() {
                    // First send
                    let text_to_send = if display_text.len() < 4 && !got_done {
                        // Too short for a standalone message — accumulate more
                        if got_done {
                            accumulated.clone()
                        } else {
                            continue;
                        }
                    } else {
                        display_text.clone()
                    };

                    match self
                        .channel
                        .send_message(&self.chat_id, &text_to_send, self.reply_to.as_deref())
                        .await
                    {
                        Ok(result) if result.success => {
                            message_id = result.message_id;
                            last_sent_text = text_to_send;
                            true
                        }
                        _ => false,
                    }
                } else {
                    false
                };

                if delivered {
                    last_edit = Instant::now();
                }
            }

            // Handle tool break: send tool progress message, reset for next segment
            if tool_break {
                if let Some(ref tn) = tool_name {
                    let tool_msg = format!("Using {tn}...");
                    let _ = self
                        .channel
                        .send_message(&self.chat_id, &tool_msg, message_id.as_deref())
                        .await;
                }
                // Reset segment state for next streaming phase
                accumulated.clear();
                last_sent_text.clear();
                message_id = None;
            }

            if got_done {
                // Final edit: remove cursor
                if !accumulated.is_empty() && edit_supported {
                    if let Some(ref mid) = message_id {
                        let _ = self
                            .channel
                            .edit_message(&self.chat_id, mid, &accumulated)
                            .await;
                    }
                }
                return message_id;
            }

            // Small yield to avoid busy-looping
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
