//! Session data types.

use nanobot_core::{Message, MessageRole, SessionSource};
use serde::{Deserialize, Serialize};

/// A conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Unique session key (`platform:chat_id[:thread_id]`).
    pub key: String,

    /// Conversation message history.
    pub messages: Vec<SessionEntry>,

    /// Session metadata.
    #[serde(default)]
    pub metadata: SessionMetadata,

    /// Session source information.
    #[serde(default)]
    pub source: Option<SessionSource>,
}

impl Session {
    /// Create a new empty session with the given key.
    pub fn new(key: String) -> Self {
        Self {
            key,
            messages: Vec::new(),
            metadata: SessionMetadata::default(),
            source: None,
        }
    }

    /// Add a user message to the session.
    pub fn add_user_message(&mut self, content: String) {
        self.messages.push(SessionEntry {
            role: MessageRole::User,
            content,
            timestamp: Some(chrono::Local::now()),
            ..Default::default()
        });
    }

    /// Add an assistant message to the session.
    pub fn add_assistant_message(&mut self, content: String) {
        self.messages.push(SessionEntry {
            role: MessageRole::Assistant,
            content,
            timestamp: Some(chrono::Local::now()),
            ..Default::default()
        });
    }

    /// Add a system message to the session.
    pub fn add_system_message(&mut self, content: String) {
        self.messages.push(SessionEntry {
            role: MessageRole::System,
            content,
            timestamp: Some(chrono::Local::now()),
            ..Default::default()
        });
    }

    /// Add a tool result message to the session.
    pub fn add_tool_result(&mut self, tool_call_id: String, content: String) {
        self.messages.push(SessionEntry {
            role: MessageRole::Tool,
            content,
            tool_call_id: Some(tool_call_id),
            timestamp: Some(chrono::Local::now()),
            ..Default::default()
        });
    }

    /// Convert session entries to LLM-ready Messages.
    pub fn to_messages(&self) -> Vec<Message> {
        self.messages
            .iter()
            .map(|entry| Message {
                role: entry.role.clone(),
                content: entry.content.clone(),
                name: entry.name.clone(),
                tool_call_id: entry.tool_call_id.clone(),
                tool_calls: entry.tool_calls.clone(),
            })
            .collect()
    }

    /// Truncate history to keep only the last `max_messages` entries.
    /// Always keeps the first system message if present.
    pub fn truncate(&mut self, max_messages: usize) {
        if self.messages.len() <= max_messages {
            return;
        }

        // Preserve the first system message if it exists
        let system_msg = self
            .messages
            .first()
            .filter(|m| m.role == MessageRole::System)
            .cloned();

        // Keep the last N messages
        let mut truncated: Vec<SessionEntry> = self
            .messages
            .split_off(self.messages.len().saturating_sub(max_messages));

        // Re-prepend system message
        if let Some(sys) = system_msg {
            truncated.insert(0, sys);
        }

        self.messages = truncated;
        self.metadata.truncated = true;
    }

    /// Get the total token count estimate for the session.
    pub fn estimated_tokens(&self) -> usize {
        self.messages
            .iter()
            .map(|m| m.content.len() / 4) // rough estimate: 4 chars per token
            .sum()
    }

    /// Reset the session, clearing all messages.
    pub fn reset(&mut self) {
        self.messages.clear();
        self.metadata.truncated = false;
        self.metadata.turn_count = 0;
    }
}

/// A single entry in the session history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    /// Role of the message author (user, assistant, system, tool).
    pub role: MessageRole,
    /// Text content of the message.
    pub content: String,
    /// Optional sender name for function/tool message routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// ID linking a tool result back to its originating tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool calls requested by the assistant in this message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<nanobot_core::ToolCall>>,
    /// When this entry was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<chrono::DateTime<chrono::Local>>,
}

impl Default for SessionEntry {
    fn default() -> Self {
        Self {
            role: MessageRole::User,
            content: String::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            timestamp: Some(chrono::Local::now()),
        }
    }
}

