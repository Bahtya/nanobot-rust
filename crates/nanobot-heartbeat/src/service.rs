//! Heartbeat service — two-phase task checking.
//!
//! Phase 1: LLM decides if there are pending tasks via a lightweight prompt.
//! Phase 2: Full agent execution if tasks are found.
//! Mirrors the Python `heartbeat/service.py`.

use anyhow::Result;
use nanobot_agent::AgentRunner;
use nanobot_config::Config;
use nanobot_core::Message;
use nanobot_core::MessageRole;
use nanobot_providers::ProviderRegistry;
use nanobot_session::SessionManager;
use nanobot_tools::ToolRegistry;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// The heartbeat service.
pub struct HeartbeatService {
    config: Arc<Config>,
    provider_registry: Arc<ProviderRegistry>,
    tool_registry: Arc<ToolRegistry>,
    session_manager: Option<Arc<SessionManager>>,
    interval: Duration,
    running: Arc<RwLock<bool>>,
}

impl HeartbeatService {
    /// Create a new heartbeat service.
    pub fn new(config: Config) -> Self {
        let interval_secs = config.heartbeat.interval_secs;
        Self {
            config: Arc::new(config),
            provider_registry: Arc::new(ProviderRegistry::new()),
            tool_registry: Arc::new(ToolRegistry::new()),
            session_manager: None,
            interval: Duration::from_secs(interval_secs.max(60)),
            running: Arc::new(RwLock::new(false)),
        }
    }

    /// Create with provider and tool registries for actual LLM access.
    pub fn with_registries(
        config: Config,
        provider_registry: ProviderRegistry,
        tool_registry: ToolRegistry,
        session_manager: SessionManager,
    ) -> Self {
        let interval_secs = config.heartbeat.interval_secs;
        Self {
            config: Arc::new(config),
            provider_registry: Arc::new(provider_registry),
            tool_registry: Arc::new(tool_registry),
            session_manager: Some(Arc::new(session_manager)),
            interval: Duration::from_secs(interval_secs.max(60)),
            running: Arc::new(RwLock::new(false)),
        }
    }

    /// Start the heartbeat loop.
    pub async fn run(&self) -> Result<()> {
        let mut running = self.running.write().await;
        if *running {
            return Ok(());
        }
        *running = true;
        drop(running);

        info!(
            "Heartbeat service started (interval: {}s)",
            self.interval.as_secs()
        );

        loop {
            if !*self.running.read().await {
                break;
            }

            tokio::time::sleep(self.interval).await;

            if !*self.running.read().await {
                break;
            }

            match self.check_and_execute().await {
                Ok(acted) => {
                    if acted {
                        info!("Heartbeat completed task execution");
                    } else {
                        debug!("Heartbeat: no pending tasks");
                    }
                }
                Err(e) => {
                    warn!("Heartbeat check failed: {}", e);
                }
            }
        }

        *self.running.write().await = false;
        info!("Heartbeat service stopped");
        Ok(())
    }

    /// Check for pending tasks and execute if found.
    async fn check_and_execute(&self) -> Result<bool> {
        // Phase 1: Ask the LLM if there are pending tasks
        let has_tasks = self.check_tasks().await?;

        if !has_tasks {
            return Ok(false);
        }

        info!("Heartbeat found pending tasks — executing Phase 2");
        // Phase 2: Run the full agent to handle the tasks
        self.execute_tasks().await
    }

