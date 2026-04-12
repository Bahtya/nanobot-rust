//! Core types shared across all nanobot crates.

use serde::{Deserialize, Serialize};

/// Supported chat platforms.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    /// Local / CLI interface.
    Local,
    /// Telegram messenger.
    Telegram,
    /// Discord messaging platform.
    Discord,
    /// WhatsApp messenger.
    Whatsapp,
    /// Slack collaboration platform.
    Slack,
    /// Signal messenger.
    Signal,
    /// Matrix decentralized chat protocol.
    Matrix,
    /// Email channel.
    Email,
    /// DingTalk (DingDing) messaging platform.
    Dingtalk,
    /// Feishu (Lark) collaboration platform.
    Feishu,
    /// WeCom (Enterprise WeChat).
    Wecom,
    /// WeChat messenger.
    Weixin,
    /// QQ messenger.
    QQ,
    /// MoChat messaging platform.
    Mochat,
    /// Built-in HTTP API server.
    ApiServer,
    /// Generic webhook endpoint.
    Webhook,
}

impl Platform {
    /// Returns the string identifier used in config and routing.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Telegram => "telegram",
            Self::Discord => "discord",
            Self::Whatsapp => "whatsapp",
            Self::Slack => "slack",
            Self::Signal => "signal",
            Self::Matrix => "matrix",
            Self::Email => "email",
            Self::Dingtalk => "dingtalk",
            Self::Feishu => "feishu",
            Self::Wecom => "wecom",
            Self::Weixin => "weixin",
            Self::QQ => "qq",
            Self::Mochat => "mochat",
            Self::ApiServer => "api_server",
            Self::Webhook => "webhook",
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Message content types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    /// Plain text message.
    #[default]
    Text,
    /// Geographic location.
    Location,
    /// Photo / image.
    Photo,
    /// Video clip.
    Video,
    /// Audio file.
    Audio,
    /// Voice recording.
    Voice,
    /// Arbitrary file / document attachment.
    Document,
    /// Sticker or emoji.
    Sticker,
    /// Bot command (e.g. `/start`).
    Command,
}

/// Media attachment in a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaAttachment {
    /// URL or local path to the media file.
    pub url: String,
    /// MIME type of the attachment (e.g. `"image/png"`).
    #[serde(default)]
    pub mime_type: Option<String>,
    /// Optional caption describing the media.
    #[serde(default)]
    pub caption: Option<String>,
    /// Original file name of the attachment.
    #[serde(default)]
    pub file_name: Option<String>,
    /// Size of the file in bytes.
    #[serde(default)]
    pub file_size: Option<u64>,
}

/// Role of a message in a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// System-level instruction.
    System,
    /// Message from the user.
    User,
    /// Message from the assistant / LLM.
    Assistant,
    /// Tool / function result.
    Tool,
}

/// A single message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Role of the message author.
    pub role: MessageRole,
    /// Text content of the message.
    pub content: String,
    /// Optional sender name (used for tool-call messages).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// ID correlating this message to a tool call request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool calls requested by the assistant in this message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

/// A tool call from the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique identifier for this tool call.
    pub id: String,
    /// Type of the call (always `"function"` for now).
    #[serde(rename = "type")]
    pub call_type: String,
    /// The function invocation details.
    pub function: FunctionCall,
}

/// A function call within a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    /// Name of the function to invoke.
    pub name: String,
    /// JSON-encoded arguments for the function.
    pub arguments: String,
}

/// Token usage from an LLM response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Number of tokens in the prompt.
    pub prompt_tokens: Option<u64>,
    /// Number of tokens in the completion.
    pub completion_tokens: Option<u64>,
    /// Total tokens (prompt + completion).
    pub total_tokens: Option<u64>,
}

/// Tool definition for the LLM API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Type of the tool (typically `"function"`).
    #[serde(rename = "type")]
    pub tool_type: String,
    /// The function schema definition.
    pub function: FunctionDefinition,
}

/// Function definition within a tool definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
    /// Name of the function.
    pub name: String,
    /// Human-readable description of what the function does.
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema describing the function parameters.
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
}