/// Session metadata.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionMetadata {
    /// Total number of conversation turns.
    #[serde(default)]
    pub turn_count: usize,

    /// Whether the history has been truncated.
    #[serde(default)]
    pub truncated: bool,

    /// Creation timestamp.
    #[serde(default)]
    pub created_at: Option<chrono::DateTime<chrono::Local>>,

    /// Last activity timestamp.
    #[serde(default)]
    pub last_active: Option<chrono::DateTime<chrono::Local>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanobot_core::MessageRole;

    #[test]
    fn test_session_new() {
        let session = Session::new("test:key".to_string());
        assert_eq!(session.key, "test:key");
        assert!(session.messages.is_empty());
        assert!(session.source.is_none());
    }

    #[test]
    fn test_session_add_messages() {
        let mut session = Session::new("test:key".to_string());
        session.add_system_message("system prompt".to_string());
        session.add_user_message("hello".to_string());
        session.add_assistant_message("hi".to_string());
        session.add_tool_result("call_1".to_string(), "result data".to_string());

        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.messages[0].role, MessageRole::System);
        assert_eq!(session.messages[1].role, MessageRole::User);
        assert_eq!(session.messages[2].role, MessageRole::Assistant);
        assert_eq!(session.messages[3].role, MessageRole::Tool);
        assert_eq!(session.messages[3].tool_call_id, Some("call_1".to_string()));
    }

    #[test]
    fn test_session_to_messages() {
        let mut session = Session::new("test:key".to_string());
        session.add_system_message("system".to_string());
        session.add_user_message("hello".to_string());
        session.add_assistant_message("world".to_string());

        let messages: Vec<Message> = session.to_messages();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, MessageRole::System);
        assert_eq!(messages[0].content, "system");
        assert_eq!(messages[1].role, MessageRole::User);
        assert_eq!(messages[1].content, "hello");
        assert_eq!(messages[2].role, MessageRole::Assistant);
        assert_eq!(messages[2].content, "world");
    }

    #[test]
    fn test_session_truncate_preserves_system() {
        let mut session = Session::new("test:key".to_string());
        session.add_system_message("system prompt".to_string());
        for i in 0..10 {
            session.add_user_message(format!("message {}", i));
        }

        assert_eq!(session.messages.len(), 11);
        session.truncate(5);
        // System message + last 5 user messages = 6
        assert_eq!(session.messages.len(), 6);
        assert_eq!(session.messages[0].role, MessageRole::System);
        assert_eq!(session.messages[0].content, "system prompt");
        // Last 5 user messages kept (messages 5-9)
        assert_eq!(session.messages[1].content, "message 5");
        assert_eq!(session.messages[5].content, "message 9");
        assert!(session.metadata.truncated);
    }

    #[test]
    fn test_session_truncate_noop_when_small() {
        let mut session = Session::new("test:key".to_string());
        session.add_user_message("a".to_string());
        session.add_user_message("b".to_string());
        session.add_user_message("c".to_string());

        session.truncate(10);
        assert_eq!(session.messages.len(), 3);
        assert!(!session.metadata.truncated);
    }

    #[test]
    fn test_session_estimated_tokens() {
        let mut session = Session::new("test:key".to_string());
        // 8 chars = 2 tokens estimate
        session.add_user_message("abcd1234".to_string());
        // 12 chars = 3 tokens estimate
        session.add_assistant_message("hello world!".to_string());

        let tokens = session.estimated_tokens();
        assert_eq!(tokens, (8 / 4) + (12 / 4));
        assert_eq!(tokens, 5);
    }

    #[test]
    fn test_session_reset() {
        let mut session = Session::new("test:key".to_string());
        session.add_user_message("hello".to_string());
        session.add_assistant_message("world".to_string());
        session.metadata.truncated = true;
        session.metadata.turn_count = 5;

        session.reset();
        assert!(session.messages.is_empty());
        assert!(!session.metadata.truncated);
        assert_eq!(session.metadata.turn_count, 0);
    }

    #[test]
    fn test_session_entry_default() {
        let entry = SessionEntry::default();
        assert_eq!(entry.role, MessageRole::User);
        assert!(entry.content.is_empty());
        assert!(entry.name.is_none());
        assert!(entry.tool_call_id.is_none());
        assert!(entry.tool_calls.is_none());
        assert!(entry.timestamp.is_some());
    }
}
