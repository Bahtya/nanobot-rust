//! Core Skill trait and related types.
//!
//! The [`Skill`] trait is the primary abstraction. [`CompiledSkill`] is the default
//! implementation backed by a TOML [`SkillManifest`](crate::SkillManifest).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::SkillResult;

/// Output produced by skill execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SkillOutput {
    /// Inject a prompt segment into the agent context.
    PromptOnly {
        /// The prompt text to inject.
        segment: String,
    },
    /// Skill executed with a concrete result.
    Executed {
        /// Result text.
        result: String,
        /// Side effects produced (e.g. "file written to /tmp/x").
        side_effects: Vec<String>,
    },
}

/// Events that adjust a skill's confidence score.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum ConfidenceEvent {
    /// Skill was used and helped produce a correct result.
    UsedSuccessfully,
    /// Skill was invoked but the result was wrong.
    UsedButFailed,
    /// User explicitly confirmed the output.
    UserConfirmed,
    /// User corrected the output, indicating the skill was imprecise.
    UserCorrected,
    /// Natural confidence decay over time (hours since last use).
    TimeDecay {
        /// Hours elapsed since last use.
        hours: f64,
    },
}

/// Asynchronous trait for a nanobot skill.
#[async_trait]
pub trait Skill: Send + Sync + 'static {
    /// Unique skill name (kebab-case).
    fn name(&self) -> &str;

    /// Human-readable description.
    fn description(&self) -> &str;

    /// Skill category (defaults to "uncategorized").
    fn category(&self) -> &str {
        "uncategorized"
    }

    /// Current confidence score (0.0 – 1.0).
    fn confidence(&self) -> f64;

    /// Return a match score (0.0 – 1.0) for the given user input.
    ///
    /// Phase 1: keyword-based matching. Future phases: regex + semantic.
    fn matches(&self, query: &str) -> f64;

    /// Execute the skill with optional JSON arguments.
    async fn execute(&self, args: serde_json::Value) -> SkillResult<SkillOutput>;

    /// Update confidence based on a feedback event.
    fn update_confidence(&mut self, event: ConfidenceEvent);
}

/// A skill compiled from a TOML manifest with keyword-based matching.
#[derive(Debug, Clone)]
pub struct CompiledSkill {
    manifest: crate::SkillManifest,
    confidence: f64,
    usage_count: u64,
}

impl CompiledSkill {
    /// Create a new compiled skill from a validated manifest.
    pub fn new(manifest: crate::SkillManifest) -> Self {
        Self {
            manifest,
            confidence: 0.5,
            usage_count: 0,
        }
    }

    /// Reference to the underlying manifest.
    pub fn manifest(&self) -> &crate::SkillManifest {
        &self.manifest
    }

    /// How many times this skill has been used.
    pub fn usage_count(&self) -> u64 {
        self.usage_count
    }

    /// Build the prompt segment from manifest steps and pitfalls.
    fn build_prompt(&self) -> String {
        let mut parts = Vec::new();

        if !self.manifest.steps.is_empty() {
            parts.push(format!("## Steps for {}", self.manifest.name));
            for (i, step) in self.manifest.steps.iter().enumerate() {
                parts.push(format!("{}. {step}", i + 1));
            }
        }

        if !self.manifest.pitfalls.is_empty() {
            parts.push("## Pitfalls".to_string());
            for pit in &self.manifest.pitfalls {
                parts.push(format!("- {pit}"));
            }
        }

        parts.join("\n")
    }
}

#[async_trait]
impl Skill for CompiledSkill {
    fn name(&self) -> &str {
        &self.manifest.name
    }

    fn description(&self) -> &str {
        &self.manifest.description
    }

    fn category(&self) -> &str {
        &self.manifest.category
    }

    fn confidence(&self) -> f64 {
        self.confidence
    }

    fn matches(&self, query: &str) -> f64 {
        let query_lower = query.to_lowercase();
        let mut hits = 0usize;
        let total = self.manifest.triggers.len();

        for keyword in &self.manifest.triggers {
            if query_lower.contains(&keyword.to_lowercase()) {
                hits += 1;
            }
        }

        if total == 0 {
            return 0.0;
        }

        let base = hits as f64 / total as f64;
        // Weight by confidence: high-confidence skills rank above low-confidence ones
        base * self.confidence
    }

    async fn execute(&self, _args: serde_json::Value) -> SkillResult<SkillOutput> {
        Ok(SkillOutput::PromptOnly {
            segment: self.build_prompt(),
        })
    }

