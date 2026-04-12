//! Setup command — interactive configuration wizard.

use anyhow::Result;
use nanobot_config::Config;

/// Run the interactive setup wizard.
pub fn run(config: Config) -> Result<()> {
    println!("=== Nanobot Setup ===\n");

    println!("Current configuration:");
    println!("  Model: {}", config.agent.model);
    println!("  Temperature: {}", config.agent.temperature);
    println!("  Max tokens: {}", config.agent.max_tokens);
    println!("  Streaming: {}", config.agent.streaming);
    println!();

    println!("Providers:");
    let providers = [
        ("Anthropic", config.providers.anthropic.is_some()),
        ("OpenAI", config.providers.openai.is_some()),
        ("DeepSeek", config.providers.deepseek.is_some()),
        ("Groq", config.providers.groq.is_some()),
        ("OpenRouter", config.providers.openrouter.is_some()),
        ("Ollama", config.providers.ollama.is_some()),
    ];
    for (name, configured) in &providers {
        let status = if *configured {
            "configured"
        } else {
            "not configured"
        };
        println!("  {}: {}", name, status);
    }
    println!();

    println!("Channels:");
    let channels = [
        ("Telegram", config.channels.telegram.is_some()),
        ("Discord", config.channels.discord.is_some()),
    ];
    for (name, configured) in &channels {
        let status = if *configured {
            "configured"
        } else {
            "not configured"
        };
        println!("  {}: {}", name, status);
    }
    println!();

    // Save config
    let config_path = nanobot_config::paths::get_config_path()?;
    println!("Saving configuration to: {}", config_path.display());
    nanobot_config::loader::save_config(&config, &config_path)?;

    println!("Setup complete.");
    println!("Edit {} to customize further.", config_path.display());

    Ok(())
}
