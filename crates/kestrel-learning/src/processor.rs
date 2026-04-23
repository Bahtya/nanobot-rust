//! Event processor trait and basic implementation.
//!
//! Defines [`LearningEventHandler`] for custom event processors and provides
//! [`BasicEventProcessor`] which tracks tool success/failure patterns and
//! user corrections.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kestrel_core::Result;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tracing;

use crate::event::{LearningAction, LearningEvent};

const PROCESSOR_STATS_VERSION: u32 = 3;
const SKILL_PATCH_SCORE_THRESHOLD: f64 = 0.85;
const SKILL_PROPOSE_SCORE_THRESHOLD: f64 = 0.25;
const SKILL_DEPRECATION_STREAK_THRESHOLD: u32 = 3;

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

/// Statistics tracked per skill by [`BasicEventProcessor`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillStats {
    /// Number of helpful outcomes observed for this skill.
    pub helpful_count: u64,
    /// Number of irrelevant outcomes observed for this skill.
    pub irrelevant_count: u64,
    /// Number of harmful outcomes observed for this skill.
    pub harmful_count: u64,
    /// Consecutive non-helpful outcomes (irrelevant or harmful).
    pub low_outcome_streak: u32,
}

/// Per-action-type outcome counts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActionOutcomeStats {
    /// Number of successful action executions.
    pub success_count: u64,
    /// Number of failed action executions.
    pub failure_count: u64,
}

/// Aggregate statistics from the [`BasicEventProcessor`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessorStats {
    /// Schema version for persisted processor stats.
    #[serde(default = "default_processor_stats_version")]
    pub version: u32,
    /// Per-tool statistics.
    pub tools: HashMap<String, ToolStats>,
    /// Per-topic correction statistics.
    pub corrections: HashMap<String, CorrectionStats>,
    /// Per-skill learning statistics.
    #[serde(default)]
    pub skills: HashMap<String, SkillStats>,
    /// Total events processed.
    pub events_processed: u64,
    /// Per-action-type outcome counts.
    #[serde(default)]
    pub action_outcomes: HashMap<String, ActionOutcomeStats>,
}

impl Default for ProcessorStats {
    fn default() -> Self {
        Self {
            version: PROCESSOR_STATS_VERSION,
            tools: HashMap::new(),
            corrections: HashMap::new(),
            skills: HashMap::new(),
            events_processed: 0,
            action_outcomes: HashMap::new(),
        }
    }
}

/// A basic event processor that tracks tool success/failure patterns,
/// user corrections, and skill usage.
///
/// Behavioral guidance insights are stored via `RecordInsight` and become
/// available to future prompts through the memory recall mechanism
/// (kestrel-memory warm store). They are NOT injected via a dedicated
/// prompt-adjustment channel.
pub struct BasicEventProcessor {
    stats: Arc<RwLock<ProcessorStats>>,
    stats_path: Option<PathBuf>,
}

impl BasicEventProcessor {
    /// Creates a new processor with empty statistics.
    pub fn new() -> Self {
        Self {
            stats: Arc::new(RwLock::new(ProcessorStats::default())),
            stats_path: None,
        }
    }

