//! Gateway command — start the full nanobot gateway.
//!
//! Wires together: bus, channels, agent loop, session manager,
//! provider registry, tool registry, skill registry, heartbeat, and API server.
//!
//! Supports daemon mode when launched via `nanobot-rs daemon start`.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use nanobot_agent::AgentLoop;
use nanobot_api::ApiServer;
use nanobot_bus::events::AgentEvent;
use nanobot_bus::MessageBus;
use nanobot_channels::{ChannelManager, ChannelRegistry};
use nanobot_config::Config;
use nanobot_heartbeat::HeartbeatService;
use nanobot_memory::{HotStore, MemoryConfig};
use nanobot_providers::ProviderRegistry;
use nanobot_session::SessionManager;
use nanobot_skill::{SkillConfig, SkillLoader, SkillRegistry};
use nanobot_tools::builtins;
use tracing::info;

/// Initialize the skill registry by loading TOML manifests from the skills directory.
///
/// Looks for `skills/` under the given nanobot home directory. If the directory
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
            tracing::warn!(
                "Failed to load skills from {}: {}",
                skills_dir.display(),
                e
            );
        }
    }

    registry
}

/// Run the gateway — starts all components and connects them via the bus.
///
/// PID file management is handled by the `daemon start` command, not here.
pub async fn run(config: Config, channels: Vec<String>) -> Result<()> {
    info!("Starting nanobot gateway...");

    // ── Shared bus ────────────────────────────────────────────
    let bus = MessageBus::new();

    // ── Session manager ───────────────────────────────────────
    let home = nanobot_config::paths::get_nanobot_home()?;
    let session_manager = SessionManager::new(home.clone())?;

    // ── Provider registry ─────────────────────────────────────
    let provider_registry = ProviderRegistry::from_config(&config)?;
    info!("Providers: {:?}", provider_registry.provider_names());

    // ── Tool registry ─────────────────────────────────────────
    let tool_registry = nanobot_tools::ToolRegistry::new();
    builtins::register_all(&tool_registry);
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
    let agent_loop = {
        let mut al = AgentLoop::new(
            config.clone(),
            bus.clone(),
            session_manager.clone(),
            provider_registry.clone(),
            tool_registry.clone(),
        );

        // Wire memory store (HotStore L1)
        let memory_config = MemoryConfig {
            hot_store_path: home.join("memory").join("hot.jsonl"),
            ..MemoryConfig::default()
        };
        match HotStore::new(&memory_config).await {
            Ok(hot_store) => {
                info!("Memory store initialized (HotStore L1)");
                al = al.with_memory_store(Arc::new(hot_store));
            }
            Err(e) => {
                tracing::warn!("Failed to initialize memory store, continuing without memory: {}", e);
            }
        }

        // Wire skill registry
        al = al.with_skill_registry(skill_registry);

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
    let heartbeat = HeartbeatService::with_registries(
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
        provider_registry,
        tool_registry,
        None,
    );

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

    info!("Gateway is running (API on port {})", config.api.port);
    info!("Press Ctrl+C to stop");

    // ── Wait for shutdown signal ──────────────────────────────
    #[cfg(target_family = "unix")]
    {
        loop {
            let sig = nanobot_daemon::signal::wait_for_signal().await;
            match sig {
                nanobot_daemon::signal::ShutdownSignal::Graceful => {
                    info!("Received graceful shutdown signal (SIGTERM)");
                    break;
                }
                nanobot_daemon::signal::ShutdownSignal::Fast => {
                    info!("Received fast shutdown signal (SIGINT)");
                    break;
                }
                nanobot_daemon::signal::ShutdownSignal::Reload => {
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

    info!("Gateway shutting down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a valid TOML skill manifest to a directory.
    fn write_skill(dir: &Path, name: &str, triggers: &[&str]) -> std::path::PathBuf {
        let manifest = nanobot_skill::SkillManifest {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            description: format!("Skill {name}"),
            triggers: triggers.iter().map(|s| s.to_string()).collect(),
            steps: vec![],
            pitfalls: vec![],
            category: "test".to_string(),
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

        let manifest = nanobot_skill::SkillManifest {
            name: "deploy-k8s".to_string(),
            version: "1.0.0".to_string(),
            description: "Deploy to Kubernetes".to_string(),
            triggers: vec!["deploy".to_string(), "k8s".to_string()],
            steps: vec!["Apply manifests".to_string(), "Verify rollout".to_string()],
            pitfalls: vec!["Do not deploy on Fridays".to_string()],
            category: "devops".to_string(),
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
}
