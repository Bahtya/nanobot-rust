//! Event processor trait and basic implementation.
//!
//! Defines [`LearningEventHandler`] for custom event processors and provides
//! [`BasicEventProcessor`] which tracks tool success/failure patterns and
//! user corrections.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::event::{LearningAction, LearningEvent};

/// Trait for processing learning events.
#[async_trait]
pub trait LearningEventHandler: Send + Sync {
    /// Handles a single learning event, optionally returning an action.
    async fn handle(&self, event: &LearningEvent) -> Vec<LearningAction>;

    /// Returns the processor name for logging and debugging.
    fn name(&self) -> &str;
}

/// Statistics tracked per tool by [`BasicEventProcessor`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolStats {
    /// Number of successful invocations.
    pub success_count: u64,
    /// Number of failed invocations.
    pub failure_count: u64,
    /// Map from error classification to count.
    pub error_breakdown: HashMap<String, u64>,
    /// Total duration in milliseconds for successful calls.
    pub total_duration_ms: u64,
}

impl ToolStats {
    /// Returns the success rate as a value between 0.0 and 1.0.
    pub fn success_rate(&self) -> f64 {
        let total = self.success_count + self.failure_count;
        if total == 0 {
            1.0
        } else {
            self.success_count as f64 / total as f64
        }
    }

    /// Returns the average duration of successful calls in milliseconds.
    pub fn avg_duration_ms(&self) -> f64 {
        if self.success_count == 0 {
            0.0
        } else {
            self.total_duration_ms as f64 / self.success_count as f64
        }
    }
}

/// Statistics tracked per topic by [`BasicEventProcessor`] for user corrections.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CorrectionStats {
    /// Number of corrections for this topic.
    pub count: u64,
    /// Most recent correction hint.
    pub last_hint: String,
    /// Timestamp of the last correction.
    pub last_seen: DateTime<Utc>,
}

/// Aggregate statistics from the [`BasicEventProcessor`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProcessorStats {
    /// Per-tool statistics.
    pub tools: HashMap<String, ToolStats>,
    /// Per-topic correction statistics.
    pub corrections: HashMap<String, CorrectionStats>,
    /// Total events processed.
    pub events_processed: u64,
}

/// A basic event processor that tracks tool success/failure patterns,
/// user corrections, and skill usage.
pub struct BasicEventProcessor {
    stats: Arc<RwLock<ProcessorStats>>,
}

impl BasicEventProcessor {
    /// Creates a new processor with empty statistics.
    pub fn new() -> Self {
        Self {
            stats: Arc::new(RwLock::new(ProcessorStats::default())),
        }
    }

    /// Returns a snapshot of the current processor statistics.
    pub fn stats(&self) -> ProcessorStats {
        self.stats.read().clone()
    }

    /// Resets all accumulated statistics.
    pub fn reset(&self) {
        *self.stats.write() = ProcessorStats::default();
    }
}

impl Default for BasicEventProcessor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LearningEventHandler for BasicEventProcessor {
    async fn handle(&self, event: &LearningEvent) -> Vec<LearningAction> {
        let mut actions = Vec::new();
        let mut stats = self.stats.write();
        stats.events_processed += 1;

        match event {
            LearningEvent::ToolSucceeded {
                tool,
                duration_ms,
                ..
            } => {
                let tool_stats = stats.tools.entry(tool.clone()).or_default();
                tool_stats.success_count += 1;
                tool_stats.total_duration_ms += duration_ms;
            }

            LearningEvent::ToolFailed {
                tool,
                error,
                error_message,
                retry_count,
                ..
            } => {
                let tool_stats = stats.tools.entry(tool.clone()).or_default();
                tool_stats.failure_count += 1;
                let err_key = format!("{error:?}");
                *tool_stats.error_breakdown.entry(err_key).or_insert(0) += 1;

                // If a tool fails repeatedly, record an insight.
                if *retry_count >= 3 || tool_stats.failure_count > 5 {
                    actions.push(LearningAction::RecordInsight {
                        insight: format!(
                            "Tool '{tool}' is unreliable: {} failures ({}). Last error: {error_message}",
                            tool_stats.failure_count,
                            error_message
                        ),
                        category: "tool_reliability".into(),
                    });
                }
            }

            LearningEvent::UserCorrection {
                original_action: _,
                correction_hint,
                topic,
                ..
            } => {
                let corr = stats.corrections.entry(topic.clone()).or_default();
                corr.count += 1;
                corr.last_hint = correction_hint.clone();
                corr.last_seen = Utc::now();

                // If we see repeated corrections on the same topic, flag it.
                if corr.count >= 2 {
                    actions.push(LearningAction::RecordInsight {
                        insight: format!(
                            "User corrected '{topic}' {} times. Hint: {correction_hint}",
                            corr.count
                        ),
                        category: "user_correction".into(),
                    });
                }
            }

            LearningEvent::SkillUsed {
                skill_name,
                match_score,
                outcome,
                ..
            } => {
                // Simple confidence adjustment based on outcome.
                match outcome {
                    crate::event::SkillOutcome::Helpful => {
                        let delta = 0.05 * (1.0 - *match_score);
                        actions.push(LearningAction::AdjustConfidence {
                            skill: skill_name.clone(),
                            delta,
                        });
                    }
                    crate::event::SkillOutcome::Irrelevant => {
                        actions.push(LearningAction::AdjustConfidence {
                            skill: skill_name.clone(),
                            delta: -0.1,
                        });
                    }
                    crate::event::SkillOutcome::Harmful => {
                        actions.push(LearningAction::AdjustConfidence {
                            skill: skill_name.clone(),
                            delta: -0.3,
                        });
                    }
                }
            }

            LearningEvent::UserApproval { .. }
            | LearningEvent::SkillCreated { .. }
            | LearningEvent::MemoryAccessed { .. } => {
                // No action from the basic processor for these.
            }
        }

        actions
    }