    fn update_confidence(&mut self, event: ConfidenceEvent) {
        match event {
            ConfidenceEvent::UsedSuccessfully => {
                self.confidence = (self.confidence + 0.1).min(1.0);
                self.usage_count += 1;
            }
            ConfidenceEvent::UsedButFailed => {
                self.confidence = (self.confidence - 0.15).max(0.0);
                self.usage_count += 1;
            }
            ConfidenceEvent::UserConfirmed => {
                self.confidence = (self.confidence + 0.2).min(1.0);
            }
            ConfidenceEvent::UserCorrected => {
                self.confidence = (self.confidence - 0.1).max(0.0);
            }
            ConfidenceEvent::TimeDecay { hours } => {
                // Exponential decay: lose 1% per hour
                let decay = (-0.01 * hours).exp();
                self.confidence *= decay;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::SkillManifestBuilder;

    fn test_manifest() -> crate::SkillManifest {
        SkillManifestBuilder::new("test-skill", "1.0.0", "A test skill")
            .triggers(["test", "example"])
            .steps(["Do thing A", "Do thing B"])
            .pitfalls(["Watch out for X"])
            .category("testing")
            .build()
    }

    fn test_skill() -> CompiledSkill {
        CompiledSkill::new(test_manifest())
    }

    #[test]
    fn test_name_description_category() {
        let skill = test_skill();
        assert_eq!(skill.name(), "test-skill");
        assert_eq!(skill.description(), "A test skill");
        assert_eq!(skill.category(), "testing");
    }

    #[test]
    fn test_matches_hit() {
        let skill = test_skill();
        let score = skill.matches("please test this code");
        assert!(score > 0.0, "should match on 'test' keyword");
    }

    #[test]
    fn test_matches_case_insensitive() {
        let skill = test_skill();
        let score = skill.matches("TEST THIS");
        assert!(score > 0.0);
    }

    #[test]
    fn test_matches_miss() {
        let skill = test_skill();
        let score = skill.matches("deploy to production");
        assert_eq!(score, 0.0, "no trigger keywords present");
    }

    #[test]
    fn test_matches_partial_hit() {
        let skill = test_skill();
        // Only "test" matches, not "example"
        let score = skill.matches("test");
        let score_both = skill.matches("test example");
        assert!(score_both > score);
    }

    #[tokio::test]
    async fn test_execute_returns_prompt() {
        let skill = test_skill();
        let output = skill.execute(serde_json::Value::Null).await.unwrap();
        match output {
            SkillOutput::PromptOnly { segment } => {
                assert!(segment.contains("Steps for test-skill"));
                assert!(segment.contains("1. Do thing A"));
                assert!(segment.contains("Watch out for X"));
            }
            _ => panic!("expected PromptOnly output"),
        }
    }

    #[test]
    fn test_confidence_update_success() {
        let mut skill = test_skill();
        let initial = skill.confidence();
        skill.update_confidence(ConfidenceEvent::UsedSuccessfully);
        assert!(skill.confidence() > initial);
        assert_eq!(skill.usage_count(), 1);
    }

    #[test]
    fn test_confidence_update_failed() {
        let mut skill = test_skill();
        let initial = skill.confidence();
        skill.update_confidence(ConfidenceEvent::UsedButFailed);
        assert!(skill.confidence() < initial);
    }

    #[test]
    fn test_confidence_update_confirmed() {
        let mut skill = test_skill();
        let initial = skill.confidence();
        skill.update_confidence(ConfidenceEvent::UserConfirmed);
        assert!(skill.confidence() > initial);
    }

    #[test]
    fn test_confidence_update_corrected() {
        let mut skill = test_skill();
        let initial = skill.confidence();
        skill.update_confidence(ConfidenceEvent::UserCorrected);
        assert!(skill.confidence() < initial);
    }

    #[test]
    fn test_confidence_time_decay() {
        let mut skill = test_skill();
        skill.confidence = 0.8;
        skill.update_confidence(ConfidenceEvent::TimeDecay { hours: 10.0 });
        assert!(skill.confidence() < 0.8);
    }

    #[test]
    fn test_confidence_clamps_high() {
        let mut skill = test_skill();
        skill.confidence = 0.95;
        skill.update_confidence(ConfidenceEvent::UsedSuccessfully);
        skill.update_confidence(ConfidenceEvent::UserConfirmed);
        assert!(skill.confidence() <= 1.0);
    }

    #[test]
    fn test_confidence_clamps_low() {
        let mut skill = test_skill();
        skill.confidence = 0.05;
        skill.update_confidence(ConfidenceEvent::UsedButFailed);
        skill.update_confidence(ConfidenceEvent::UsedButFailed);
        assert!(skill.confidence() >= 0.0);
    }

    #[test]
    fn test_build_prompt_empty_manifest() {
        let manifest = SkillManifestBuilder::new("empty", "0.1.0", "no steps")
            .triggers(["x"])
            .build();
        let skill = CompiledSkill::new(manifest);
        let prompt = skill.build_prompt();
        assert!(prompt.is_empty());
    }
}