/// Result of an agent run.
#[derive(Debug, Clone)]
pub struct RunResult {
    /// Final text content produced by the agent.
    pub content: String,
    /// Token usage accumulated during the run.
    pub usage: Usage,
    /// Number of tool calls made during the run.
    pub tool_calls_made: usize,
    /// Number of agent-loop iterations consumed.
    pub iterations_used: usize,
}

/// Session source identifies where a conversation originates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSource {
    /// Platform the conversation originated from.
    pub platform: Platform,
    /// Unique identifier for the chat / conversation.
    pub chat_id: String,
    /// Human-readable name of the chat (group name, channel name, etc.).
    #[serde(default)]
    pub chat_name: Option<String>,
    /// Type of chat: `"dm"`, `"group"`, `"channel"`, or `"thread"`.
    #[serde(default = "default_chat_type")]
    pub chat_type: String,
    /// Unique identifier of the sender within the platform.
    #[serde(default)]
    pub user_id: Option<String>,
    /// Display name of the sender.
    #[serde(default)]
    pub user_name: Option<String>,
    /// Thread / topic ID within a group or channel, if applicable.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Topic or subject line of the chat.
    #[serde(default)]
    pub chat_topic: Option<String>,
}

fn default_chat_type() -> String {
    "dm".to_string()
}

impl SessionSource {
    /// Build the session key used for session lookup.
    pub fn session_key(&self) -> String {
        match &self.thread_id {
            Some(tid) => format!("{}:{}:{}", self.platform, self.chat_id, tid),
            None => format!("{}:{}", self.platform, self.chat_id),
        }
    }
}

/// Chat type classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatType {
    /// Direct (one-to-one) message.
    Dm,
    /// Group chat with multiple participants.
    Group,
    /// Broadcast channel.
    Channel,
    /// Thread within a group or channel.
    Thread,
}

