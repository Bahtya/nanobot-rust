//! Base channel trait — interface for all chat platform adapters.

use anyhow::Result;
use async_trait::async_trait;
use nanobot_bus::events::InboundMessage;
use nanobot_core::Platform;

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

    /// Send a typing indicator.
    async fn send_typing(&self, chat_id: &str) -> Result<()>;

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