    fn name(&self) -> &str {
        "basic_event_processor"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ErrorClassification, SkillOutcome};

    fn tool_failed(tool: &str, error: ErrorClassification, retries: u32) -> LearningEvent {
        LearningEvent::ToolFailed {
            tool: tool.into(),
            args_summary: "args".into(),
            error,
            error_message: "something went wrong".into(),
            retry_count: retries,
            timestamp: Utc::now(),
        }
    }

    fn tool_ok(tool: &str, dur: u64) -> LearningEvent {
        LearningEvent::ToolSucceeded {
            tool: tool.into(),
            args_summary: "args".into(),
            duration_ms: dur,
            context_hash: "h".into(),
            timestamp: Utc::now(),
        }
    }

    fn user_correction(topic: &str, hint: &str) -> LearningEvent {
        LearningEvent::UserCorrection {
            original_action: "did X".into(),
            correction_hint: hint.into(),
            topic: topic.into(),
            timestamp: Utc::now(),
        }
    }

    fn skill_used(name: &str, score: f64, outcome: SkillOutcome) -> LearningEvent {
        LearningEvent::SkillUsed {
            skill_name: name.into(),
            match_score: score,
            outcome,
            timestamp: Utc::now(),
        }
    }

    #[tokio::test]
    async fn tracks_tool_success() {
        let proc = BasicEventProcessor::new();
        proc.handle(&tool_ok("shell", 100)).await;
        proc.handle(&tool_ok("shell", 200)).await;

        let stats = proc.stats();
        let ts = stats.tools.get("shell").expect("shell stats");
        assert_eq!(ts.success_count, 2);
        assert_eq!(ts.failure_count, 0);
        assert_eq!(ts.total_duration_ms, 300);
        assert!((ts.avg_duration_ms() - 150.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn tracks_tool_failure() {
        let proc = BasicEventProcessor::new();
        proc.handle(&tool_failed("web", ErrorClassification::Environment, 0))
            .await;

        let stats = proc.stats();
        let ts = stats.tools.get("web").expect("web stats");
        assert_eq!(ts.failure_count, 1);
        assert_eq!(ts.success_count, 0);
        assert_eq!(ts.success_rate(), 0.0);
    }

    #[tokio::test]
    async fn repeated_failures_emit_insight() {
        let proc = BasicEventProcessor::new();
        // 6 failures should trigger insight (threshold is failure_count > 5).
        for _ in 0..6 {
            proc.handle(&tool_failed("web", ErrorClassification::Environment, 0))
                .await;
        }
        let actions = proc
            .handle(&tool_failed("web", ErrorClassification::Environment, 0))
            .await;
        assert!(actions.iter().any(|a| matches!(
            a,
            LearningAction::RecordInsight { .. }
        )));
    }

    #[tokio::test]
    async fn high_retry_emits_insight() {
        let proc = BasicEventProcessor::new();
        let actions = proc
            .handle(&tool_failed("db", ErrorClassification::ToolConfig, 3))
            .await;
        assert!(actions.iter().any(|a| matches!(
            a,
            LearningAction::RecordInsight { .. }
        )));
    }

    #[tokio::test]
    async fn correction_tracking() {
        let proc = BasicEventProcessor::new();
        proc.handle(&user_correction("formatting", "use markdown"))
            .await;
        proc.handle(&user_correction("formatting", "use markdown please"))
            .await;

        let stats = proc.stats();
        let corr = stats.corrections.get("formatting").expect("correction");
        assert_eq!(corr.count, 2);

        // Second correction should emit an insight (count >= 2).
        let actions = proc
            .handle(&user_correction("formatting", "seriously, markdown"))
            .await;
        assert!(actions.iter().any(|a| matches!(
            a,
            LearningAction::RecordInsight { category, .. } if category == "user_correction"
        )));
    }

    #[tokio::test]
    async fn skill_confidence_adjustment() {
        let proc = BasicEventProcessor::new();

        let actions = proc.handle(&skill_used("deploy", 0.8, SkillOutcome::Helpful)).await;
        assert!(actions.iter().any(|a| matches!(
            a,
            LearningAction::AdjustConfidence { delta, .. } if *delta > 0.0
        )));

        let actions = proc.handle(&skill_used("deploy", 0.8, SkillOutcome::Harmful)).await;
        assert!(actions.iter().any(|a| matches!(
            a,
            LearningAction::AdjustConfidence { delta, .. } if *delta < 0.0
        )));
    }

    #[tokio::test]
    async fn events_processed_counter() {
        let proc = BasicEventProcessor::new();
        proc.handle(&tool_ok("a", 1)).await;
        proc.handle(&tool_ok("b", 2)).await;
        proc.handle(&tool_failed("c", ErrorClassification::AgentStrategy, 0))
            .await;

        assert_eq!(proc.stats().events_processed, 3);
    }

    #[tokio::test]
    async fn reset_clears_stats() {
        let proc = BasicEventProcessor::new();
        proc.handle(&tool_ok("x", 10)).await;
        assert_eq!(proc.stats().events_processed, 1);

        proc.reset();
        assert_eq!(proc.stats().events_processed, 0);
        assert!(proc.stats().tools.is_empty());
    }

    #[tokio::test]
    async fn processor_name() {
        let proc = BasicEventProcessor::new();
        assert_eq!(proc.name(), "basic_event_processor");
    }
}
