//! Learning action consumer — dispatches processed actions to real backends.
//!
//! [`LearningConsumer`] takes the `Vec<LearningAction>` produced by event
//! processors and routes each action to the appropriate subsystem:
//!
//! - `RecordInsight` → `MemoryStore`
//! - `AdjustConfidence` → `SkillRegistry`
//! - `ProposeSkill` / `PatchSkill` / `DeprecateSkill` → logged (future sprint)
//! - `NoOp` → skipped

use std::sync::Arc;

use nanobot_memory::{MemoryCategory, MemoryEntry, MemoryStore};
use nanobot_skill::{ConfidenceEvent, SkillRegistry};
use tracing::info;

use crate::event::LearningAction;

/// Dispatches [`LearningAction`]s produced by learning event processors
/// to the concrete subsystems (memory store, skill registry).
///
/// Both backends are optional — if a backend is `None`, actions that
/// require it are logged and skipped rather than causing errors.
pub struct LearningConsumer {
    memory_store: Option<Arc<dyn MemoryStore>>,
    skill_registry: Option<Arc<SkillRegistry>>,
}

impl LearningConsumer {
    /// Creates a new consumer with the given backends.
    ///
    /// Either or both backends may be `None` if the subsystem is unavailable.
    pub fn new(
        memory_store: Option<Arc<dyn MemoryStore>>,
        skill_registry: Option<Arc<SkillRegistry>>,
    ) -> Self {
        Self {
            memory_store,
            skill_registry,
        }
    }

    /// Dispatches a batch of learning actions to their target backends.
    ///
    /// Each action is handled independently — a failure in one action does
    /// not prevent subsequent actions from being processed.
    pub async fn dispatch_actions(&self, actions: Vec<LearningAction>) {
        for action in actions {
            self.dispatch_one(action).await;
        }
    }

    /// Dispatches a single learning action.
    async fn dispatch_one(&self, action: LearningAction) {
        match action {
            LearningAction::RecordInsight { insight, category } => {
                self.handle_record_insight(insight, category).await;
            }
            LearningAction::AdjustConfidence { skill, delta } => {
                self.handle_adjust_confidence(skill, delta).await;
            }
            LearningAction::ProposeSkill { name, reason } => {
                info!(
                    "ProposeSkill: name={}, reason={} (not yet implemented)",
                    name, reason
                );
            }
            LearningAction::PatchSkill { skill, description } => {
                info!(
                    "PatchSkill: skill={}, description={} (not yet implemented)",
                    skill, description
                );
            }
            LearningAction::DeprecateSkill { skill, reason } => {
                info!(
                    "DeprecateSkill: skill={}, reason={} (not yet implemented)",
                    skill, reason
                );
            }
            LearningAction::NoOp => {
                // Explicitly skipped — no work to do.
            }
        }
    }

    /// Stores an insight as a memory entry in the memory store.
    async fn handle_record_insight(&self, insight: String, category: String) {
        let Some(ref store) = self.memory_store else {
            info!(
                "RecordInsight skipped (no memory store): [{}] {}",
                category, insight
            );
            return;
        };

        let entry = MemoryEntry::new(format!("[{category}] {insight}"), MemoryCategory::AgentNote)
            .with_confidence(0.8);

        if let Err(e) = store.store(entry).await {
            tracing::warn!("Failed to store insight: {}", e);
        }
    }

