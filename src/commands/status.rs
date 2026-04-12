//! Status command — show current configuration and system status.

use anyhow::Result;
use nanobot_config::Config;

/// Show status information.
pub fn run(config: &Config) -> Result<()> {
    println!("=== Nanobot Status ===\n");
    println!("Version: {}", nanobot_core::VERSION);
    println!();

    println!("Configuration:");
    println!("  Model: {}", config.agent.model);
    println!("  Temperature: {}", config.agent.temperature);
    println!("  Max tokens: {}", config.agent.max_tokens);
    println!("  Max iterations: {}", config.agent.max_iterations);
    println!("  Streaming: {}", config.agent.streaming);
    println!("  Tool timeout: {}s", config.agent.tool_timeout);
    println!();

    // Provider status — attempt to build registry to show what's available
    println!("Providers:");
    let provider_entries = [
        ("Anthropic", config.providers.anthropic.as_ref()),
        ("OpenAI", config.providers.openai.as_ref()),
        ("DeepSeek", config.providers.deepseek.as_ref()),
        ("Groq", config.providers.groq.as_ref()),
        ("OpenRouter", config.providers.openrouter.as_ref()),
        ("Ollama", config.providers.ollama.as_ref()),
        ("Gemini", config.providers.gemini.as_ref()),
    ];
    for (name, entry) in &provider_entries {
        let status = match entry {
            Some(e) => {
                let has_key = e.api_key.as_ref().is_some_and(|k| !k.is_empty());
                if has_key {
                    "configured (API key set)"
                } else if name == &"Ollama" {
                    "configured (local)"
                } else {
                    "configured (no API key)"
                }
            }
            None => "not configured",
        };
        println!("  {}: {}", name, status);
    }
    for custom in &config.custom_providers {
        println!("  Custom [{}]: {}", custom.name, custom.base_url);
    }
    println!();

    // Channel status
    println!("Channels:");
    let channels = [
        (
            "Telegram",
            config.channels.telegram.as_ref().map(|c| c.enabled),
        ),
        (
            "Discord",
            config.channels.discord.as_ref().map(|c| c.enabled),
        ),
        ("Slack", config.channels.slack.as_ref().map(|c| c.enabled)),
        ("Matrix", config.channels.matrix.as_ref().map(|c| c.enabled)),
    ];
    for (name, enabled) in &channels {
        let status = match enabled {
            Some(true) => "enabled",
            Some(false) => "disabled",
            None => "not configured",
        };
        println!("  {}: {}", name, status);
    }
    println!();

    println!("Services:");
    println!(
        "  Heartbeat: {}",
        if config.heartbeat.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "  Dream: {}",
        if config.dream.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );

    // Config file location
    if let Ok(path) = nanobot_config::paths::get_config_path() {
        let exists = path.exists();
        println!(
            "\nConfig file: {} {}",
            path.display(),
            if exists {
                "(exists)"
            } else {
                "(not found, using defaults)"
            }
        );
    }

    if let Ok(home) = nanobot_config::paths::get_nanobot_home() {
        println!("Data directory: {}", home.display());
    }

    Ok(())
}
