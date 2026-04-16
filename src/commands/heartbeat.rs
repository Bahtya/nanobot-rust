//! Heartbeat command — start the periodic task checking service.

use anyhow::Result;
use nanobot_bus::MessageBus;
use nanobot_config::Config;
use nanobot_heartbeat::{
    BusHealthCheck, HeartbeatService, MemoryStoreHealthCheck, ProviderHealthCheck,
};
use nanobot_memory::{HotStore, MemoryConfig, MemoryStore};
use nanobot_providers::ProviderRegistry;
use nanobot_session::SessionManager;
use nanobot_tools::builtins;
use tracing::info;

/// Run the heartbeat service.
pub async fn run(config: Config, dangerous: bool) -> Result<()> {
    info!("Starting nanobot heartbeat service...");

    let home = nanobot_config::paths::get_nanobot_home()?;
    let session_manager = SessionManager::new(home.clone())?;
    let bus = MessageBus::new();

    let provider_registry = ProviderRegistry::from_config(&config)?;
    info!("Providers: {:?}", provider_registry.provider_names());

    let tool_registry = nanobot_tools::ToolRegistry::new();
    builtins::register_all_with_config(&tool_registry, builtins::BuiltinsConfig { dangerous });
    info!("Tools: {:?}", tool_registry.tool_names());

    let mut heartbeat = HeartbeatService::with_registries(
        config.clone(),
        provider_registry.clone(),
        tool_registry,
        session_manager,
    );
    heartbeat.set_bus(bus.clone());
    heartbeat.register_check(std::sync::Arc::new(ProviderHealthCheck::new(
        std::sync::Arc::new(provider_registry),
    )));
    heartbeat.register_check(std::sync::Arc::new(BusHealthCheck::new(bus)));

    let memory_config = MemoryConfig {
        hot_store_path: home.join("memory").join("hot.jsonl"),
        ..MemoryConfig::default()
    };
    match HotStore::new(&memory_config).await {
        Ok(hot_store) => {
            let store: std::sync::Arc<dyn MemoryStore> = std::sync::Arc::new(hot_store);
            heartbeat.register_check(std::sync::Arc::new(MemoryStoreHealthCheck::new(store)));
        }
        Err(e) => {
            tracing::warn!(
                "Failed to initialize memory store for heartbeat checks, continuing without memory: {}",
                e
            );
        }
    }
    heartbeat.run_checks().await?;

    info!(
        "Heartbeat running (interval: {}s, press Ctrl+C to stop)",
        config.heartbeat.interval_secs
    );

    tokio::select! {
        result = heartbeat.run() => {
            if let Err(e) = result {
                tracing::error!("Heartbeat error: {}", e);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received shutdown signal");
        }
    }

    Ok(())
}
