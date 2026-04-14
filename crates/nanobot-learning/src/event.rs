//! Learning event types and event bus.
//!
//! Defines the [`LearningEvent`] enum capturing observable agent actions,
//! the [`LearningEventBus`] for async publish/subscribe, and the
//! [`LearningAction`] enum representing outcomes of processing events.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Default channel capacity for the learning event bus.
const LEARNING_BUS_CAPACITY: usize = 256;

/// Classification of tool-call errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClassification {
    /// Problem with user input.
    UserInput,
    /// Problem with tool configuration.
    ToolConfig,
    /// Environmental issue (network, permissions, etc.).
    Environment,
    /// Agent chose the wrong tool or strategy.
    AgentStrategy,
    /// Skill was incomplete or missing steps.
    SkillIncomplete,
}

/// Outcome of a skill invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillOutcome {
    /// Skill was helpful.
    Helpful,
    /// Skill was activated but irrelevant.
    Irrelevant,
    /// Skill caused problems.
    Harmful,
}

/// A learning event — something observable that the agent can learn from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LearningEvent {
    /// A tool invocation succeeded.
    ToolSucceeded {
        tool: String,
        args_summary: String,
        duration_ms: u64,
        context_hash: String,
        #[serde(with = "chrono::serde::ts_seconds")]
        timestamp: DateTime<Utc>,
    },
    /// A tool invocation failed.
    ToolFailed {
        tool: String,
        args_summary: String,
        error: ErrorClassification,
        error_message: String,
        retry_count: u32,
        #[serde(with = "chrono::serde::ts_seconds")]
        timestamp: DateTime<Utc>,
    },
    /// The user corrected the agent's behaviour.
    UserCorrection {
        original_action: String,
        correction_hint: String,
        topic: String,
        #[serde(with = "chrono::serde::ts_seconds")]
        timestamp: DateTime<Utc>,
    },
    /// The user approved or acknowledged an action.
    UserApproval {
        action_taken: String,
        implicit: bool,
        #[serde(with = "chrono::serde::ts_seconds")]
        timestamp: DateTime<Utc>,
    },
    /// A skill was used.
    SkillUsed {
        skill_name: String,
        match_score: f64,
        outcome: SkillOutcome,
        #[serde(with = "chrono::serde::ts_seconds")]
        timestamp: DateTime<Utc>,
    },
    /// A new skill was created.
    SkillCreated {
        skill_name: String,
        trigger_reason: String,
        source_session: String,
        #[serde(with = "chrono::serde::ts_seconds")]
        timestamp: DateTime<Utc>,
    },
    /// Memory was accessed.
    MemoryAccessed {
        query: String,
        results_count: usize,
        hit: bool,
        #[serde(with = "chrono::serde::ts_seconds")]
        timestamp: DateTime<Utc>,
    },
}

impl LearningEvent {
    /// Returns the timestamp of this event.
    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            Self::ToolSucceeded { timestamp, .. }
            | Self::ToolFailed { timestamp, .. }
            | Self::UserCorrection { timestamp, .. }
            | Self::UserApproval { timestamp, .. }
            | Self::SkillUsed { timestamp, .. }
            | Self::SkillCreated { timestamp, .. }
            | Self::MemoryAccessed { timestamp, .. } => *timestamp,
        }
    }
}

/// An action produced by processing a learning event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LearningAction {
    /// No action needed.
    NoOp,
    /// Adjust a skill's confidence score.
    AdjustConfidence {
        skill: String,
        delta: f64,
    },
    /// Propose creating a new skill.
    ProposeSkill {
        name: String,
        reason: String,
    },
    /// Propose patching an existing skill.
    PatchSkill {
        skill: String,
        description: String,
    },
    /// Deprecate a skill.
    DeprecateSkill {
        skill: String,
        reason: String,
    },
    /// Record an insight to long-term memory.
    RecordInsight {
        insight: String,
        category: String,
    },
}

/// Async event bus for learning events using tokio broadcast.
#[derive(Debug, Clone)]
pub struct LearningEventBus {
    tx: broadcast::Sender<LearningEvent>,
}

impl LearningEventBus {
    /// Creates a new event bus with the default channel capacity.
    pub fn new() -> Self {
        Self::with_capacity(LEARNING_BUS_CAPACITY)
    }

    /// Creates a new event bus with a custom channel capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publishes a learning event to all subscribers.
    ///
    /// Returns the number of receivers that received the event.
    /// If no receivers exist, the event is silently dropped.
    pub fn publish(&self, event: LearningEvent) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// Subscribes to learning events.
    ///
    /// The returned receiver will see events published after this call.
    /// Events published before subscribing are not replayed.
    pub fn subscribe(&self) -> broadcast::Receiver<LearningEvent> {
        self.tx.subscribe()
    }

    /// Returns a sender handle that can be used to publish events.
    pub fn sender(&self) -> broadcast::Sender<LearningEvent> {
        self.tx.clone()
    }
}

impl Default for LearningEventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_bus_publish_subscribe() {
        let bus = LearningEventBus::new();
        let mut rx = bus.subscribe();

        let event = LearningEvent::ToolSucceeded {
            tool: "shell".into(),
            args_summary: "ls -la".into(),
            duration_ms: 150,
            context_hash: "abc123".into(),
            timestamp: Utc::now(),
        };

        bus.publish(event.clone());
        let received = rx.try_recv().expect("should receive event");
        assert!(matches!(received, LearningEvent::ToolSucceeded { .. }));
    }

    #[test]
    fn event_bus_no_subscribers() {
        let bus = LearningEventBus::new();
        let event = LearningEvent::ToolSucceeded {
            tool: "shell".into(),
            args_summary: "echo hi".into(),
            duration_ms: 10,
            context_hash: "def".into(),
            timestamp: Utc::now(),
        };
        // Should not panic when no subscribers
        assert_eq!(bus.publish(event), 0);
    }

    #[test]
    fn event_bus_multiple_subscribers() {
        let bus = LearningEventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        let event = LearningEvent::MemoryAccessed {
            query: "test".into(),
            results_count: 3,
            hit: true,
            timestamp: Utc::now(),
        };

        assert_eq!(bus.publish(event), 2);
        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
    }

    #[test]
    fn event_timestamp() {
        let ts = Utc::now();
        let event = LearningEvent::SkillCreated {
            skill_name: "my-skill".into(),
            trigger_reason: "repeated pattern".into(),
            source_session: "sess-1".into(),
            timestamp: ts,
        };
        assert_eq!(event.timestamp(), ts);
    }

    #[test]
    fn event_serde_roundtrip() {
        let event = LearningEvent::ToolFailed {
            tool: "web_fetch".into(),
            args_summary: "https://example.com".into(),
            error: ErrorClassification::Environment,
            error_message: "connection timeout".into(),
            retry_count: 3,
            timestamp: Utc::now(),
        };
        let json = serde_json::to_string(&event).expect("serialize");
        let decoded: LearningEvent = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(decoded, LearningEvent::ToolFailed { .. }));
    }

    #[test]
    fn learning_action_serde() {
        let action = LearningAction::AdjustConfidence {
            skill: "my-skill".into(),
            delta: -0.1,
        };
        let json = serde_json::to_string(&action).expect("serialize");
        let decoded: LearningAction = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(action, decoded);
    }
}
