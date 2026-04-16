//! Gateway command — start the full kestrel gateway.
//!
//! Wires together: bus, channels, agent loop, session manager,
//! provider registry, tool registry, skill registry, heartbeat, and API server.
//!
//! Supports daemon mode when launched via `kestrel daemon start`.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use kestrel_agent::AgentLoop;
use kestrel_api::ApiServer;
use kestrel_bus::events::AgentEvent;
use kestrel_bus::MessageBus;
use kestrel_channels::{ChannelManager, ChannelRegistry};
use kestrel_config::Config;
use kestrel_heartbeat::{
    BusHealthCheck, ChannelHealthCheck, HeartbeatService, MemoryStoreHealthCheck,
    ProviderHealthCheck,
};
use kestrel_learning::config::LearningConfig;
use kestrel_learning::event::{LearningAction, LearningEvent, LearningEventBus};
use kestrel_learning::processor::BasicEventProcessor;
use kestrel_learning::prompt::PromptAssembler;
use kestrel_learning::store::EventStore;
use kestrel_learning::LearningEventHandler;
use kestrel_memory::{HotStore, MemoryCategory, MemoryConfig, MemoryEntry, MemoryStore};
use kestrel_providers::ProviderRegistry;
use kestrel_session::SessionManager;
use kestrel_skill::{SkillConfig, SkillLoader, SkillRegistry};
use kestrel_tools::builtins;
use tokio::sync::{broadcast, watch};
use tracing::info;

/// Initialize the skill registry by loading TOML manifests from the skills directory.
///
/// Looks for `skills/` under the given kestrel home directory. If the directory
/// does not exist, returns an empty registry. Invalid manifests are logged and skipped.
async fn init_skill_registry(home: &Path) -> Arc<SkillRegistry> {
    let skills_dir = home.join("skills");
    let registry = Arc::new(SkillRegistry::new());

    if !skills_dir.exists() {
        info!(
            "Skills directory not found at {}, skipping skill loading",
            skills_dir.display()
        );
        return registry;
    }

    let config = SkillConfig::default().with_skills_dir(&skills_dir);
    let loader = SkillLoader::new(config);

    match loader.load_all(&registry).await {
        Ok(loaded) => {
            if loaded.is_empty() {
                info!("No skill manifests found in {}", skills_dir.display());
            } else {
                info!("Loaded {} skills: {:?}", loaded.len(), loaded);
            }
        }
        Err(e) => {
            tracing::warn!("Failed to load skills from {}: {}", skills_dir.display(), e);
        }
    }

    registry
}

/// Register the gateway heartbeat checks and publish an initial snapshot.
async fn prime_gateway_heartbeat(
    heartbeat: &mut HeartbeatService,
    api_server: &ApiServer,
    provider_registry: ProviderRegistry,
    bus: MessageBus,
    memory_store: Option<Arc<dyn MemoryStore>>,
    channel_manager: Arc<ChannelManager>,
) -> Result<Arc<parking_lot::RwLock<Vec<(String, bool)>>>> {
    let channel_statuses = Arc::new(parking_lot::RwLock::new(
        channel_manager.channel_statuses().await,
    ));

    heartbeat.set_bus(bus.clone());
    heartbeat.register_check(Arc::new(ProviderHealthCheck::new(Arc::new(
        provider_registry,
    ))));
    heartbeat.register_check(Arc::new(BusHealthCheck::new(bus)));
    if let Some(store) = memory_store {
        heartbeat.register_check(Arc::new(MemoryStoreHealthCheck::new(store)));
    }
    heartbeat.register_check(Arc::new(ChannelHealthCheck::new(channel_statuses.clone())));
    heartbeat.add_snapshot_sink(api_server.health_snapshot_lock());
    heartbeat.run_checks().await?;

    Ok(channel_statuses)
}

/// Keep the shared channel health snapshot in sync with the channel manager.
async fn refresh_channel_health(
    channel_manager: Arc<ChannelManager>,
    statuses: Arc<parking_lot::RwLock<Vec<(String, bool)>>>,
) {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
    loop {
        interval.tick().await;
        *statuses.write() = channel_manager.channel_statuses().await;
    }
}