/// Outcome of processing a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessingOutcome {
    /// Successfully processed and responded.
    Responded,
    /// Message was a command that was handled.
    CommandHandled,
    /// Message was filtered (e.g., from self, or empty).
    Filtered,
    /// Processing failed.
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_as_str() {
        assert_eq!(Platform::Local.as_str(), "local");
        assert_eq!(Platform::Telegram.as_str(), "telegram");
        assert_eq!(Platform::Discord.as_str(), "discord");
        assert_eq!(Platform::Whatsapp.as_str(), "whatsapp");
        assert_eq!(Platform::Slack.as_str(), "slack");
        assert_eq!(Platform::Signal.as_str(), "signal");
        assert_eq!(Platform::Matrix.as_str(), "matrix");
        assert_eq!(Platform::Email.as_str(), "email");
        assert_eq!(Platform::Dingtalk.as_str(), "dingtalk");
        assert_eq!(Platform::Feishu.as_str(), "feishu");
        assert_eq!(Platform::Wecom.as_str(), "wecom");
        assert_eq!(Platform::Weixin.as_str(), "weixin");
        assert_eq!(Platform::QQ.as_str(), "qq");
        assert_eq!(Platform::Mochat.as_str(), "mochat");
        assert_eq!(Platform::ApiServer.as_str(), "api_server");
        assert_eq!(Platform::Webhook.as_str(), "webhook");
    }

    #[test]
    fn test_platform_display() {
        assert_eq!(format!("{}", Platform::Telegram), "telegram");
        assert_eq!(format!("{}", Platform::Discord), "discord");
        assert_eq!(format!("{}", Platform::Local), "local");
        assert_eq!(format!("{}", Platform::ApiServer), "api_server");
    }

    #[test]
    fn test_platform_serde_roundtrip() {
        let variants = [
            Platform::Local,
            Platform::Telegram,
            Platform::Discord,
            Platform::Whatsapp,
            Platform::Slack,
            Platform::Signal,
            Platform::Matrix,
            Platform::Email,
            Platform::Dingtalk,
            Platform::Feishu,
            Platform::Wecom,
            Platform::Weixin,
            Platform::QQ,
            Platform::Mochat,
            Platform::ApiServer,
            Platform::Webhook,
        ];
        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let back: Platform = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, back);
        }
    }

    #[test]
    fn test_message_type_default() {
        let mt = MessageType::default();
        assert_eq!(mt, MessageType::Text);
    }

    #[test]
    fn test_message_role_serde() {
        let roles = [
            MessageRole::System,
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Tool,
        ];
        for role in roles {
            let json = serde_json::to_string(&role).unwrap();
            let back: MessageRole = serde_json::from_str(&json).unwrap();
            assert_eq!(role, back);
        }
    }

    #[test]
    fn test_message_construction() {
        let msg = Message {
            role: MessageRole::User,
            content: "hello".to_string(),
            name: Some("alice".to_string()),
            tool_call_id: None,
            tool_calls: None,
        };
        assert_eq!(msg.role, MessageRole::User);
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.name.as_deref(), Some("alice"));
        assert!(msg.tool_call_id.is_none());
        assert!(msg.tool_calls.is_none());
    }

    #[test]
    fn test_tool_call_construction() {
        let tc = ToolCall {
            id: "call_123".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "get_weather".to_string(),
                arguments: r#"{"city":"Tokyo"}"#.to_string(),
            },
        };
        assert_eq!(tc.id, "call_123");
        assert_eq!(tc.call_type, "function");
        assert_eq!(tc.function.name, "get_weather");
        assert_eq!(tc.function.arguments, r#"{"city":"Tokyo"}"#);
    }

    #[test]
    fn test_function_definition_construction() {
        let fd = FunctionDefinition {
            name: "search".to_string(),
            description: Some("Search the web".to_string()),
            parameters: Some(serde_json::json!({"type": "object"})),
        };
        assert_eq!(fd.name, "search");
        assert_eq!(fd.description.as_deref(), Some("Search the web"));
        assert!(fd.parameters.is_some());

        let fd_minimal = FunctionDefinition {
            name: "ping".to_string(),
            description: None,
            parameters: None,
        };
        assert_eq!(fd_minimal.name, "ping");
        assert!(fd_minimal.description.is_none());
        assert!(fd_minimal.parameters.is_none());
    }

    #[test]
    fn test_session_source_key() {
        let src_with_thread = SessionSource {
            platform: Platform::Telegram,
            chat_id: "chat42".to_string(),
            chat_name: None,
            chat_type: "dm".to_string(),
            user_id: None,
            user_name: None,
            thread_id: Some("thread99".to_string()),
            chat_topic: None,
        };
        assert_eq!(src_with_thread.session_key(), "telegram:chat42:thread99");

        let src_without_thread = SessionSource {
            platform: Platform::Discord,
            chat_id: "chat77".to_string(),
            chat_name: None,
            chat_type: "group".to_string(),
            user_id: None,
            user_name: None,
            thread_id: None,
            chat_topic: None,
        };
        assert_eq!(src_without_thread.session_key(), "discord:chat77");
    }

    #[test]
    fn test_usage_default() {
        let usage = Usage::default();
        assert!(usage.prompt_tokens.is_none());
        assert!(usage.completion_tokens.is_none());
        assert!(usage.total_tokens.is_none());
    }

    #[test]
    fn test_processing_outcome_variants() {
        let responded = ProcessingOutcome::Responded;
        let command = ProcessingOutcome::CommandHandled;
        let filtered = ProcessingOutcome::Filtered;
        let failed = ProcessingOutcome::Failed;

        assert_eq!(responded, ProcessingOutcome::Responded);
        assert_eq!(command, ProcessingOutcome::CommandHandled);
        assert_eq!(filtered, ProcessingOutcome::Filtered);
        assert_eq!(failed, ProcessingOutcome::Failed);
    }

    #[test]
    fn test_run_result_construction() {
        let rr = RunResult {
            content: "done".to_string(),
            usage: Usage {
                prompt_tokens: Some(10),
                completion_tokens: Some(20),
                total_tokens: Some(30),
            },
            tool_calls_made: 2,
            iterations_used: 3,
        };
        assert_eq!(rr.content, "done");
        assert_eq!(rr.usage.prompt_tokens, Some(10));
        assert_eq!(rr.tool_calls_made, 2);
        assert_eq!(rr.iterations_used, 3);
    }
}