    /// Sets the file path for persisting statistics and returns self.
    ///
    /// When set, [`save_stats`](Self::save_stats) writes to this path and
    /// [`load_stats`](Self::load_stats) reads from it.
    pub fn with_stats_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.stats_path = Some(path.into());
        self
    }

    /// Returns a snapshot of the current processor statistics.
    pub fn stats(&self) -> ProcessorStats {
        self.stats.read().clone()
    }

    /// Resets all accumulated statistics.
    pub fn reset(&self) {
        *self.stats.write() = ProcessorStats::default();
    }

    /// Records the outcome of a learning action execution.
    pub fn record_action_outcome(&self, action_type: &str, success: bool) {
        let mut stats = self.stats.write();
        let entry = stats
            .action_outcomes
            .entry(action_type.to_string())
            .or_default();
        if success {
            entry.success_count += 1;
        } else {
            entry.failure_count += 1;
        }
    }

    /// Persists the current statistics to the configured file path.
    ///
    /// Uses atomic write (temp file + rename) to avoid corruption on crash.
    /// Returns `Ok(())` if no stats path is configured (no-op).
    pub async fn save_stats(&self) -> Result<()> {
        let Some(path) = &self.stats_path else {
            return Ok(());
        };

        let snapshot = {
            let guard = self.stats.read();
            serde_json::to_string_pretty(&*guard)?
        };

        // Ensure parent directory exists.
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Atomic write: write to temp file, then rename.
        let tmp_path = path.with_extension("tmp");
        tokio::fs::write(&tmp_path, &snapshot).await?;
        tokio::fs::rename(&tmp_path, path).await?;

        tracing::debug!("Saved processor stats to {}", path.display());
        Ok(())
    }

    /// Loads statistics from the configured file path, replacing in-memory state.
    ///
    /// If the file does not exist, stats are left as default (graceful degradation).
    /// Returns an error if the file exists but cannot be parsed.
    pub async fn load_stats(&mut self) -> Result<()> {
        let Some(path) = &self.stats_path else {
            return Ok(());
        };

        if !path.exists() {
            tracing::debug!(
                "No stats file at {}, starting with empty stats",
                path.display()
            );
            return Ok(());
        }

        let data = tokio::fs::read_to_string(path).await?;
        let mut loaded: ProcessorStats = serde_json::from_str(&data)?;
        if loaded.version != PROCESSOR_STATS_VERSION {
            tracing::warn!(
                "Processor stats version mismatch at {}: found {}, expected {}",
                path.display(),
                loaded.version,
                PROCESSOR_STATS_VERSION
            );
            loaded = Self::migrate_stats(loaded);
        }
        *self.stats.write() = loaded;

        tracing::debug!("Loaded processor stats from {}", path.display());
        Ok(())
    }

    /// Migrate loaded stats into the current in-memory schema version.
    fn migrate_stats(mut stats: ProcessorStats) -> ProcessorStats {
        stats.version = PROCESSOR_STATS_VERSION;
        stats
    }
}

impl Default for BasicEventProcessor {
    fn default() -> Self {
        Self::new()
    }
}

fn default_processor_stats_version() -> u32 {
    PROCESSOR_STATS_VERSION
}

