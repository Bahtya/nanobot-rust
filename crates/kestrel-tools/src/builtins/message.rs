//! Message tool — send messages from the agent to users/channels.

use crate::trait_def::{Tool, ToolError};
use async_trait::async_trait;
use kestrel_bus::events::OutboundMessage;
use kestrel_core::Platform;
use serde_json::{json, Value};
use std::collections::HashMap;

/// Tool for sending messages via the bus.
pub struct MessageTool {
    bus_sender: Option<tokio::sync::mpsc::Sender<OutboundMessage>>,
    channel: Option<Platform>,
    chat_id: Option<String>,
}

impl MessageTool {
    pub fn new() -> Self {
        Self {
            bus_sender: None,
            channel: None,
            chat_id: None,
        }
    }

    /// Set the bus sender for delivering outbound messages to channels.
    pub fn with_bus(mut self, sender: tokio::sync::mpsc::Sender<OutboundMessage>) -> Self {
        self.bus_sender = Some(sender);
        self
    }

    /// Set the default channel and chat_id for routing messages when the caller
    /// does not specify them explicitly.
    pub fn with_default_channel(mut self, channel: Platform, chat_id: String) -> Self {
        self.channel = Some(channel);
        self.chat_id = Some(chat_id);
        self
    }
}

impl Default for MessageTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for MessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn description(&self) -> &str {
        "Send a message to the user. Use for proactive communication or delivering results."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "Message content to send" },
                "channel": { "type": "string", "description": "Channel to send to (default: current channel)" },
                "chat_id": { "type": "string", "description": "Chat ID to send to (default: current chat)" },
                "file": { "type": "string", "description": "File path to attach" },
            },
            "required": ["content"],
        })
    }

    fn is_mutating(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let content = args["content"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'content'".to_string()))?;

        if let Some(ref sender) = self.bus_sender {
            // Resolve the target channel — use args first, then the default.
            let channel = match args["channel"].as_str() {
                Some(ch) => serde_json::from_value::<Platform>(json!(ch))
                    .map_err(|e| ToolError::Validation(format!("Invalid channel: {}", e)))?,
                None => self.channel.clone().ok_or_else(|| {
                    ToolError::Validation("No channel specified and no default set".to_string())
                })?,
            };

            let chat_id = match args["chat_id"].as_str() {
                Some(id) => id.to_string(),
                None => self.chat_id.clone().ok_or_else(|| {
                    ToolError::Validation("No chat_id specified and no default set".to_string())
                })?,
            };

            // Collect optional file attachment as media.
            let media: Vec<String> = args["file"]
                .as_str()
                .map(|f| vec![f.to_string()])
                .unwrap_or_default();

            let msg = OutboundMessage {
                channel,
                chat_id,
                content: content.to_string(),
                reply_to: None,
                trace_id: None,
                media,
                metadata: HashMap::new(),
            };

            sender.send(msg).await.map_err(|_| {
                ToolError::Execution("Bus sender closed — could not deliver message".to_string())
            })?;

            Ok(format!("Message sent: {}", truncate(content, 100)))
        } else {
            // Fallback: text-only confirmation when no bus is wired.
            Ok(format!("Message sent: {}", truncate(content, 100)))
        }
    }
}

/// Truncate `s` to at most `max_len` characters.
fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        // Try to find a valid char boundary so we don't panic on multi-byte chars.
        match s.char_indices().nth(max_len) {
            Some((idx, _)) => &s[..idx],
            None => s,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_tool_metadata() {
        let tool = MessageTool::new();
        assert_eq!(tool.name(), "send_message");
        assert!(!tool.description().is_empty());
    }

    #[tokio::test]
    async fn test_message_tool_execute_short() {
        let tool = MessageTool::new();
        let result = tool
            .execute(serde_json::json!({
                "content": "Hello world"
            }))
            .await
            .unwrap();
        assert_eq!(result, "Message sent: Hello world");
    }

    #[tokio::test]
    async fn test_message_tool_execute_long() {
        let tool = MessageTool::new();
        let long_content = "x".repeat(200);
        let result = tool
            .execute(serde_json::json!({
                "content": long_content
            }))
            .await
            .unwrap();
        assert!(result.starts_with("Message sent: "));
        // Should be truncated to 100 chars
        assert!(result.len() < 120);
    }

    #[tokio::test]
    async fn test_message_tool_missing_content() {
        let tool = MessageTool::new();
        let result = tool.execute(serde_json::json!({})).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing 'content'"));
    }

    #[test]
    fn test_message_tool_default() {
        let tool = MessageTool::default();
        assert_eq!(tool.name(), "send_message");
    }

    #[test]
    fn test_message_tool_schema() {
        let tool = MessageTool::new();
        let schema = tool.parameters_schema();
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("content")));
    }

    #[tokio::test]
    async fn test_message_tool_with_bus_no_defaults() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<OutboundMessage>(8);
        let tool = MessageTool::new().with_bus(tx);

        // No channel/chat_id provided — should fail with validation error.
        let result = tool
            .execute(serde_json::json!({
                "content": "Hello"
            }))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("No channel specified"),
            "unexpected error: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_message_tool_with_bus_and_defaults() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<OutboundMessage>(8);
        let tool = MessageTool::new()
            .with_bus(tx)
            .with_default_channel(Platform::Telegram, "chat-42".to_string());

        tool.execute(serde_json::json!({
            "content": "Hello from bus"
        }))
        .await
        .unwrap();

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.channel, Platform::Telegram);
        assert_eq!(msg.chat_id, "chat-42");
        assert_eq!(msg.content, "Hello from bus");
        assert!(msg.media.is_empty());
    }

    #[tokio::test]
    async fn test_message_tool_with_bus_override_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<OutboundMessage>(8);
        let tool = MessageTool::new()
            .with_bus(tx)
            .with_default_channel(Platform::Telegram, "default-chat".to_string());

        tool.execute(serde_json::json!({
            "content": "Override test",
            "channel": "discord",
            "chat_id": "override-chat"
        }))
        .await
        .unwrap();

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.channel, Platform::Discord);
        assert_eq!(msg.chat_id, "override-chat");
    }

    #[tokio::test]
    async fn test_message_tool_with_bus_file_attachment() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<OutboundMessage>(8);
        let tool = MessageTool::new()
            .with_bus(tx)
            .with_default_channel(Platform::Local, "local-chat".to_string());

        tool.execute(serde_json::json!({
            "content": "See attached",
            "file": "/tmp/report.pdf"
        }))
        .await
        .unwrap();

        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.media, vec!["/tmp/report.pdf".to_string()]);
    }

    #[tokio::test]
    async fn test_message_tool_bus_sender_dropped() {
        let (tx, rx) = tokio::sync::mpsc::channel::<OutboundMessage>(8);
        let tool = MessageTool::new()
            .with_bus(tx)
            .with_default_channel(Platform::Local, "chat".to_string());

        // Drop the receiver so the send fails.
        drop(rx);

        let result = tool
            .execute(serde_json::json!({
                "content": "Hello"
            }))
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Bus sender closed"));
    }

    #[test]
    fn test_truncate_short() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_exact() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_long() {
        let s = "abcdefghij".repeat(3); // 30 chars
        let truncated = truncate(&s, 10);
        assert_eq!(truncated, "abcdefghij");
    }
}