    /// Phase 1: Check for pending tasks using LLM.
    async fn check_tasks(&self) -> Result<bool> {
        let model = &self.config.agent.model;

        let provider = match self.provider_registry.get_provider(model) {
            Some(p) => p,
            None => {
                debug!("No provider configured for heartbeat model '{}'", model);
                return Ok(false);
            }
        };

        let check_prompt = format!(
            "You are a task scheduler. Check if there are any pending tasks or follow-ups \
             that need attention right now. \
             Model: {}\n\n\
             Respond with ONLY 'YES' or 'NO'.",
            model,
        );

        let request = nanobot_providers::CompletionRequest {
            model: model.clone(),
            messages: vec![Message {
                role: MessageRole::User,
                content: check_prompt,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            tools: None,
            max_tokens: Some(10),
            temperature: Some(0.0),
            stream: false,
        };

        match provider.complete(request).await {
            Ok(response) => {
                let content = response.content.unwrap_or_default().to_uppercase();
                let has = content.contains("YES");
                debug!(
                    "Heartbeat check response: '{}' → has_tasks={}",
                    content, has
                );
                Ok(has)
            }
            Err(e) => {
                error!("Heartbeat LLM call failed: {}", e);
                Ok(false)
            }
        }
    }

    /// Phase 2: Execute pending tasks using the full agent.
    async fn execute_tasks(&self) -> Result<bool> {
        let session_manager = match &self.session_manager {
            Some(sm) => sm.clone(),
            None => {
                warn!("Heartbeat: no session manager configured for task execution");
                return Ok(false);
            }
        };

        // Get or create a heartbeat session
        let session = session_manager.get_or_create("heartbeat:system", None);

        let system_prompt = self
            .config
            .agent
            .system_prompt
            .clone()
            .unwrap_or_else(|| "You are a task management assistant.".to_string());

        let messages = session.to_messages();
        let runner = AgentRunner::new(
            self.config.clone(),
            self.provider_registry.clone(),
            self.tool_registry.clone(),
        );

        match runner.run(system_prompt, messages).await {
            Ok(result) => {
                // Save the result back to the session
                let mut session = session;
                session.add_assistant_message(result.content.clone());
                session_manager.save_session(&session)?;
                info!(
                    "Heartbeat executed tasks ({} iterations)",
                    result.iterations_used
                );
                Ok(true)
            }
            Err(e) => {
                error!("Heartbeat task execution failed: {}", e);
                Ok(false)
            }
        }
    }

    /// Stop the heartbeat service.
    pub async fn stop(&self) {
        *self.running.write().await = false;
    }

    /// Check if the service is running.
    pub async fn is_running(&self) -> bool {
        *self.running.read().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanobot_config::Config;
    use std::time::Duration;

    #[test]
    fn test_heartbeat_service_construction() {
        let config = Config::default();
        let svc = HeartbeatService::new(config);
        assert!(svc.interval >= Duration::from_secs(60));
    }

    #[tokio::test]
    async fn test_heartbeat_service_not_running_initially() {
        let config = Config::default();
        let svc = HeartbeatService::new(config);
        assert!(!svc.is_running().await);
    }

    #[tokio::test]
    async fn test_heartbeat_service_stop_when_not_running() {
        let config = Config::default();
        let svc = HeartbeatService::new(config);
        svc.stop().await;
        assert!(!svc.is_running().await);
    }

    #[test]
    fn test_heartbeat_with_registries() {
        let config = Config::default();
        let tmp = tempfile::tempdir().unwrap();
        let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();
        let providers = ProviderRegistry::new();
        let tools = ToolRegistry::new();
        let svc = HeartbeatService::with_registries(config, providers, tools, session_manager);
        assert!(svc.session_manager.is_some());
    }

    #[test]
    fn test_heartbeat_interval_minimum_60s() {
        let mut config = Config::default();
        config.heartbeat.interval_secs = 10; // Below minimum
        let svc = HeartbeatService::new(config);
        assert!(svc.interval >= Duration::from_secs(60));
    }

    #[test]
    fn test_heartbeat_interval_uses_config() {
        let mut config = Config::default();
        config.heartbeat.interval_secs = 300;
        let svc = HeartbeatService::new(config);
        assert_eq!(svc.interval, Duration::from_secs(300));
    }

    #[test]
    fn test_heartbeat_default_disabled() {
        let config = Config::default();
        assert!(!config.heartbeat.enabled);
    }

    #[tokio::test]
    async fn test_heartbeat_stop_idempotent() {
        let config = Config::default();
        let svc = HeartbeatService::new(config);
        svc.stop().await;
        svc.stop().await;
        assert!(!svc.is_running().await);
    }
}