#[async_trait]
impl LearningEventHandler for BasicEventProcessor {
    async fn handle(&self, event: &LearningEvent) -> Vec<LearningAction> {
        let mut actions = Vec::new();
        let mut stats = self.stats.write();
        stats.events_processed += 1;

        match event {
            LearningEvent::ToolSucceeded {
                tool, duration_ms, ..
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

                // If a tool fails repeatedly, record an insight and adjust the prompt.
                if *retry_count >= 3 || tool_stats.failure_count > 5 {
                    actions.push(LearningAction::RecordInsight {
                        insight: format!(
                            "Tool '{tool}' is unreliable: {} failures ({}). Last error: {error_message}",
                            tool_stats.failure_count,
                            error_message
                        ),
                        category: "tool_reliability".into(),
                    });
                    // Store behavioral guidance for later memory-based recall.
                    actions.push(LearningAction::RecordInsight {
                        insight: format!(
                            "Consider alternatives to tool '{tool}' — it has failed {} times. \
                             Last issue: {error_message}",
                            tool_stats.failure_count
                        ),
                        category: "behavioral_guidance".into(),
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

                // If we see repeated corrections on the same topic, flag it and adjust the prompt.
                if corr.count >= 2 {
                    actions.push(LearningAction::RecordInsight {
                        insight: format!(
                            "User corrected '{topic}' {} times. Hint: {correction_hint}",
                            corr.count
                        ),
                        category: "user_correction".into(),
                    });
                    // Store behavioral guidance for later memory-based recall.
                    actions.push(LearningAction::RecordInsight {
                        insight: format!(
                            "Always follow user preference for '{topic}'. Rule: {correction_hint}"
                        ),
                        category: "behavioral_guidance".into(),
                    });
                }
            }

            LearningEvent::SkillUsed {
                skill_name,
                match_score,
                outcome,
                ..
            } => {
                let skill_stats = stats.skills.entry(skill_name.clone()).or_default();

                match outcome {
                    crate::event::SkillOutcome::Helpful => {
                        skill_stats.helpful_count += 1;
                        skill_stats.low_outcome_streak = 0;
                        let delta = 0.05 * (1.0 - *match_score);
                        actions.push(LearningAction::AdjustConfidence {
                            skill: skill_name.clone(),
                            delta,
                        });

                        if *match_score >= SKILL_PATCH_SCORE_THRESHOLD {
                            actions.push(LearningAction::PatchSkill {
                                skill: skill_name.clone(),
                                description: format!(
                                    "Observed repeated helpful usage at match score {:.2}. Refine the instructions with the successful pattern that helped in this context.",
                                    match_score
                                ),
                            });
                        } else if *match_score <= SKILL_PROPOSE_SCORE_THRESHOLD {
                            actions.push(LearningAction::ProposeSkill {
                                name: format!("{skill_name}-variant"),
                                reason: format!(
                                    "Skill '{skill_name}' was still helpful despite a low match score of {:.2}. Consider creating a narrower companion skill or additional trigger coverage for this workflow.",
                                    match_score
                                ),
                            });
                        }
                    }
                    crate::event::SkillOutcome::Irrelevant => {
                        skill_stats.irrelevant_count += 1;
                        skill_stats.low_outcome_streak += 1;
                        actions.push(LearningAction::AdjustConfidence {
                            skill: skill_name.clone(),
                            delta: -0.1,
                        });
                    }
                    crate::event::SkillOutcome::Harmful => {
                        skill_stats.harmful_count += 1;
                        skill_stats.low_outcome_streak += 1;
                        actions.push(LearningAction::AdjustConfidence {
                            skill: skill_name.clone(),
                            delta: -0.3,
                        });
                    }
                }

                if skill_stats.low_outcome_streak >= SKILL_DEPRECATION_STREAK_THRESHOLD {
                    actions.push(LearningAction::DeprecateSkill {
                        skill: skill_name.clone(),
                        reason: format!(
                            "Skill produced {} consecutive non-helpful outcomes and should be reviewed or deprecated.",
                            skill_stats.low_outcome_streak
                        ),
                    });
                }
            }

            LearningEvent::TaskReflection {
                task_summary,
                tool_calls_count,
                success,
                reflection,
                ..
            } => {
                let status = if *success { "success" } else { "failure" };
                actions.push(LearningAction::RecordInsight {
                    insight: format!(
                        "Task '{task_summary}' ({tool_calls_count} tool calls, {status}): {reflection}"
                    ),
                    category: "task_reflection".into(),
                });
            }

            LearningEvent::UserApproval { .. }
            | LearningEvent::SkillCreated { .. }
            | LearningEvent::MemoryAccessed { .. }
            | LearningEvent::ReflectionFailed { .. } => {
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
            trace_id: None,
        }
    }

    fn tool_ok(tool: &str, dur: u64) -> LearningEvent {
        LearningEvent::ToolSucceeded {
            tool: tool.into(),
            args_summary: "args".into(),
            duration_ms: dur,
            context_hash: "h".into(),
            timestamp: Utc::now(),
            trace_id: None,
        }
    }

    fn user_correction(topic: &str, hint: &str) -> LearningEvent {
        LearningEvent::UserCorrection {
            original_action: "did X".into(),
            correction_hint: hint.into(),
            topic: topic.into(),
            timestamp: Utc::now(),
            trace_id: None,
        }
    }

    fn skill_used(name: &str, score: f64, outcome: SkillOutcome) -> LearningEvent {
        LearningEvent::SkillUsed {
            skill_name: name.into(),
            match_score: score,
            outcome,
            timestamp: Utc::now(),
            trace_id: None,
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
        assert!(actions
            .iter()
            .any(|a| matches!(a, LearningAction::RecordInsight { .. })));
    }

    #[tokio::test]
    async fn high_retry_emits_insight() {
        let proc = BasicEventProcessor::new();
        let actions = proc
            .handle(&tool_failed("db", ErrorClassification::ToolConfig, 3))
            .await;
        assert!(actions
            .iter()
            .any(|a| matches!(a, LearningAction::RecordInsight { .. })));
    }

    #[tokio::test]
    async fn repeated_failures_emit_behavioral_guidance() {
        let proc = BasicEventProcessor::new();
        // 6 failures exceed the threshold (failure_count > 5).
        for _ in 0..6 {
            proc.handle(&tool_failed("web", ErrorClassification::Environment, 0))
                .await;
        }
        let actions = proc
            .handle(&tool_failed("web", ErrorClassification::Environment, 0))
            .await;
        let behavioral_guidance: Vec<_> = actions
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    LearningAction::RecordInsight { category, .. } if category == "behavioral_guidance"
                )
            })
            .collect();
        assert_eq!(
            behavioral_guidance.len(),
            1,
            "should emit exactly one behavioral_guidance insight"
        );
        if let LearningAction::RecordInsight { insight, .. } = behavioral_guidance[0] {
            assert!(insight.contains("web"));
            assert!(insight.contains("Consider alternatives"));
        }
    }

    #[tokio::test]
    async fn high_retry_emits_behavioral_guidance() {
        let proc = BasicEventProcessor::new();
        let actions = proc
            .handle(&tool_failed("db", ErrorClassification::ToolConfig, 3))
            .await;
        assert!(actions.iter().any(|a| {
            matches!(
                a,
                LearningAction::RecordInsight { category, .. } if category == "behavioral_guidance"
            )
        }));
    }

    #[tokio::test]
    async fn single_failure_no_behavioral_guidance() {
        let proc = BasicEventProcessor::new();
        let actions = proc
            .handle(&tool_failed("web", ErrorClassification::Environment, 0))
            .await;
        assert!(!actions.iter().any(|a| {
            matches!(
                a,
                LearningAction::RecordInsight { category, .. } if category == "behavioral_guidance"
            )
        }));
    }

    #[tokio::test]
    async fn repeated_corrections_emit_behavioral_guidance() {
        let proc = BasicEventProcessor::new();
        proc.handle(&user_correction("formatting", "use markdown"))
            .await;
        let actions = proc
            .handle(&user_correction("formatting", "use markdown please"))
            .await;
        let behavioral_guidance: Vec<_> = actions
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    LearningAction::RecordInsight { category, .. } if category == "behavioral_guidance"
                )
            })
            .collect();
        assert_eq!(
            behavioral_guidance.len(),
            1,
            "should emit exactly one behavioral_guidance insight"
        );
        if let LearningAction::RecordInsight { insight, .. } = behavioral_guidance[0] {
            assert!(insight.contains("formatting"));
            assert!(insight.contains("use markdown please"));
        }
    }

    #[tokio::test]
    async fn single_correction_no_behavioral_guidance() {
        let proc = BasicEventProcessor::new();
        let actions = proc
            .handle(&user_correction("formatting", "use markdown"))
            .await;
        assert!(!actions.iter().any(|a| {
            matches!(
                a,
                LearningAction::RecordInsight { category, .. } if category == "behavioral_guidance"
            )
        }));
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

        let actions = proc
            .handle(&skill_used("deploy", 0.8, SkillOutcome::Helpful))
            .await;
        assert!(actions.iter().any(|a| matches!(
            a,
            LearningAction::AdjustConfidence { delta, .. } if *delta > 0.0
        )));

        let actions = proc
            .handle(&skill_used("deploy", 0.8, SkillOutcome::Harmful))
            .await;
        assert!(actions.iter().any(|a| matches!(
            a,
            LearningAction::AdjustConfidence { delta, .. } if *delta < 0.0
        )));
    }

    #[tokio::test]
    async fn helpful_high_score_emits_patch_skill() {
        let proc = BasicEventProcessor::new();
        let actions = proc
            .handle(&skill_used("deploy", 0.9, SkillOutcome::Helpful))
            .await;

        assert!(actions.iter().any(|a| matches!(
            a,
            LearningAction::PatchSkill { skill, .. } if skill == "deploy"
        )));
    }

    #[tokio::test]
    async fn helpful_low_score_emits_propose_skill() {
        let proc = BasicEventProcessor::new();
        let actions = proc
            .handle(&skill_used("deploy", 0.2, SkillOutcome::Helpful))
            .await;

        assert!(actions.iter().any(|a| matches!(
            a,
            LearningAction::ProposeSkill { name, .. } if name == "deploy-variant"
        )));
    }

    #[tokio::test]
    async fn repeated_non_helpful_skill_emits_deprecate_skill() {
        let proc = BasicEventProcessor::new();

        for _ in 0..2 {
            let actions = proc
                .handle(&skill_used("deploy", 0.6, SkillOutcome::Irrelevant))
                .await;
            assert!(!actions
                .iter()
                .any(|a| matches!(a, LearningAction::DeprecateSkill { .. })));
        }

        let actions = proc
            .handle(&skill_used("deploy", 0.6, SkillOutcome::Harmful))
            .await;
        assert!(actions.iter().any(|a| matches!(
            a,
            LearningAction::DeprecateSkill { skill, .. } if skill == "deploy"
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
        assert!(proc.stats().action_outcomes.is_empty());
    }

    #[tokio::test]
    async fn processor_name() {
        let proc = BasicEventProcessor::new();
        assert_eq!(proc.name(), "basic_event_processor");
    }

    #[tokio::test]
    async fn task_reflection_produces_insight() {
        let proc = BasicEventProcessor::new();
        let event = LearningEvent::TaskReflection {
            task_summary: "deploy to prod".into(),
            tool_calls_count: 3,
            success: true,
            reflection: "Smooth execution, all tools worked correctly.".into(),
            timestamp: Utc::now(),
            trace_id: None,
        };
        let actions = proc.handle(&event).await;
        assert_eq!(actions.len(), 1);
        assert!(actions.iter().any(|a| {
            matches!(
                a,
                LearningAction::RecordInsight { category, .. } if category == "task_reflection"
            )
        }));
        if let LearningAction::RecordInsight { insight, .. } = &actions[0] {
            assert!(insight.contains("deploy to prod"));
            assert!(insight.contains("success"));
            assert!(insight.contains("Smooth execution"));
        }
    }

    #[tokio::test]
    async fn task_reflection_failure_produces_insight() {
        let proc = BasicEventProcessor::new();
        let event = LearningEvent::TaskReflection {
            task_summary: "fix bug".into(),
            tool_calls_count: 0,
            success: false,
            reflection: "Task failed due to missing permissions.".into(),
            timestamp: Utc::now(),
            trace_id: None,
        };
        let actions = proc.handle(&event).await;
        assert!(actions.iter().any(|a| {
            matches!(
                a,
                LearningAction::RecordInsight { insight, category }
                    if category == "task_reflection" && insight.contains("failure")
            )
        }));
    }

    #[tokio::test]
    async fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let stats_path = dir.path().join("stats.json");

        // Create processor, add events, save.
        let proc = BasicEventProcessor::new().with_stats_path(&stats_path);
        proc.handle(&tool_ok("shell", 100)).await;
        proc.handle(&tool_ok("shell", 200)).await;
        proc.handle(&tool_failed("web", ErrorClassification::Environment, 0))
            .await;
        proc.handle(&user_correction("style", "use tabs")).await;

        let expected = proc.stats();
        proc.save_stats().await.unwrap();

        // Create a new processor, load, verify counts match.
        let mut proc2 = BasicEventProcessor::new().with_stats_path(&stats_path);
        proc2.load_stats().await.unwrap();

        let loaded = proc2.stats();
        assert_eq!(loaded.events_processed, expected.events_processed);
        assert_eq!(loaded.tools.len(), expected.tools.len());

        let shell_loaded = loaded.tools.get("shell").unwrap();
        let shell_expected = expected.tools.get("shell").unwrap();
        assert_eq!(shell_loaded.success_count, shell_expected.success_count);
        assert_eq!(
            shell_loaded.total_duration_ms,
            shell_expected.total_duration_ms
        );

        let web_loaded = loaded.tools.get("web").unwrap();
        let web_expected = expected.tools.get("web").unwrap();
        assert_eq!(web_loaded.failure_count, web_expected.failure_count);

        assert_eq!(loaded.corrections.len(), expected.corrections.len());
        let corr_loaded = loaded.corrections.get("style").unwrap();
        let corr_expected = expected.corrections.get("style").unwrap();
        assert_eq!(corr_loaded.count, corr_expected.count);
        assert_eq!(corr_loaded.last_hint, corr_expected.last_hint);

        assert_eq!(loaded.skills.len(), expected.skills.len());
    }

    #[tokio::test]
    async fn save_stats_atomic_write() {
        let dir = tempfile::tempdir().unwrap();
        let stats_path = dir.path().join("stats.json");

        let proc = BasicEventProcessor::new().with_stats_path(&stats_path);
        proc.handle(&tool_ok("shell", 50)).await;
        proc.save_stats().await.unwrap();

        // File should exist and be valid JSON.
        let content = std::fs::read_to_string(&stats_path).unwrap();
        let parsed: ProcessorStats = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.events_processed, 1);
        assert_eq!(parsed.version, PROCESSOR_STATS_VERSION);

        // Temp file should not linger.
        assert!(!stats_path.with_extension("tmp").exists());
    }

    #[tokio::test]
    async fn load_stats_missing_file_graceful() {
        let dir = tempfile::tempdir().unwrap();
        let stats_path = dir.path().join("nonexistent.json");

        let mut proc = BasicEventProcessor::new().with_stats_path(&stats_path);
        // Loading from a nonexistent file should succeed with empty stats.
        proc.load_stats().await.unwrap();
        assert_eq!(proc.stats().events_processed, 0);
        assert!(proc.stats().tools.is_empty());
    }

    #[tokio::test]
    async fn load_stats_rejects_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let stats_path = dir.path().join("stats.json");
        std::fs::write(&stats_path, "{not valid json").unwrap();

        let mut proc = BasicEventProcessor::new().with_stats_path(&stats_path);
        assert!(proc.load_stats().await.is_err());
        assert_eq!(proc.stats().events_processed, 0);
    }

    #[tokio::test]
    async fn save_load_without_path_is_noop() {
        let mut proc = BasicEventProcessor::new();
        proc.handle(&tool_ok("shell", 10)).await;

        // save_stats and load_stats without a path should be no-ops.
        proc.save_stats().await.unwrap();
        proc.load_stats().await.unwrap();
        assert_eq!(proc.stats().events_processed, 1);
    }

    #[tokio::test]
    async fn load_stats_preserves_accumulation() {
        let dir = tempfile::tempdir().unwrap();
        let stats_path = dir.path().join("stats.json");

        // First session: process events and save.
        let proc = BasicEventProcessor::new().with_stats_path(&stats_path);
        for _ in 0..5 {
            proc.handle(&tool_failed("web", ErrorClassification::Environment, 0))
                .await;
        }
        proc.save_stats().await.unwrap();

        // Second session: load and continue processing.
        let mut proc2 = BasicEventProcessor::new().with_stats_path(&stats_path);
        proc2.load_stats().await.unwrap();
        assert_eq!(proc2.stats().events_processed, 5);

        // Process one more — failure_count should now be 6, triggering insight.
        let actions = proc2
            .handle(&tool_failed("web", ErrorClassification::Environment, 0))
            .await;
        assert!(actions
            .iter()
            .any(|a| matches!(a, LearningAction::RecordInsight { .. })));
        assert_eq!(proc2.stats().events_processed, 6);
    }

    #[tokio::test]
    async fn save_and_load_stats_preserves_version() {
        let dir = tempfile::tempdir().unwrap();
        let stats_path = dir.path().join("stats.json");

        let proc = BasicEventProcessor::new().with_stats_path(&stats_path);
        proc.save_stats().await.unwrap();

        let content = std::fs::read_to_string(&stats_path).unwrap();
        let saved: ProcessorStats = serde_json::from_str(&content).unwrap();
        assert_eq!(saved.version, PROCESSOR_STATS_VERSION);

        let mut loaded = BasicEventProcessor::new().with_stats_path(&stats_path);
        loaded.load_stats().await.unwrap();
        assert_eq!(loaded.stats().version, PROCESSOR_STATS_VERSION);
    }

    #[tokio::test]
    async fn record_action_outcome_tracks_success_and_failure() {
        let proc = BasicEventProcessor::new();
        proc.record_action_outcome("record_insight", true);
        proc.record_action_outcome("record_insight", true);
        proc.record_action_outcome("record_insight", false);

        let stats = proc.stats();
        let outcomes = stats
            .action_outcomes
            .get("record_insight")
            .expect("should exist");
        assert_eq!(outcomes.success_count, 2);
        assert_eq!(outcomes.failure_count, 1);
    }

    #[tokio::test]
    async fn action_outcomes_persist_across_save_load() {
        let dir = tempfile::tempdir().unwrap();
        let stats_path = dir.path().join("stats.json");

        let proc = BasicEventProcessor::new().with_stats_path(&stats_path);
        proc.record_action_outcome("adjust_confidence", true);
        proc.record_action_outcome("adjust_confidence", false);
        proc.save_stats().await.unwrap();

        let mut proc2 = BasicEventProcessor::new().with_stats_path(&stats_path);
        proc2.load_stats().await.unwrap();

        let stats2 = proc2.stats();
        let outcomes = stats2
            .action_outcomes
            .get("adjust_confidence")
            .expect("should exist");
        assert_eq!(outcomes.success_count, 1);
        assert_eq!(outcomes.failure_count, 1);
    }

    #[tokio::test]
    async fn event_trace_id_propagated_through_processor() {
        let proc = BasicEventProcessor::new();
        let event = LearningEvent::ToolSucceeded {
            tool: "shell".into(),
            args_summary: "ls".into(),
            duration_ms: 50,
            context_hash: "h".into(),
            timestamp: Utc::now(),
            trace_id: Some("trace-123".into()),
        };
        let actions = proc.handle(&event).await;
        // Should process normally; trace_id is on the event for correlation.
        assert_eq!(proc.stats().events_processed, 1);
        // ToolSucceeded produces no actions in basic processor.
        assert!(actions.is_empty());
    }
}
