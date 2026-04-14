//! Skill registry — register, query, and match skills by keyword.
//!
//! The registry holds compiled skills in an `Arc<RwLock<HashMap>>` and provides
//! keyword-based matching against user input.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::RwLock as AsyncRwLock;

use crate::error::{SkillError, SkillResult};
use crate::skill::{CompiledSkill, Skill};

/// A scored skill match returned by [`SkillRegistry::match_skills`].
#[derive(Debug, Clone)]
pub struct SkillMatch {
    /// Name of the matched skill.
    pub name: String,
    /// Match score (0.0 – 1.0).
    pub score: f64,
}

/// Central registry for loaded skills.
///
/// Thread-safe via `Arc<AsyncRwLock>`. Skills are stored by name and can be
/// queried by keyword matching against trigger lists.
#[derive(Debug, Clone)]
pub struct SkillRegistry {
    skills: Arc<AsyncRwLock<HashMap<String, Arc<RwLock<CompiledSkill>>>>>,
}

impl SkillRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            skills: Arc::new(AsyncRwLock::new(HashMap::new())),
        }
    }

    /// Register a compiled skill.
    ///
    /// Returns an error if a skill with the same name already exists.
    pub async fn register(&self, skill: CompiledSkill) -> SkillResult<()> {
        let name = skill.name().to_string();
        let mut skills = self.skills.write().await;
        if skills.contains_key(&name) {
            return Err(SkillError::AlreadyExists(name));
        }
        skills.insert(name, Arc::new(RwLock::new(skill)));
        Ok(())
    }

    /// Get a skill by name.
    pub async fn get(&self, name: &str) -> Option<Arc<RwLock<CompiledSkill>>> {
        let skills = self.skills.read().await;
        skills.get(name).cloned()
    }

    /// Remove a skill by name. Returns true if the skill was present.
    pub async fn unregister(&self, name: &str) -> bool {
        let mut skills = self.skills.write().await;
        skills.remove(name).is_some()
    }

    /// Return the number of registered skills.
    pub async fn len(&self) -> usize {
        self.skills.read().await.len()
    }

    /// Check if the registry is empty.
    pub async fn is_empty(&self) -> bool {
        self.skills.read().await.is_empty()
    }

    /// List all registered skill names.
    pub async fn skill_names(&self) -> Vec<String> {
        self.skills.read().await.keys().cloned().collect()
    }

    /// Match skills against a query string, sorted by descending score.
    ///
    /// Returns skills with a score > 0.0, sorted from best to worst match.
    pub async fn match_skills(&self, query: &str) -> Vec<SkillMatch> {
        let skills = self.skills.read().await;
        let mut matches: Vec<SkillMatch> = Vec::new();

        for (name, skill) in skills.iter() {
            let score = skill.read().matches(query);
            if score > 0.0 {
                matches.push(SkillMatch {
                    name: name.clone(),
                    score,
                });
            }
        }

        matches.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        matches
    }

    /// Update the confidence of a registered skill.
    pub async fn update_confidence(&self, name: &str, event: crate::skill::ConfidenceEvent) -> SkillResult<()> {
        let skills = self.skills.read().await;
        let skill = skills
            .get(name)
            .ok_or_else(|| SkillError::NotFound(name.to_string()))?;
        skill.write().update_confidence(event);
        Ok(())
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::SkillManifestBuilder;

    fn make_skill(name: &str, triggers: &[&str]) -> CompiledSkill {
        CompiledSkill::new(
            SkillManifestBuilder::new(name, "1.0.0", format!("Skill {name}"))
                .triggers(triggers.to_vec())
                .build(),
        )
    }

    #[tokio::test]
    async fn test_register_and_get() {
        let registry = SkillRegistry::new();
        let skill = make_skill("deploy", &["deploy", "release"]);
        registry.register(skill).await.unwrap();

        let got = registry.get("deploy").await;
        assert!(got.is_some());
        assert_eq!(got.unwrap().read().name(), "deploy");
    }

    #[tokio::test]
    async fn test_register_duplicate() {
        let registry = SkillRegistry::new();
        registry.register(make_skill("dup", &["x"])).await.unwrap();
        let result = registry.register(make_skill("dup", &["x"])).await;
        assert!(matches!(result.unwrap_err(), SkillError::AlreadyExists(_)));
    }

    #[tokio::test]
    async fn test_get_missing() {
        let registry = SkillRegistry::new();
        assert!(registry.get("nope").await.is_none());
    }

    #[tokio::test]
    async fn test_unregister() {
        let registry = SkillRegistry::new();
        registry.register(make_skill("rm", &["x"])).await.unwrap();
        assert!(registry.unregister("rm").await);
        assert!(!registry.unregister("rm").await);
        assert!(registry.get("rm").await.is_none());
    }

    #[tokio::test]
    async fn test_len_and_is_empty() {
        let registry = SkillRegistry::new();
        assert!(registry.is_empty().await);
        assert_eq!(registry.len().await, 0);

        registry.register(make_skill("a", &["a"])).await.unwrap();
        assert_eq!(registry.len().await, 1);
        assert!(!registry.is_empty().await);
    }

    #[tokio::test]
    async fn test_skill_names() {
        let registry = SkillRegistry::new();
        registry.register(make_skill("alpha", &["a"])).await.unwrap();
        registry.register(make_skill("beta", &["b"])).await.unwrap();
        let mut names = registry.skill_names().await;
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[tokio::test]
    async fn test_match_skills_single_hit() {
        let registry = SkillRegistry::new();
        registry
            .register(make_skill("deploy", &["deploy", "k8s"]))
            .await
            .unwrap();
        registry
            .register(make_skill("test", &["test", "unit"]))
            .await
            .unwrap();

        let matches = registry.match_skills("please deploy").await;
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "deploy");
        assert!(matches[0].score > 0.0);
    }

    #[tokio::test]
    async fn test_match_skills_multiple_hits() {
        let registry = SkillRegistry::new();
        registry
            .register(make_skill("deploy-k8s", &["deploy", "k8s"]))
            .await
            .unwrap();
        registry
            .register(make_skill("deploy-docker", &["deploy", "docker"]))
            .await
            .unwrap();

        let matches = registry.match_skills("deploy k8s").await;
        // Both match on "deploy", deploy-k8s also matches on "k8s"
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].name, "deploy-k8s");
    }

    #[tokio::test]
    async fn test_match_skills_no_hits() {
        let registry = SkillRegistry::new();
        registry.register(make_skill("deploy", &["deploy"])).await.unwrap();
        let matches = registry.match_skills("run tests").await;
        assert!(matches.is_empty());
    }

    #[tokio::test]
    async fn test_match_skills_sorted_by_score() {
        let registry = SkillRegistry::new();
        // high confidence skill
        let mut s1 = make_skill("skill-a", &["search"]);
        s1.update_confidence(crate::skill::ConfidenceEvent::UsedSuccessfully);
        s1.update_confidence(crate::skill::ConfidenceEvent::UsedSuccessfully);

        // low confidence skill
        let mut s2 = make_skill("skill-b", &["search"]);
        s2.update_confidence(crate::skill::ConfidenceEvent::UsedButFailed);

        registry.register(s1).await.unwrap();
        registry.register(s2).await.unwrap();

        let matches = registry.match_skills("search").await;
        assert_eq!(matches.len(), 2);
        assert!(matches[0].score > matches[1].score);
        assert_eq!(matches[0].name, "skill-a");
    }

    #[tokio::test]
    async fn test_update_confidence() {
        let registry = SkillRegistry::new();
        registry.register(make_skill("conf", &["x"])).await.unwrap();
        registry
            .update_confidence("conf", crate::skill::ConfidenceEvent::UserConfirmed)
            .await
            .unwrap();

        let skill = registry.get("conf").await.unwrap();
        assert!(skill.read().confidence() > 0.5);
    }

    #[tokio::test]
    async fn test_update_confidence_missing() {
        let registry = SkillRegistry::new();
        let result = registry
            .update_confidence("ghost", crate::skill::ConfidenceEvent::UsedSuccessfully)
            .await;
        assert!(matches!(result.unwrap_err(), SkillError::NotFound(_)));
    }
}
