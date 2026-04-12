//! Heartbeat command — start the periodic task checking service.

use anyhow::Result;
use nanobot_config::Config;
use nanobot_heartbeat::HeartbeatService;
use nanobot_providers::ProviderRegistry;
use nanobot_session::SessionManager;
use nanobot_tools::builtins;
use tracing::info;

/// Run the heartbeat service.
pub async fn run(config: Config) -> Result<()> {
    info!("Starting nanobot heartbeat service...");

    let home = nanobot_config::paths::get_nanobot_home()?;
    let session_manager = SessionManager::new(home)?;

    let provider_registry = ProviderRegistry::from_config(&config)?;
    info!("Providers: {:?}", provider_registry.provider_names());

    let tool_registry = nanobot_tools::ToolRegistry::new();
    builtins::register_all(&tool_registry);
    info!("Tools: {:?}", tool_registry.tool_names());

    let heartbeat = HeartbeatService::with_registries(
        config.clone(),
        provider_registry,
        tool_registry,
        session_manager,
    );

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