/// Async interface used by the learning consumer task.
#[async_trait]
trait GatewayLearningProcessor: Send {
    /// Process a learning event into concrete actions.
    async fn process_event(&mut self, event: &LearningEvent) -> Vec<LearningAction>;

    /// Persist processor stats.
    async fn save_stats(&self) -> Result<()>;
}

#[async_trait]
impl GatewayLearningProcessor for BasicEventProcessor {
    async fn process_event(&mut self, event: &LearningEvent) -> Vec<LearningAction> {
        self.handle(event).await
    }

    async fn save_stats(&self) -> Result<()> {
        BasicEventProcessor::save_stats(self)
            .await
            .map_err(Into::into)
    }
}

/// Execute a single learning action against the shared runtime state.
async fn execute_learning_action(
    action: &LearningAction,
    memory_store: Option<&Arc<dyn MemoryStore>>,
    skill_registry: &SkillRegistry,
    prompt_adjustment_tx: &watch::Sender<Option<String>>,
) -> Result<()> {
    match action {
        LearningAction::NoOp => Ok(()),
        LearningAction::AdjustConfidence { skill, delta } => skill_registry
            .adjust_confidence(skill, *delta)
            .await
            .with_context(|| format!("failed to adjust confidence for skill '{skill}'")),
        LearningAction::ProposeSkill { name, reason } => {
            let instructions = if reason.is_empty() {
                format!("Auto-generated skill: {name}")
            } else {
                reason.clone()
            };
            skill_registry
                .create_skill(name, reason, &instructions)
                .await
                .with_context(|| format!("failed to create skill '{name}'"))
        }
        LearningAction::PatchSkill { skill, description } => skill_registry
            .update_skill_instructions(skill, description)
            .await
            .with_context(|| format!("failed to patch skill '{skill}'")),
        LearningAction::DeprecateSkill { skill, reason } => skill_registry
            .deprecate_skill(skill, reason)
            .await
            .with_context(|| format!("failed to deprecate skill '{skill}'")),
        LearningAction::RecordInsight { insight, category } => {
            if category == "prompt_adjustment" {
                prompt_adjustment_tx.send_replace(Some(insight.clone()));
                return Ok(());
            }

            let store = memory_store.context("memory store not configured")?;
            let entry = build_memory_entry(insight, category);
            store
                .store(entry)
                .await
                .with_context(|| format!("failed to store insight in category '{category}'"))
        }
    }
}

/// Execute all learning actions, logging individual failures and continuing.
async fn execute_learning_actions(
    actions: &[LearningAction],
    memory_store: Option<&Arc<dyn MemoryStore>>,
    skill_registry: &SkillRegistry,
    prompt_adjustment_tx: &watch::Sender<Option<String>>,
) {
    for action in actions {
        if let Err(e) =
            execute_learning_action(action, memory_store, skill_registry, prompt_adjustment_tx)
                .await
        {
            tracing::error!("Failed to execute learning action {:?}: {}", action, e);
        }
    }
}

/// Convert an insight action into a memory entry for persistence.
fn build_memory_entry(insight: &str, category: &str) -> MemoryEntry {
    MemoryEntry::new(insight, map_memory_category(category)).with_confidence(0.8)
}

/// Map a learning insight category to the closest memory category.
fn map_memory_category(category: &str) -> MemoryCategory {
    match category {
        "user_profile" => MemoryCategory::UserProfile,
        "preference" => MemoryCategory::Preference,
        "environment" => MemoryCategory::Environment,
        "project_convention" => MemoryCategory::ProjectConvention,
        "tool_reliability" => MemoryCategory::ToolDiscovery,
        "error_lesson" => MemoryCategory::ErrorLesson,
        "workflow_pattern" => MemoryCategory::WorkflowPattern,
        "critical" => MemoryCategory::Critical,
        _ => MemoryCategory::AgentNote,
    }
}

