//! Bus event types — InboundMessage and OutboundMessage.
//!
//! These are the core data structures that flow through the message bus,
//! decoupling channels from agent processing.

use chrono::{DateTime, Local};
use kestrel_core::{MediaAttachment, MessageType, Platform, SessionSource};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A message arriving from a chat channel to the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    /// The platform this message came from.
    pub channel: Platform,

    /// The sender's unique ID on the platform.
    pub sender_id: String,

    /// The chat/session ID (DM or group).
    pub chat_id: String,

    /// Text content of the message.
    pub content: String,

    /// Attached media (images, files, etc.).
    #[serde(default)]
    pub media: Vec<MediaAttachment>,

    /// Extra metadata from the platform.
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,

    /// Session source information for routing.
    #[serde(default)]
    pub source: Option<SessionSource>,

    /// Message type classification.
    #[serde(default)]
    pub message_type: MessageType,

    /// Platform-specific message ID (for reply threading).
    #[serde(default)]
    pub message_id: Option<String>,

    /// Full-chain trace ID, injected from the channel and propagated through
    /// the entire processing pipeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,

    /// ID of the message this is replying to.
    #[serde(default)]
    pub reply_to: Option<String>,

    /// Timestamp of the message.
    #[serde(default = "default_timestamp")]
    pub timestamp: DateTime<Local>,
}

impl InboundMessage {
    /// Build the session key for routing this message to the correct session.
    pub fn session_key(&self) -> String {
        match &self.source {
            Some(src) => src.session_key(),
            None => format!("{}:{}", self.channel, self.chat_id),
        }
    }
}

fn default_timestamp() -> DateTime<Local> {
    Local::now()
}

/// A message being sent from the agent to a chat channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    /// The platform to send to.
    pub channel: Platform,

    /// The chat/session ID to send to.
    pub chat_id: String,

    /// Text content to send.
    pub content: String,

    /// ID of a message to reply to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,

    /// Full-chain trace ID propagated from the originating inbound message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,

    /// Media URLs to attach.
    #[serde(default)]
    pub media: Vec<String>,

    /// Extra metadata for platform-specific sending options.
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Streaming chunk for real-time output.
#[derive(Debug, Clone)]
pub struct StreamChunk {
    /// Session key this chunk belongs to.
    pub session_key: String,

    /// The text chunk.
    pub content: String,

    /// Whether this is the final chunk.
    pub done: bool,

    /// Full-chain trace ID propagated from the originating inbound message.
    pub trace_id: Option<String>,
}

/// Agent lifecycle event for hooks and monitoring.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Agent started processing a message.
    Started { session_key: String },

    /// Agent produced a streaming chunk.
    StreamingChunk {
        session_key: String,
        content: String,
    },

    /// Agent is executing a tool.
    ToolCall {
        session_key: String,
        tool_name: String,
        iteration: usize,
    },

    /// Agent completed processing.
    Completed {
        session_key: String,
        iterations: usize,
        tool_calls: usize,
    },

    /// Agent encountered an error.
    Error { session_key: String, error: String },

    /// A cron job fired.
    CronFired {
        job_id: String,
        job_name: Option<String>,
        message: String,
    },

    /// Heartbeat health check completed.
    HeartbeatCheck {
        healthy: bool,
        checks_total: usize,
        checks_failed: usize,
    },

    /// Heartbeat requests a component restart.
    RestartRequested { component: String, reason: String },

    /// Gateway is reconnecting (possibly with resume).
    GatewayReconnecting {
        platform: String,
        attempt: u32,
        resumable: bool,
    },

    /// Gateway successfully resumed a session.
    GatewayResumed {
        platform: String,
        session_id: String,
    },

    /// Gateway starting a fresh identify (old session lost).
    GatewayReidentify { platform: String },

    /// Health status changed (transition between healthy/degraded/unhealthy).
    HealthStatusChanged {
        /// Previous aggregate status.
        from: String,
        /// New aggregate status.
        to: String,
        /// Number of checks that are unhealthy.
        failed_count: usize,
        /// Number of checks that are degraded.
        degraded_count: usize,
    },

    /// Context window exceeded budget and messages were pruned.
    ContextOverflow {
        /// Session that overflowed.
        session_key: String,
        /// Estimated token count before pruning.
        tokens_before: usize,
        /// Estimated token count after pruning.
        tokens_after: usize,
        /// Number of messages removed.
        messages_removed: usize,
    },

    /// A single component's health status changed.
    ComponentStatusChanged {
        /// Component name (e.g. "provider", "bus").
        component: String,
        /// Previous status string.
        from: String,
        /// New status string.
        to: String,
        /// Human-readable detail message.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_inbound_message_session_key_with_source() {
        let source = SessionSource {
            platform: Platform::Telegram,
            chat_id: "chat123".to_string(),
            chat_name: None,
            chat_type: "dm".to_string(),
            user_id: Some("user456".to_string()),
            user_name: None,
            thread_id: Some("thread789".to_string()),
            chat_topic: None,
        };
        let msg = InboundMessage {
            channel: Platform::Telegram,
            sender_id: "user456".to_string(),
            chat_id: "chat123".to_string(),
            content: "hello".to_string(),
            media: vec![],
            metadata: HashMap::new(),
            source: Some(source),
            message_type: MessageType::Text,
            message_id: None,
            trace_id: None,
            reply_to: None,
            timestamp: default_timestamp(),
        };
        assert_eq!(msg.session_key(), "telegram:chat123:thread789");
    }

    #[tokio::test]
    async fn test_inbound_message_session_key_without_source() {
        let msg = InboundMessage {
            channel: Platform::Discord,
            sender_id: "user1".to_string(),
            chat_id: "chat99".to_string(),
            content: "hi".to_string(),
            media: vec![],
            metadata: HashMap::new(),
            source: None,
            message_type: MessageType::Text,
            message_id: None,
            trace_id: None,
            reply_to: None,
            timestamp: default_timestamp(),
        };
        assert_eq!(msg.session_key(), "discord:chat99");
    }

    #[tokio::test]
    async fn test_inbound_message_serde() {
        let msg = InboundMessage {
            channel: Platform::Slack,
            sender_id: "U12345".to_string(),
            chat_id: "C67890".to_string(),
            content: "test message".to_string(),
            media: vec![],
            metadata: HashMap::new(),
            source: None,
            message_type: MessageType::Text,
            message_id: Some("1234.5678".to_string()),
            trace_id: None,
            reply_to: None,
            timestamp: default_timestamp(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: InboundMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.channel, Platform::Slack);
        assert_eq!(back.sender_id, "U12345");
        assert_eq!(back.chat_id, "C67890");
        assert_eq!(back.content, "test message");
        assert_eq!(back.message_id.as_deref(), Some("1234.5678"));
    }

    #[tokio::test]
    async fn test_outbound_message_serde() {
        let msg = OutboundMessage {
            channel: Platform::Telegram,
            chat_id: "chat42".to_string(),
            content: "reply text".to_string(),
            reply_to: Some("msg99".to_string()),
            trace_id: None,
            media: vec!["http://example.com/img.png".to_string()],
            metadata: {
                let mut m = HashMap::new();
                m.insert("key".to_string(), serde_json::json!("value"));
                m
            },
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let back: OutboundMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.channel, Platform::Telegram);
        assert_eq!(back.chat_id, "chat42");
        assert_eq!(back.content, "reply text");
        assert_eq!(back.reply_to.as_deref(), Some("msg99"));
        assert_eq!(back.media.len(), 1);
        assert_eq!(back.metadata.get("key").unwrap(), "value");
    }
}