    /// Maps a confidence delta to a [`ConfidenceEvent`] and updates the skill registry.
    async fn handle_adjust_confidence(&self, skill: String, delta: f64) {
        let Some(ref registry) = self.skill_registry else {
            info!(
                "AdjustConfidence skipped (no skill registry): skill={}, delta={:.3}",
                skill, delta
            );
            return;
        };

        let event = if delta >= 0.0 {
            ConfidenceEvent::UsedSuccessfully
        } else {
            ConfidenceEvent::UsedButFailed
        };

        if let Err(e) = registry.update_confidence(&skill, event).await {
            tracing::warn!("Failed to update confidence for skill '{}': {}", skill, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanobot_memory::error::Result as MemoryResult;
    use nanobot_skill::{manifest::SkillManifestBuilder, CompiledSkill, Skill};
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Thread-safe mock that records `store()` calls.
    #[derive(Debug, Default)]
    struct MockMemoryStore {
        entries: Mutex<Vec<MemoryEntry>>,
        fail_store: AtomicBool,
    }

    #[async_trait::async_trait]
    impl MemoryStore for MockMemoryStore {
        async fn store(&self, entry: MemoryEntry) -> MemoryResult<()> {
            if self.fail_store.load(Ordering::Relaxed) {
                return Err(nanobot_memory::MemoryError::Config(
                    "injected store failure".into(),
                ));
            }

            self.entries.lock().push(entry);
            Ok(())
        }

        async fn recall(&self, id: &str) -> MemoryResult<Option<MemoryEntry>> {
            Ok(self
                .entries
                .lock()
                .iter()
                .find(|entry| entry.id == id)
                .cloned())
        }

        async fn search(
            &self,
            _query: &nanobot_memory::MemoryQuery,
        ) -> MemoryResult<Vec<nanobot_memory::ScoredEntry>> {
            Ok(vec![])
        }

        async fn delete(&self, _id: &str) -> MemoryResult<()> {
            Ok(())
        }

        async fn len(&self) -> usize {
            self.entries.lock().len()
        }

        async fn clear(&self) -> MemoryResult<()> {
            self.entries.lock().clear();
            Ok(())
        }
    }

    impl MockMemoryStore {
        fn set_fail_store(&self, fail_store: bool) {
            self.fail_store.store(fail_store, Ordering::Relaxed);
        }

        fn entries(&self) -> Vec<MemoryEntry> {
            self.entries.lock().clone()
        }
    }

    fn make_skill(name: &str, trigger: &str) -> CompiledSkill {
        CompiledSkill::new(
            SkillManifestBuilder::new(name, "1.0.0", "Test skill")
                .triggers([trigger])
                .build(),
        )
    }

    #[tokio::test]
    async fn dispatch_actions_records_insight_in_memory_store() {
        let mock_store = Arc::new(MockMemoryStore::default());
        let consumer =
            LearningConsumer::new(Some(mock_store.clone() as Arc<dyn MemoryStore>), None);

        consumer
            .dispatch_actions(vec![LearningAction::RecordInsight {
                insight: "Tool 'web' is unreliable".into(),
                category: "tool_reliability".into(),
            }])
            .await;

        assert_eq!(mock_store.len().await, 1);

        let entries = mock_store.entries();
        let entry = entries.first().expect("expected one stored entry");
        assert!(entry
            .content
            .contains("[tool_reliability] Tool 'web' is unreliable"));
        assert_eq!(entry.category, MemoryCategory::AgentNote);
        assert!((entry.confidence - 0.8).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn dispatch_actions_skips_record_insight_without_memory_store() {
        let consumer = LearningConsumer::new(None, None);

        consumer
            .dispatch_actions(vec![LearningAction::RecordInsight {
                insight: "something".into(),
                category: "test".into(),
            }])
            .await;
    }

    #[tokio::test]
    async fn dispatch_actions_continue_after_memory_store_failure() {
        let mock_store = Arc::new(MockMemoryStore::default());
        mock_store.set_fail_store(true);

        let registry = Arc::new(SkillRegistry::new());
        registry
            .register(make_skill("recover-skill", "recover"))
            .await
            .expect("register skill");

        let consumer = LearningConsumer::new(
            Some(mock_store as Arc<dyn MemoryStore>),
            Some(registry.clone()),
        );

        consumer
            .dispatch_actions(vec![
                LearningAction::RecordInsight {
                    insight: "will fail".into(),
                    category: "test".into(),
                },
                LearningAction::AdjustConfidence {
                    skill: "recover-skill".into(),
                    delta: 0.2,
                },
            ])
            .await;

        let skill = registry
            .get("recover-skill")
            .await
            .expect("skill should remain registered");
        assert!(skill.read().confidence() > 0.5);
    }

    #[tokio::test]
    async fn dispatch_actions_raise_skill_confidence_for_positive_delta() {
        let registry = Arc::new(SkillRegistry::new());
        registry
            .register(make_skill("my-skill", "test"))
            .await
            .expect("register skill");

        let initial_confidence = {
            let s = registry.get("my-skill").await.expect("skill should exist");
            let confidence = s.read().confidence();
            confidence
        };

        let consumer = LearningConsumer::new(None, Some(registry.clone()));
        consumer
            .dispatch_actions(vec![LearningAction::AdjustConfidence {
                skill: "my-skill".into(),
                delta: 0.05,
            }])
            .await;

        let new_confidence = {
            let s = registry.get("my-skill").await.expect("skill should exist");
            let confidence = s.read().confidence();
            confidence
        };
        assert!(
            new_confidence > initial_confidence,
            "positive delta should increase confidence"
        );
    }

    #[tokio::test]
    async fn dispatch_actions_lower_skill_confidence_for_negative_delta() {
        let registry = Arc::new(SkillRegistry::new());
        registry
            .register(make_skill("neg-skill", "test"))
            .await
            .expect("register skill");
        let initial_confidence = {
            let s = registry.get("neg-skill").await.expect("skill should exist");
            let confidence = s.read().confidence();
            confidence
        };

        let consumer = LearningConsumer::new(None, Some(registry.clone()));
        consumer
            .dispatch_actions(vec![LearningAction::AdjustConfidence {
                skill: "neg-skill".into(),
                delta: -0.2,
            }])
            .await;

        let new_confidence = {
            let s = registry.get("neg-skill").await.expect("skill should exist");
            let confidence = s.read().confidence();
            confidence
        };
        assert!(
            new_confidence < initial_confidence,
            "negative delta should decrease confidence"
        );
    }

    #[tokio::test]
    async fn dispatch_actions_skip_adjust_confidence_without_registry() {
        let consumer = LearningConsumer::new(None, None);

        consumer
            .dispatch_actions(vec![LearningAction::AdjustConfidence {
                skill: "missing".into(),
                delta: 0.1,
            }])
            .await;
    }

    #[tokio::test]
    async fn dispatch_actions_ignore_missing_skill_for_confidence_update() {
        let registry = Arc::new(SkillRegistry::new());
        let consumer = LearningConsumer::new(None, Some(registry.clone()));

        consumer
            .dispatch_actions(vec![LearningAction::AdjustConfidence {
                skill: "missing".into(),
                delta: 0.4,
            }])
            .await;

        assert!(registry.get("missing").await.is_none());
    }

    #[tokio::test]
    async fn dispatch_actions_treat_noop_as_noop() {
        let mock_store = Arc::new(MockMemoryStore::default());
        let registry = Arc::new(SkillRegistry::new());
        registry
            .register(make_skill("noop-skill", "noop"))
            .await
            .expect("register skill");

        let initial_confidence = registry
            .get("noop-skill")
            .await
            .expect("skill should exist")
            .read()
            .confidence();

        let consumer = LearningConsumer::new(
            Some(mock_store.clone() as Arc<dyn MemoryStore>),
            Some(registry.clone()),
        );

        consumer.dispatch_actions(vec![LearningAction::NoOp]).await;

        assert!(mock_store.is_empty().await);
        let final_confidence = registry
            .get("noop-skill")
            .await
            .expect("skill should exist")
            .read()
            .confidence();
        assert_eq!(initial_confidence, final_confidence);
    }

    #[tokio::test]
    async fn dispatch_actions_accept_propose_skill_variant() {
        let consumer = LearningConsumer::new(None, None);
        consumer
            .dispatch_actions(vec![LearningAction::ProposeSkill {
                name: "new-skill".into(),
                reason: "repeated pattern".into(),
            }])
            .await;
    }

    #[tokio::test]
    async fn dispatch_actions_accept_patch_skill_variant() {
        let consumer = LearningConsumer::new(None, None);
        consumer
            .dispatch_actions(vec![LearningAction::PatchSkill {
                skill: "existing-skill".into(),
                description: "adjust trigger coverage".into(),
            }])
            .await;
    }

    #[tokio::test]
    async fn dispatch_actions_accept_deprecate_skill_variant() {
        let consumer = LearningConsumer::new(None, None);
        consumer
            .dispatch_actions(vec![LearningAction::DeprecateSkill {
                skill: "old-skill".into(),
                reason: "superseded".into(),
            }])
            .await;
    }

    #[tokio::test]
    async fn dispatch_actions_process_mixed_batches() {
        let mock_store = Arc::new(MockMemoryStore::default());
        let registry = Arc::new(SkillRegistry::new());
        registry
            .register(make_skill("batch-skill", "batch"))
            .await
            .expect("register skill");

        let consumer = LearningConsumer::new(
            Some(mock_store.clone() as Arc<dyn MemoryStore>),
            Some(registry.clone()),
        );

        consumer
            .dispatch_actions(vec![
                LearningAction::NoOp,
                LearningAction::RecordInsight {
                    insight: "first insight".into(),
                    category: "test".into(),
                },
                LearningAction::AdjustConfidence {
                    skill: "batch-skill".into(),
                    delta: 0.1,
                },
                LearningAction::ProposeSkill {
                    name: "future-skill".into(),
                    reason: "not yet".into(),
                },
            ])
            .await;

        assert_eq!(mock_store.len().await, 1);

        let s = registry
            .get("batch-skill")
            .await
            .expect("skill should exist");
        assert!(s.read().confidence() > 0.5);
    }

    #[tokio::test]
    async fn dispatch_actions_handle_empty_batches() {
        let consumer = LearningConsumer::new(None, None);
        consumer.dispatch_actions(vec![]).await;
    }
}