/// Run the gateway learning consumer until shutdown is requested.
async fn run_learning_consumer<P>(
    learning_rx: &mut broadcast::Receiver<LearningEvent>,
    shutdown_rx: &mut watch::Receiver<bool>,
    event_store: EventStore,
    processor: &mut P,
    memory_store: Option<Arc<dyn MemoryStore>>,
    skill_registry: Arc<SkillRegistry>,
    prompt_adjustment_tx: watch::Sender<Option<String>>,
) where
    P: GatewayLearningProcessor,
{
    let mut events_since_save: u64 = 0;

    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                match changed {
                    Ok(()) if *shutdown_rx.borrow() => break,
                    Ok(()) => {}
                    Err(_) => break,
                }
            }
            received = learning_rx.recv() => {
                match received {
                    Ok(event) => {
                        if let Err(e) = event_store.append(&event).await {
                            tracing::warn!("Failed to persist learning event: {}", e);
                        }

                        let actions = processor.process_event(&event).await;
                        execute_learning_actions(
                            &actions,
                            memory_store.as_ref(),
                            skill_registry.as_ref(),
                            &prompt_adjustment_tx,
                        )
                        .await;

                        events_since_save += 1;
                        if events_since_save >= 50 {
                            if let Err(e) = processor.save_stats().await {
                                tracing::warn!("Failed to save processor stats: {}", e);
                            }
                            events_since_save = 0;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("Learning event consumer lagged by {n} messages");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    if let Err(e) = processor.save_stats().await {
        tracing::warn!("Failed to save processor stats during shutdown: {}", e);
    }
}

/// Run the gateway — starts all components and connects them via the bus.
///
/// PID file management is handled by the `daemon start` command, not here.
pub async fn run(config: Config, channels: Vec<String>, dangerous: bool) -> Result<()> {
    info!("Starting kestrel gateway...");

    // ── Shared bus ────────────────────────────────────────────
    let bus = MessageBus::new();

    // ── Session manager ───────────────────────────────────────
    let home = kestrel_config::paths::get_kestrel_home()?;
    let session_manager = SessionManager::new(home.clone())?;

    // ── Provider registry ─────────────────────────────────────
    let provider_registry = ProviderRegistry::from_config(&config)?;
    info!("Providers: {:?}", provider_registry.provider_names());

    // ── Tool registry ─────────────────────────────────────────
    let tool_registry = kestrel_tools::ToolRegistry::new();
    builtins::register_all_with_config(&tool_registry, builtins::BuiltinsConfig { dangerous });
    info!("Tools: {:?}", tool_registry.tool_names());

    // ── Seed channel tokens from config into env vars ─────────
    if let Some(ref tg) = config.channels.telegram {
        if tg.enabled {
            std::env::set_var("TELEGRAM_BOT_TOKEN", &tg.token);
        }
    }
    if let Some(ref dc) = config.channels.discord {
        if dc.enabled {
            std::env::set_var("DISCORD_BOT_TOKEN", &dc.token);
        }
    }

    // ── Channel manager (wrapped in Arc for shared access) ────
    let channel_registry = ChannelRegistry::new();
    let channel_manager = Arc::new(ChannelManager::new(channel_registry, bus.clone()));

    // ── Skill registry ───────────────────────────────────────
    let skill_registry = init_skill_registry(&home).await;

    // ── Agent loop ────────────────────────────────────────────
    let learning_bus = LearningEventBus::new();
    let (prompt_adjustment_tx, prompt_adjustment_rx) = watch::channel(None::<String>);

    // Initialize memory store early so it can be shared with the learning consumer.
    let memory_config = MemoryConfig {
        hot_store_path: home.join("memory").join("hot.jsonl"),
        ..MemoryConfig::default()
    };
    let memory_store: Option<Arc<dyn kestrel_memory::MemoryStore>> =
        match HotStore::new(&memory_config).await {
            Ok(hot_store) => {
                info!("Memory store initialized (HotStore L1)");
                Some(Arc::new(hot_store))
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to initialize memory store, continuing without memory: {}",
                    e
                );
                None
            }
        };
    let heartbeat_memory_store = memory_store.clone();
    let learning_memory_store = memory_store.clone();

    let agent_loop = {
        let mut al = AgentLoop::new(
            config.clone(),
            bus.clone(),
            session_manager.clone(),
            provider_registry.clone(),
            tool_registry.clone(),
        );

        // Wire memory store (HotStore L1)
        if let Some(ref ms) = memory_store {
            al = al.with_memory_store(ms.clone());
        }

        // Wire skill registry
        al = al.with_skill_registry(skill_registry.clone());

        // Wire learning event bus
        al = al.with_learning_bus(learning_bus.clone());

        // Wire prompt assembler for dynamic system prompt construction
        al = al.with_prompt_assembler(PromptAssembler::new());
        al = al.with_prompt_adjustments(prompt_adjustment_rx.clone());
        info!("Prompt assembler wired into agent loop");

        al
    };

    // ── Determine which channels to start ─────────────────────
    let channels_to_start = if channels.is_empty() {
        let mut auto = Vec::new();
        if let Some(ref tg) = config.channels.telegram {
            if tg.enabled {
                auto.push("telegram".to_string());
            }
        }
        if let Some(ref dc) = config.channels.discord {
            if dc.enabled {
                auto.push("discord".to_string());
            }
        }
        if let Some(ref ws) = config.channels.websocket {
            if ws.enabled {
                auto.push("websocket".to_string());
            }
        }
        if auto.is_empty() {
            info!("No channels configured; starting with local-only mode");
        }
        auto
    } else {
        channels
    };

    info!("Starting channels: {:?}", channels_to_start);

    for channel_name in &channels_to_start {
        if let Err(e) = channel_manager.start_channel(channel_name).await {
            tracing::error!("Failed to start channel '{}': {}", channel_name, e);
        }
    }

    // ── Heartbeat service ─────────────────────────────────────
    let mut heartbeat = HeartbeatService::with_registries(
        config.clone(),
        provider_registry.clone(),
        tool_registry.clone(),
        session_manager.clone(),
    );

    // ── API server (with registries for direct agent access) ──
    let api_server = ApiServer::with_registries(
        config.clone(),
        bus.clone(),
        session_manager,
        provider_registry.clone(),
        tool_registry,
        None,
    );

    let channel_statuses = prime_gateway_heartbeat(
        &mut heartbeat,
        &api_server,
        provider_registry.clone(),
        bus.clone(),
        heartbeat_memory_store,
        channel_manager.clone(),
    )
    .await?;

    // ── Spawn background tasks ────────────────────────────────
    let _agent_handle = tokio::spawn(async move {
        if let Err(e) = agent_loop.run().await {
            tracing::error!("Agent loop error: {}", e);
        }
    });

    let outbound_cm = channel_manager.clone();
    let _outbound_handle = tokio::spawn(async move {
        outbound_cm.run_outbound_consumer().await;
    });

    let health_cm = channel_manager.clone();
    let _channel_health_handle = tokio::spawn(async move {
        refresh_channel_health(health_cm, channel_statuses).await;
    });

    // ── Typing indicator lifecycle ──────────────────────────────
    let typing_cm = channel_manager.clone();
    let mut typing_event_rx = bus.subscribe_events();
    let _typing_handle = tokio::spawn(async move {
        loop {
            match typing_event_rx.recv().await {
                Ok(AgentEvent::Started { session_key }) => {
                    typing_cm.start_typing(&session_key);
                }
                Ok(AgentEvent::Completed { session_key, .. })
                | Ok(AgentEvent::Error { session_key, .. }) => {
                    typing_cm.stop_typing(&session_key);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Typing event consumer lagged by {n} messages");
                }
                Err(_) => break,
                _ => {}
            }
        }
    });

    let heartbeat_enabled = config.heartbeat.enabled;
    let _heartbeat_handle = tokio::spawn(async move {
        if heartbeat_enabled {
            if let Err(e) = heartbeat.run().await {
                tracing::error!("Heartbeat error: {}", e);
            }
        } else {
            // Park forever so the select! doesn't fire on a disabled service
            std::future::pending::<()>().await;
        }
    });

    let _api_handle = tokio::spawn(async move {
        if let Err(e) = api_server.run().await {
            tracing::error!("API server error: {}", e);
        }
    });

    // ── Learning event processor + persistent store ──────────
    let learning_config = LearningConfig::default();
    let event_store = EventStore::new(learning_config.event_log_file(), learning_config.max_events);
    info!(
        "EventStore initialized at {} (max_events={})",
        event_store.path().display(),
        learning_config.max_events
    );

    let (learning_shutdown_tx, mut learning_shutdown_rx) = watch::channel(false);
    let learning_handle = {
        let mut learning_rx = learning_bus.subscribe();
        let stats_path = learning_config.stats_file();
        let mut processor = BasicEventProcessor::new().with_stats_path(&stats_path);
        if let Err(e) = processor.load_stats().await {
            tracing::warn!("Failed to load processor stats, starting fresh: {}", e);
        } else {
            info!(
                "Loaded processor stats from {} ({} events processed)",
                stats_path.display(),
                processor.stats().events_processed
            );
        }
        let store = event_store.clone();
        let memory_store = learning_memory_store.clone();
        let skill_registry = skill_registry.clone();
        let prompt_adjustment_tx = prompt_adjustment_tx.clone();
        tokio::spawn(async move {
            run_learning_consumer(
                &mut learning_rx,
                &mut learning_shutdown_rx,
                store,
                &mut processor,
                memory_store,
                skill_registry,
                prompt_adjustment_tx,
            )
            .await;
        })
    };

    // ── Periodic event log prune task ─────────────────────────
    let _prune_handle = {
        let store = event_store.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600));
            loop {
                interval.tick().await;
                if let Err(e) = store.prune().await {
                    tracing::warn!("Event log prune failed: {}", e);
                }
            }
        })
    };

    info!("Gateway is running (API on port {})", config.api.port);
    info!("Press Ctrl+C to stop");

    // ── Wait for shutdown signal ──────────────────────────────
    #[cfg(target_family = "unix")]
    {
        loop {
            let sig = kestrel_daemon::signal::wait_for_signal().await;
            match sig {
                kestrel_daemon::signal::ShutdownSignal::Graceful => {
                    info!("Received graceful shutdown signal (SIGTERM)");
                    break;
                }
                kestrel_daemon::signal::ShutdownSignal::Fast => {
                    info!("Received fast shutdown signal (SIGINT)");
                    break;
                }
                kestrel_daemon::signal::ShutdownSignal::Reload => {
                    info!("Received SIGHUP (log rotation placeholder — not yet implemented)");
                    // Keep running; future sprint will add config reload
                    continue;
                }
            }
        }
    }

    #[cfg(not(target_family = "unix"))]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received shutdown signal");
            }
            _ = _agent_handle => {
                info!("Agent loop exited");
            }
            _ = _outbound_handle => {
                info!("Outbound consumer exited");
            }
            _ = _heartbeat_handle => {
                info!("Heartbeat exited");
            }
            _ = _api_handle => {
                info!("API server exited");
            }
            _ = _typing_handle => {
                info!("Typing handler exited");
            }
        }
    }

    // ── Graceful shutdown ─────────────────────────────────────
    info!("Stopping all channels...");
    channel_manager.stop_all().await;

    if learning_shutdown_tx.send(true).is_err() {
        tracing::warn!("Learning consumer shutdown channel closed unexpectedly");
    }
    if let Err(e) = learning_handle.await {
        tracing::warn!("Learning consumer task join failed: {}", e);
    }

    info!("Gateway shutting down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use kestrel_learning::event::SkillOutcome;
    use kestrel_memory::{MemoryQuery, ScoredEntry};
    use kestrel_skill::manifest::SkillManifestBuilder;
    use kestrel_skill::skill::CompiledSkill;
    use kestrel_skill::Skill;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tempfile::tempdir;

    #[derive(Default)]
    struct MockMemoryStore {
        stored: Mutex<Vec<MemoryEntry>>,
        fail_count: AtomicUsize,
    }

    impl MockMemoryStore {
        fn with_fail_count(fail_count: usize) -> Self {
            Self {
                stored: Mutex::new(Vec::new()),
                fail_count: AtomicUsize::new(fail_count),
            }
        }

        fn stored_entries(&self) -> Vec<MemoryEntry> {
            self.stored.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl MemoryStore for MockMemoryStore {
        async fn store(&self, entry: MemoryEntry) -> kestrel_memory::error::Result<()> {
            if self
                .fail_count
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                    if count > 0 {
                        Some(count - 1)
                    } else {
                        None
                    }
                })
                .is_ok()
            {
                return Err(kestrel_memory::MemoryError::Io(std::io::Error::other(
                    "mock store failure",
                )));
            }
            self.stored.lock().unwrap().push(entry);
            Ok(())
        }

        async fn recall(&self, _id: &str) -> kestrel_memory::error::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn search(
            &self,
            _query: &MemoryQuery,
        ) -> kestrel_memory::error::Result<Vec<ScoredEntry>> {
            Ok(Vec::new())
        }

        async fn delete(&self, _id: &str) -> kestrel_memory::error::Result<()> {
            Ok(())
        }

        async fn len(&self) -> usize {
            self.stored.lock().unwrap().len()
        }

        async fn clear(&self) -> kestrel_memory::error::Result<()> {
            self.stored.lock().unwrap().clear();
            Ok(())
        }
    }

    struct MockProcessor {
        actions: Vec<LearningAction>,
        save_stats_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl GatewayLearningProcessor for MockProcessor {
        async fn process_event(&mut self, _event: &LearningEvent) -> Vec<LearningAction> {
            self.actions.clone()
        }

        async fn save_stats(&self) -> Result<()> {
            self.save_stats_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn make_compiled_skill(name: &str) -> CompiledSkill {
        CompiledSkill::new(
            SkillManifestBuilder::new(name, "1.0.0", format!("Skill {name}"))
                .triggers(vec![name.to_string()])
                .build(),
        )
    }

    /// Write a valid TOML skill manifest to a directory.
    fn write_skill(dir: &Path, name: &str, triggers: &[&str]) -> std::path::PathBuf {
        let manifest = kestrel_skill::SkillManifest {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            description: format!("Skill {name}"),
            triggers: triggers.iter().map(|s| s.to_string()).collect(),
            steps: vec![],
            pitfalls: vec![],
            category: "test".to_string(),
            deprecated: None,
            deprecation_reason: None,
            confidence: None,
        };
        let path = dir.join(format!("{name}.toml"));
        std::fs::write(&path, toml::to_string(&manifest).unwrap()).unwrap();
        path
    }

    #[tokio::test]
    async fn test_init_skill_registry_loads_skills() {
        let home = tempfile::tempdir().unwrap();
        let skills_dir = home.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        write_skill(&skills_dir, "deploy-k8s", &["deploy", "k8s"]);
        write_skill(&skills_dir, "test-runner", &["test", "unit"]);

        let registry = init_skill_registry(home.path()).await;

        assert_eq!(registry.len().await, 2);
        let names = registry.skill_names().await;
        assert!(names.contains(&"deploy-k8s".to_string()));
        assert!(names.contains(&"test-runner".to_string()));
    }

    #[tokio::test]
    async fn test_init_skill_registry_missing_dir_returns_empty() {
        let home = tempfile::tempdir().unwrap();
        // No skills/ directory created

        let registry = init_skill_registry(home.path()).await;

        assert!(registry.is_empty().await);
    }

    #[tokio::test]
    async fn test_init_skill_registry_skips_invalid_manifests() {
        let home = tempfile::tempdir().unwrap();
        let skills_dir = home.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        write_skill(&skills_dir, "valid-skill", &["valid"]);
        // Write an invalid TOML file
        std::fs::write(skills_dir.join("bad.toml"), "not valid toml [[[[").unwrap();

        let registry = init_skill_registry(home.path()).await;

        // Only the valid skill should be loaded
        assert_eq!(registry.len().await, 1);
        assert!(registry.get("valid-skill").await.is_some());
    }

    #[tokio::test]
    async fn test_init_skill_registry_empty_dir_returns_empty() {
        let home = tempfile::tempdir().unwrap();
        let skills_dir = home.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        // Empty directory, no TOML files

        let registry = init_skill_registry(home.path()).await;

        assert!(registry.is_empty().await);
    }

    #[tokio::test]
    async fn test_init_skill_registry_skills_matchable() {
        let home = tempfile::tempdir().unwrap();
        let skills_dir = home.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        write_skill(&skills_dir, "deploy-k8s", &["deploy", "k8s"]);

        let registry = init_skill_registry(home.path()).await;

        // Verify loaded skills are matchable
        let matches = registry.match_skills("please deploy to k8s").await;
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].name, "deploy-k8s");
    }

    #[tokio::test]
    async fn test_init_skill_registry_with_steps_and_pitfalls() {
        let home = tempfile::tempdir().unwrap();
        let skills_dir = home.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        let manifest = kestrel_skill::SkillManifest {
            name: "deploy-k8s".to_string(),
            version: "1.0.0".to_string(),
            description: "Deploy to Kubernetes".to_string(),
            triggers: vec!["deploy".to_string(), "k8s".to_string()],
            steps: vec!["Apply manifests".to_string(), "Verify rollout".to_string()],
            pitfalls: vec!["Do not deploy on Fridays".to_string()],
            category: "devops".to_string(),
            deprecated: None,
            deprecation_reason: None,
            confidence: None,
        };
        let path = skills_dir.join("deploy-k8s.toml");
        std::fs::write(&path, toml::to_string(&manifest).unwrap()).unwrap();

        let registry = init_skill_registry(home.path()).await;
        let skill = registry.get("deploy-k8s").await.unwrap();
        let guard = skill.read();
        let m = guard.manifest();

        assert_eq!(m.steps, vec!["Apply manifests", "Verify rollout"]);
        assert_eq!(m.pitfalls, vec!["Do not deploy on Fridays"]);
    }

    #[tokio::test]
    async fn test_record_insight_action_stores_memory() {
        let store_impl = Arc::new(MockMemoryStore::default());
        let memory_store: Arc<dyn MemoryStore> = store_impl.clone();
        let skill_registry = SkillRegistry::new();
        let (prompt_tx, _prompt_rx) = watch::channel(None::<String>);

        execute_learning_action(
            &LearningAction::RecordInsight {
                insight: "remember this".into(),
                category: "environment".into(),
            },
            Some(&memory_store),
            &skill_registry,
            &prompt_tx,
        )
        .await
        .unwrap();

        let entries = store_impl.stored_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "remember this");
        assert_eq!(entries[0].category, MemoryCategory::Environment);
    }

    #[tokio::test]
    async fn test_failed_action_does_not_stop_subsequent_actions() {
        let store_impl = Arc::new(MockMemoryStore::with_fail_count(1));
        let memory_store: Arc<dyn MemoryStore> = store_impl.clone();
        let skill_registry = SkillRegistry::new();
        skill_registry
            .register(make_compiled_skill("deploy"))
            .await
            .unwrap();
        let (prompt_tx, _prompt_rx) = watch::channel(None::<String>);

        let actions = vec![
            LearningAction::RecordInsight {
                insight: "first write fails".into(),
                category: "environment".into(),
            },
            LearningAction::AdjustConfidence {
                skill: "deploy".into(),
                delta: 0.2,
            },
        ];

        execute_learning_actions(&actions, Some(&memory_store), &skill_registry, &prompt_tx).await;

        let skill = skill_registry.get("deploy").await.unwrap();
        assert!(skill.read().confidence() > 0.5);
        assert!(store_impl.stored_entries().is_empty());
    }

    #[tokio::test]
    async fn test_learning_consumer_shutdown_saves_stats() {
        let learning_bus = LearningEventBus::new();
        let mut learning_rx = learning_bus.subscribe();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let save_stats_calls = Arc::new(AtomicUsize::new(0));
        let mut processor = MockProcessor {
            actions: vec![LearningAction::NoOp],
            save_stats_calls: save_stats_calls.clone(),
        };
        let event_dir = tempdir().unwrap();
        let event_store = EventStore::new(event_dir.path().join("events.jsonl"), 10);
        let skill_registry = Arc::new(SkillRegistry::new());
        let (prompt_tx, _prompt_rx) = watch::channel(None::<String>);

        let handle = tokio::spawn(async move {
            run_learning_consumer(
                &mut learning_rx,
                &mut shutdown_rx,
                event_store,
                &mut processor,
                None,
                skill_registry,
                prompt_tx,
            )
            .await;
        });

        learning_bus.publish(LearningEvent::SkillUsed {
            skill_name: "deploy".into(),
            match_score: 0.8,
            outcome: SkillOutcome::Helpful,
            timestamp: Utc::now(),
        });
        tokio::task::yield_now().await;

        shutdown_tx.send(true).unwrap();
        handle.await.unwrap();

        assert!(save_stats_calls.load(Ordering::SeqCst) >= 1);
    }
}
