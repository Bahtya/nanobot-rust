//! Base channel trait — interface for all chat platform adapters.

use anyhow::Result;
use async_trait::async_trait;
use kestrel_bus::events::InboundMessage;
use kestrel_core::Platform;
use tracing::debug;

/// Send result from a channel.
#[derive(Debug, Clone)]
pub struct SendResult {
    pub success: bool,
    pub message_id: Option<String>,
    pub error: Option<String>,
    pub retryable: bool,
}

/// Chat information from a platform.
#[derive(Debug, Clone)]
pub struct ChatInfo {
    pub chat_id: String,
    pub chat_name: Option<String>,
    pub chat_type: String,
    pub member_count: Option<u32>,
}

/// Base trait for chat platform adapters.
///
/// Each platform (Telegram, Discord, etc.) implements this trait
/// to provide a unified interface for sending/receiving messages.
/// Mirrors the Python `channels/base.py` BaseChannel.
#[async_trait]
pub trait BaseChannel: Send + Sync {
    /// Channel name (e.g., "telegram", "discord").
    fn name(&self) -> &str;

    /// The platform this channel handles.
    fn platform(&self) -> Platform;

    /// Whether the channel is currently connected.
    fn is_connected(&self) -> bool;

    /// Connect to the platform.
    async fn connect(&mut self) -> Result<bool>;

    /// Disconnect from the platform.
    async fn disconnect(&mut self) -> Result<()>;

    /// Send a text message.
    async fn send_message(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
    ) -> Result<SendResult>;

    /// Send a text message with trace context for request-response correlation.
    ///
    /// Default implementation delegates to [`send_message`](Self::send_message)
    /// ignoring the trace_id. Channels that support tracing (e.g. WebSocket)
    /// should override this to include the trace_id in outbound frames.
    async fn send_message_with_trace(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
        trace_id: Option<&str>,
    ) -> Result<SendResult> {
        if let Some(tid) = trace_id {
            debug!(trace_id = %tid, channel = %self.name(), "trace_id dropped — channel does not support tracing");
        }
        self.send_message(chat_id, content, reply_to).await
    }

    /// Send a typing indicator.
    async fn send_typing(&self, chat_id: &str, trace_id: Option<&str>) -> Result<()>;

    /// Send a reaction emoji to a message.
    ///
    /// Default no-op — platforms that don't support reactions can ignore this.
    async fn send_reaction(&self, _chat_id: &str, _message_id: &str, _emoji: &str) -> Result<()> {
        Ok(())
    }

    /// Send an image.
    async fn send_image(
        &self,
        chat_id: &str,
        image_url: &str,
        caption: Option<&str>,
    ) -> Result<SendResult>;

    /// Set the message handler for inbound messages.
    fn set_message_handler(&mut self, handler: tokio::sync::mpsc::Sender<InboundMessage>);
}
