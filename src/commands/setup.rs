//! Setup command — interactive wizard for configuring Kestrel.

use anyhow::{bail, Context, Result};
use console::Term;
use dialoguer::{Confirm, Input, Password as PasswordInput, Select};
use kestrel_config::{
    loader, paths,
    schema::{Config, FeishuConfig, ProviderEntry, TelegramConfig, WebSocketConfig, WeixinConfig},
};
use owo_colors::OwoColorize;
use std::net::SocketAddr;
use std::path::Path;

const PROVIDER_NAMES: &[&str] = &[
    "anthropic",
    "openai",
    "openrouter",
    "ollama",
    "deepseek",
    "gemini",
    "groq",
    "moonshot",
    "minimax",
    "github_copilot",
    "openai_codex",
    "glm_coding_plan",
];

const TOTAL_STEPS: usize = 6;

// ── Trait for interactive I/O (enables testability) ────────────

trait WizardIo {
    fn confirm(&self, prompt: &str, default: bool) -> Result<bool>;
    fn select(&self, prompt: &str, items: &[&str], default: usize) -> Result<usize>;
    fn input_with_default(&self, prompt: &str, default: &str) -> Result<String>;
    fn input_allow_empty(&self, prompt: &str) -> Result<String>;
    fn password(&self, prompt: &str) -> Result<String>;
    fn write_line(&self, line: &str) -> Result<()>;
}

// ── Production implementation using Term + dialoguer ───────────

struct TermWizard<'a> {
    term: &'a Term,
}

impl<'a> TermWizard<'a> {
    fn new(term: &'a Term) -> Self {
        Self { term }
    }
}

impl WizardIo for TermWizard<'_> {
    fn confirm(&self, prompt: &str, default: bool) -> Result<bool> {
        Ok(Confirm::new()
            .with_prompt(prompt)
            .default(default)
            .interact_on(self.term)?)
    }

    fn select(&self, prompt: &str, items: &[&str], default: usize) -> Result<usize> {
        Ok(Select::new()
            .with_prompt(prompt)
            .items(items)
            .default(default)
            .interact_on(self.term)?)
    }

    fn input_with_default(&self, prompt: &str, default: &str) -> Result<String> {
        Ok(Input::new()
            .with_prompt(prompt)
            .default(default.to_string())
            .interact_text_on(self.term)?)
    }

    fn input_allow_empty(&self, prompt: &str) -> Result<String> {
        Ok(Input::<String>::new()
            .with_prompt(prompt)
            .allow_empty(true)
            .interact_text_on(self.term)?)
    }

    fn password(&self, prompt: &str) -> Result<String> {
        Ok(PasswordInput::new()
            .with_prompt(prompt)
            .interact_on(self.term)?)
    }

    fn write_line(&self, line: &str) -> Result<()> {
        self.term.write_line(line)?;
        Ok(())
    }
}

// ── Provider dispatch macro (single source of truth) ───────────

#[allow(unused_macros)]
macro_rules! provider_field {
    ($config:expr, $provider:expr, mut) => {
        match $provider {
            "anthropic" => Some(&mut $config.providers.anthropic),
            "openai" => Some(&mut $config.providers.openai),
            "openrouter" => Some(&mut $config.providers.openrouter),
            "ollama" => Some(&mut $config.providers.ollama),
            "deepseek" => Some(&mut $config.providers.deepseek),
            "gemini" => Some(&mut $config.providers.gemini),
            "groq" => Some(&mut $config.providers.groq),
            "moonshot" => Some(&mut $config.providers.moonshot),
            "minimax" => Some(&mut $config.providers.minimax),
            "github_copilot" => Some(&mut $config.providers.github_copilot),
            "openai_codex" => Some(&mut $config.providers.openai_codex),
            _ => None,
        }
    };
    ($config:expr, $provider:expr) => {
        match $provider {
            "anthropic" => Some(&$config.providers.anthropic),
            "openai" => Some(&$config.providers.openai),
            "openrouter" => Some(&$config.providers.openrouter),
            "ollama" => Some(&$config.providers.ollama),
            "deepseek" => Some(&$config.providers.deepseek),
            "gemini" => Some(&$config.providers.gemini),
            "groq" => Some(&$config.providers.groq),
            "moonshot" => Some(&$config.providers.moonshot),
            "minimax" => Some(&$config.providers.minimax),
            "github_copilot" => Some(&$config.providers.github_copilot),
            "openai_codex" => Some(&$config.providers.openai_codex),
            _ => None,
        }
    };
}

// ── Entry point ────────────────────────────────────────────────

pub fn run(_config: Config) -> Result<()> {
    let term = Term::stdout();
    if !term.is_term() {
        bail!(
            "Setup requires an interactive terminal. \
             Run this command in a terminal, not in a pipe or CI environment."
        );
    }
    let io = TermWizard::new(&term);
    let config_path = paths::get_config_path()?;
    run_wizard(&io, &config_path)
}

// ── Wizard flow ────────────────────────────────────────────────

fn run_wizard(io: &dyn WizardIo, config_path: &Path) -> Result<()> {
    print_banner(io)?;

    // ── Step 1: Check existing config ────────────────────────────
    print_step(io, 1, "Existing Configuration")?;

    let mut config = if config_path.exists() {
        match load_existing_config(config_path) {
            Ok(existing) => {
                show_config_summary(io, &existing)?;
                if io.confirm("Update existing config?", true)? {
                    existing
                } else {
                    io.write_line(&format!(
                        "  {} Keeping config at {}.",
                        "✓".green(),
                        config_path.display()
                    ))?;
                    return Ok(());
                }
            }
            Err(e) => {
                io.write_line(&format!(
                    "  {} Could not parse existing config: {}",
                    "!".yellow(),
                    e
                ))?;
                if io.confirm("Start fresh with defaults?", true)? {
                    Config::default()
                } else {
                    bail!("Setup cancelled.");
                }
            }
        }
    } else {
        io.write_line("  No config file found. Starting fresh.")?;
        Config::default()
    };

    // ── Step 2: Provider configuration ───────────────────────────
    print_step(io, 2, "Provider Configuration")?;
    configure_provider(io, &mut config)?;

    // ── Step 3: Telegram channel ─────────────────────────────────
    print_step(io, 3, "Telegram Channel")?;
    configure_telegram(io, &mut config)?;

    // ── Step 4: Feishu / Lark channel ────────────────────────────
    print_step(io, 4, "Feishu / Lark Channel")?;
    configure_feishu(io, &mut config)?;

    // ── Step 5: WebSocket port ───────────────────────────────────
    print_step(io, 5, "WebSocket Port")?;
    configure_websocket(io, &mut config)?;

    // ── Step 5: Weixin channel ───────────────────────────────────
    print_step(io, 5, "WeChat Channel")?;
    configure_weixin(io, &mut config)?;

    // ── Step 6: Validate & write ─────────────────────────────────
    print_step(io, 6, "Save Configuration")?;

    io.write_line(&format!("  Config path: {}", config_path.display()))?;
    io.write_line("")?;
    show_config_summary(io, &config)?;
    io.write_line("")?;

    if !io.confirm("Write this configuration?", true)? {
        io.write_line(&format!("  {} Setup cancelled.", "!".yellow()))?;
        return Ok(());
    }

    let home = config_path
        .parent()
        .context("Config path must have a parent directory")?;

    std::fs::create_dir_all(home)
        .with_context(|| format!("Failed to create config home: {}", home.display()))?;

    loader::save_config(&config, config_path)?;

    for dir in ["skills", "sessions", "learning"] {
        let path = home.join(dir);
        std::fs::create_dir_all(&path)
            .with_context(|| format!("Failed to create directory: {}", path.display()))?;
    }

    io.write_line("")?;
    io.write_line(&format!(
        "  {} Configuration saved to {}",
        "✓".green(),
        config_path.display()
    ))?;
    io.write_line(&format!(
        "  {} Created directories: skills, sessions, learning",
        "✓".green()
    ))?;
    io.write_line(&format!("  {} Setup complete!", "✓".green()))?;

    Ok(())
}

fn print_banner(io: &dyn WizardIo) -> Result<()> {
    io.write_line("")?;
    io.write_line(&format!(
        "  {} {}",
        "▸".cyan(),
        "Kestrel Setup Wizard".bold().cyan()
    ))?;
    io.write_line("")?;
    Ok(())
}

fn print_step(io: &dyn WizardIo, step: usize, title: &str) -> Result<()> {
    io.write_line("")?;
    io.write_line(&format!(
        "  {} Step {}/{}: {}",
        "▸".cyan(),
        step,
        TOTAL_STEPS,
        title.bold()
    ))?;
    io.write_line(&format!("  {}", "─".repeat(40).dimmed()))?;
    Ok(())
}

fn load_existing_config(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let config: Config =
        toml::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(config)
}

fn mask_token(token: &str) -> String {
    if token.is_empty() {
        "(not set)".to_string()
    } else if token.len() <= 3 {
        "(masked)".to_string()
    } else {
        format!("{}…(masked)", &token[..3])
    }
}

fn show_config_summary(io: &dyn WizardIo, config: &Config) -> Result<()> {
    let provider = config.agent.provider.as_deref().unwrap_or("default");
    io.write_line(&format!("  Model:        {}", config.agent.model))?;
    io.write_line(&format!("  Provider:     {}", provider))?;
    io.write_line(&format!("  Temperature:  {}", config.agent.temperature))?;
    io.write_line(&format!("  Max tokens:   {}", config.agent.max_tokens))?;
    io.write_line(&format!("  Streaming:    {}", config.agent.streaming))?;

    if let Some(ref tg) = config.channels.telegram {
        io.write_line(&format!("  Telegram:     {}", mask_token(&tg.token)))?;
    }

    if let Some(ref fs) = config.channels.feishu {
        if fs.enabled {
            let app_id = fs.app_id.as_deref().unwrap_or("(not set)");
            io.write_line(&format!("  Feishu:       {}", mask_token(app_id)))?;
        }
    }

    if let Some(ref ws) = config.channels.websocket {
        if ws.enabled {
            io.write_line(&format!("  WebSocket:    {}", ws.listen_addr))?;
        }
    }

    if let Some(ref wx) = config.channels.weixin {
        let status = if wx.enabled {
            mask_token(wx.bot_token.as_deref().unwrap_or(""))
        } else {
            "disabled".to_string()
        };
        io.write_line(&format!("  WeChat:       {}", status))?;
    }

    Ok(())
}

fn configure_provider(io: &dyn WizardIo, config: &mut Config) -> Result<()> {
    let default_idx = config
        .agent
        .provider
        .as_deref()
        .and_then(|p| PROVIDER_NAMES.iter().position(|&n| n == p))
        .unwrap_or(1); // default to "openai"

    let provider_name = io.select("Select LLM provider", PROVIDER_NAMES, default_idx)?;

    let provider_key = PROVIDER_NAMES[provider_name];
    config.agent.provider = Some(provider_key.to_string());

    let default_model = match provider_key {
        "anthropic" => "claude-sonnet-4-20250514",
        "openai" => "gpt-4o",
        "openrouter" => "anthropic/claude-sonnet-4-20250514",
        "ollama" => "llama3",
        "deepseek" => "deepseek-chat",
        "gemini" => "gemini-2.5-pro",
        "groq" => "llama-3.3-70b-versatile",
        "moonshot" => "moonshot-v1-8k",
        "minimax" => "MiniMax-Text-01",
        "github_copilot" => "gpt-4o",
        "openai_codex" => "codex-mini",
        "glm_coding_plan" => "glm-5.1",
        _ => "gpt-4o",
    };

    let current_model = if config.agent.model.is_empty() {
        default_model
    } else {
        &config.agent.model
    };

    let model: String = io.input_with_default("Model name", current_model)?;
    config.agent.model = model;

    let default_url = match provider_key {
        "anthropic" => "https://api.anthropic.com",
        "openai" => "https://api.openai.com/v1",
        "openrouter" => "https://openrouter.ai/api/v1",
        "ollama" => "http://localhost:11434",
        "deepseek" => "https://api.deepseek.com",
        "gemini" => "https://generativelanguage.googleapis.com/v1beta",
        "groq" => "https://api.groq.com/openai/v1",
        "moonshot" => "https://api.moonshot.cn/v1",
        "minimax" => "https://api.minimax.chat/v1",
        "github_copilot" => "https://api.githubcopilot.com",
        "openai_codex" => "https://api.openai.com/v1",
        "glm_coding_plan" => "https://open.bigmodel.cn/api/coding/paas/v4",
        _ => "",
    };

    let current_url = get_provider_url(config, provider_key).unwrap_or(default_url);

    if !current_url.is_empty() {
        let base_url: String = io.input_with_default("Base URL", current_url)?;
        set_provider_url(config, provider_key, &base_url);
    } else {
        let base_url: String = io.input_allow_empty("Base URL (leave empty for default)")?;
        if !base_url.is_empty() {
            set_provider_url(config, provider_key, &base_url);
        }
    }

    let api_key: String = io.password("API key")?;
    if !api_key.is_empty() {
        set_provider_api_key(config, provider_key, &api_key);
    }

    Ok(())
}

fn configure_telegram(io: &dyn WizardIo, config: &mut Config) -> Result<()> {
    let setup_tg = if config.channels.telegram.is_some() {
        io.confirm("Configure Telegram bot?", true)?
    } else {
        io.confirm("Set up a Telegram bot?", false)?
    };

    if !setup_tg {
        io.write_line("  Skipped.")?;
        return Ok(());
    }

    let current_token = config
        .channels
        .telegram
        .as_ref()
        .map(|tg| tg.token.as_str())
        .unwrap_or("");

    let token: String = if current_token.is_empty() {
        io.input_allow_empty("Bot token (from @BotFather)")?
    } else {
        io.input_with_default("Bot token (from @BotFather)", current_token)?
    };

    if token.is_empty() {
        io.write_line("  No token provided, skipping Telegram.")?;
        return Ok(());
    }

    let allowed: String = loop {
        let input: String =
            io.input_allow_empty("Allowed user IDs (comma-separated, leave empty for all)")?;
        if input.trim().is_empty() {
            break String::new();
        }
        let ids: Vec<&str> = input
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        let mut valid = true;
        for id in &ids {
            if id.parse::<u64>().is_err() {
                io.write_line(&format!(
                    "  {} '{}' is not a valid user ID (must be a positive integer).",
                    "!".yellow(),
                    id
                ))?;
                valid = false;
                break;
            }
        }
        if valid {
            break input;
        }
    };

    let allowed_users: Vec<String> = if allowed.trim().is_empty() {
        Vec::new()
    } else {
        allowed
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };

    // Preserve existing values for fields not explicitly configured in the wizard.
    let (admin_users, enabled, streaming, proxy) = config
        .channels
        .telegram
        .as_ref()
        .map(|tg| {
            (
                tg.admin_users.clone(),
                tg.enabled,
                tg.streaming,
                tg.proxy.clone(),
            )
        })
        .unwrap_or((Vec::new(), true, true, None));

    config.channels.telegram = Some(TelegramConfig {
        token,
        allowed_users,
        admin_users,
        enabled,
        streaming,
        proxy,
    });

    Ok(())
}

fn configure_feishu(io: &dyn WizardIo, config: &mut Config) -> Result<()> {
    let has_existing = config.channels.feishu.as_ref().map_or(false, |f| f.enabled);

    let setup = if has_existing {
        io.confirm("Reconfigure Feishu / Lark?", true)?
    } else {
        io.confirm("Set up Feishu / Lark?", false)?
    };

    if !setup {
        io.write_line("  Skipped.")?;
        return Ok(());
    }

    let domain_options = ["Feishu (飞书)", "Lark (international)"];
    let idx = io.select("Select platform", &domain_options, 0)?;
    let domain = if idx == 1 { "lark" } else { "feishu" };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("Failed to create tokio runtime")?;

    // TODO: Creating a nested tokio runtime here means this cannot be called
    // from within an existing async context (e.g. tests). A future refactor
    // could make `run_wizard` async or accept a runtime handle.
    let result = match rt.block_on(feishu_onboarding::run_onboarding(domain)) {
        Ok(r) => r,
        Err(e) => {
            io.write_line(&format!("  {} Feishu setup skipped: {}", "!".yellow(), e))?;
            return Ok(());
        }
    };

    let existing_proxy = config
        .channels
        .feishu
        .as_ref()
        .and_then(|f| f.proxy.clone());

    config.channels.feishu = Some(FeishuConfig {
        app_id: Some(result.app_id),
        app_secret: Some(result.app_secret),
        enabled: true,
        proxy: existing_proxy,
    });

    Ok(())
}

fn configure_websocket(io: &dyn WizardIo, config: &mut Config) -> Result<()> {
    let default_addr = config
        .channels
        .websocket
        .as_ref()
        .map(|ws| ws.listen_addr.as_str())
        .unwrap_or("127.0.0.1:8090");

    let enable = io.confirm("Enable WebSocket channel?", false)?;

    if !enable {
        config.channels.websocket = None;
        io.write_line("  Skipped.")?;
        return Ok(());
    }

    let listen_addr: String = loop {
        let input = io.input_with_default("Listen address", default_addr)?;
        match input.parse::<SocketAddr>() {
            Ok(_) => break input,
            Err(e) => {
                io.write_line(&format!(
                    "  {} Invalid address '{}': {}",
                    "!".yellow(),
                    input,
                    e
                ))?;
            }
        }
    };

    config.channels.websocket = Some(WebSocketConfig {
        enabled: true,
        listen_addr,
        auth: Default::default(),
        max_clients: 100,
        max_message_size: 1048576,
    });

    Ok(())
}

fn configure_weixin(io: &dyn WizardIo, config: &mut Config) -> Result<()> {
    let setup_wx = if config.channels.weixin.is_some() {
        io.confirm("Configure WeChat channel?", true)?
    } else {
        io.confirm("Set up a WeChat channel?", false)?
    };

    if !setup_wx {
        io.write_line("  Skipped.")?;
        return Ok(());
    }

    let choices = &[
        "Scan QR code with WeChat (recommended)",
        "Enter credentials manually",
        "Skip",
    ];
    let choice = io.select("How would you like to configure WeChat?", choices, 0)?;

    match choice {
        0 => {
            io.write_line(
                "  QR scan setup requires running `kestrel setup weixin` in a terminal.",
            )?;
            io.write_line("  Please run that command separately, then return to this wizard.")?;
            // Mark as enabled if credentials already exist
            if let Some(ref wx) = config.channels.weixin {
                if wx.account_id.is_some() && wx.bot_token.is_some() {
                    io.write_line(&format!(
                        "  {} Existing WeChat credentials detected.",
                        "✓".green()
                    ))?;
                }
            }
            // If no credentials yet, just leave channel unconfigured
            if config.channels.weixin.is_none() {
                io.write_line(
                    "  No WeChat credentials found yet. Run `kestrel setup weixin` first.",
                )?;
            }
        }
        1 => {
            let current_account = config
                .channels
                .weixin
                .as_ref()
                .and_then(|w| w.account_id.as_deref())
                .unwrap_or("");
            let account_id: String = if current_account.is_empty() {
                io.input_allow_empty("iLink account ID (e.g. wxid_xxx@im.bot)")?
            } else {
                io.input_with_default("iLink account ID", current_account)?
            };

            if account_id.trim().is_empty() {
                io.write_line("  No account ID provided, skipping WeChat.")?;
                return Ok(());
            }

            let current_token = config
                .channels
                .weixin
                .as_ref()
                .and_then(|w| w.bot_token.as_deref())
                .unwrap_or("");
            let bot_token: String = if current_token.is_empty() {
                io.input_allow_empty("iLink bot token")?
            } else {
                io.input_with_default("iLink bot token", current_token)?
            };

            if bot_token.trim().is_empty() {
                io.write_line("  No bot token provided, skipping WeChat.")?;
                return Ok(());
            }

            // Preserve existing fields
            let (
                app_id,
                app_secret,
                old_token,
                encoding_aes_key,
                base_url,
                cdn_base_url,
                dm_policy,
                group_policy,
                allowed_users,
                group_allowed_users,
            ) = config
                .channels
                .weixin
                .as_ref()
                .map(|w| {
                    (
                        w.app_id.clone(),
                        w.app_secret.clone(),
                        w.token.clone(),
                        w.encoding_aes_key.clone(),
                        w.base_url.clone(),
                        w.cdn_base_url.clone(),
                        w.dm_policy.clone(),
                        w.group_policy.clone(),
                        w.allowed_users.clone(),
                        w.group_allowed_users.clone(),
                    )
                })
                .unwrap_or_default();

            config.channels.weixin = Some(WeixinConfig {
                account_id: Some(account_id.trim().to_string()),
                bot_token: Some(bot_token.trim().to_string()),
                app_id,
                app_secret,
                token: old_token,
                encoding_aes_key,
                base_url,
                cdn_base_url,
                dm_policy,
                group_policy,
                allowed_users,
                group_allowed_users,
                enabled: true,
            });

            io.write_line(&format!("  {} WeChat credentials saved.", "✓".green()))?;
        }
        _ => {
            io.write_line("  Skipped.")?;
        }
    }

    Ok(())
}

// ── Provider field helpers (using macro) ───────────────────────

fn get_provider_entry_mut<'a>(
    config: &'a mut Config,
    provider: &str,
) -> Option<&'a mut ProviderEntry> {
    match provider {
        "anthropic" => config.providers.anthropic.as_mut(),
        "openai" => config.providers.openai.as_mut(),
        "openrouter" => config.providers.openrouter.as_mut(),
        "ollama" => config.providers.ollama.as_mut(),
        "deepseek" => config.providers.deepseek.as_mut(),
        "gemini" => config.providers.gemini.as_mut(),
        "groq" => config.providers.groq.as_mut(),
        "moonshot" => config.providers.moonshot.as_mut(),
        "minimax" => config.providers.minimax.as_mut(),
        "github_copilot" => config.providers.github_copilot.as_mut(),
        "openai_codex" => config.providers.openai_codex.as_mut(),
        "opencode_go" => config.providers.opencode_go.as_mut(),
        "glm_coding_plan" => config.providers.glm_coding_plan.as_mut(),
        _ => None,
    }
}

fn ensure_provider_entry(config: &mut Config, provider: &str) {
    match provider {
        "anthropic" => {
            config
                .providers
                .anthropic
                .get_or_insert_with(ProviderEntry::default);
        }
        "openai" => {
            config
                .providers
                .openai
                .get_or_insert_with(ProviderEntry::default);
        }
        "openrouter" => {
            config
                .providers
                .openrouter
                .get_or_insert_with(ProviderEntry::default);
        }
        "ollama" => {
            config
                .providers
                .ollama
                .get_or_insert_with(ProviderEntry::default);
        }
        "deepseek" => {
            config
                .providers
                .deepseek
                .get_or_insert_with(ProviderEntry::default);
        }
        "gemini" => {
            config
                .providers
                .gemini
                .get_or_insert_with(ProviderEntry::default);
        }
        "groq" => {
            config
                .providers
                .groq
                .get_or_insert_with(ProviderEntry::default);
        }
        "moonshot" => {
            config
                .providers
                .moonshot
                .get_or_insert_with(ProviderEntry::default);
        }
        "minimax" => {
            config
                .providers
                .minimax
                .get_or_insert_with(ProviderEntry::default);
        }
        "github_copilot" => {
            config
                .providers
                .github_copilot
                .get_or_insert_with(ProviderEntry::default);
        }
        "openai_codex" => {
            config
                .providers
                .openai_codex
                .get_or_insert_with(ProviderEntry::default);
        }
        "opencode_go" => {
            config
                .providers
                .opencode_go
                .get_or_insert_with(ProviderEntry::default);
        }
        "glm_coding_plan" => {
            config
                .providers
                .glm_coding_plan
                .get_or_insert_with(ProviderEntry::default);
        }
        _ => {}
    }
}

fn get_provider_url<'a>(config: &'a Config, provider: &str) -> Option<&'a str> {
    let entry = match provider {
        "anthropic" => config.providers.anthropic.as_ref(),
        "openai" => config.providers.openai.as_ref(),
        "openrouter" => config.providers.openrouter.as_ref(),
        "ollama" => config.providers.ollama.as_ref(),
        "deepseek" => config.providers.deepseek.as_ref(),
        "gemini" => config.providers.gemini.as_ref(),
        "groq" => config.providers.groq.as_ref(),
        "moonshot" => config.providers.moonshot.as_ref(),
        "minimax" => config.providers.minimax.as_ref(),
        "github_copilot" => config.providers.github_copilot.as_ref(),
        "openai_codex" => config.providers.openai_codex.as_ref(),
        "opencode_go" => config.providers.opencode_go.as_ref(),
        "glm_coding_plan" => config.providers.glm_coding_plan.as_ref(),
        _ => None,
    };
    entry.and_then(|e| e.base_url.as_deref())
}

fn set_provider_url(config: &mut Config, provider: &str, url: &str) {
    ensure_provider_entry(config, provider);
    if let Some(entry) = get_provider_entry_mut(config, provider) {
        entry.base_url = if url.is_empty() {
            None
        } else {
            Some(url.to_string())
        };
    }
}

fn set_provider_api_key(config: &mut Config, provider: &str, key: &str) {
    ensure_provider_entry(config, provider);
    if let Some(entry) = get_provider_entry_mut(config, provider) {
        entry.api_key = if key.is_empty() {
            None
        } else {
            Some(key.to_string())
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    fn template_toml() -> String {
        toml::to_string(&Config::default()).unwrap()
    }

    // ── Mock wizard IO for flow testing ─────────────────────────

    #[derive(Debug, Clone)]
    enum MockAction {
        Confirm {
            prompt_contains: &'static str,
            result: bool,
        },
        Select {
            result: usize,
        },
        Input {
            result: &'static str,
        },
        Password {
            result: &'static str,
        },
    }

    struct MockWizard {
        actions: RefCell<VecDeque<MockAction>>,
        output: RefCell<String>,
    }

    impl MockWizard {
        fn new(actions: Vec<MockAction>) -> Self {
            Self {
                actions: RefCell::new(actions.into()),
                output: RefCell::new(String::new()),
            }
        }

        fn output(&self) -> String {
            self.output.borrow().clone()
        }
    }

    impl WizardIo for MockWizard {
        fn confirm(&self, prompt: &str, _default: bool) -> Result<bool> {
            let action = self.actions.borrow_mut().pop_front();
            match action {
                Some(MockAction::Confirm {
                    prompt_contains,
                    result,
                }) => {
                    assert!(
                        prompt.contains(prompt_contains),
                        "confirm prompt '{}' did not contain '{}'",
                        prompt,
                        prompt_contains
                    );
                    Ok(result)
                }
                _ => panic!("unexpected confirm call, prompt: {}", prompt),
            }
        }

        fn select(&self, _prompt: &str, _items: &[&str], _default: usize) -> Result<usize> {
            let action = self.actions.borrow_mut().pop_front();
            match action {
                Some(MockAction::Select { result }) => Ok(result),
                _ => panic!("unexpected select call"),
            }
        }

        fn input_with_default(&self, _prompt: &str, _default: &str) -> Result<String> {
            let action = self.actions.borrow_mut().pop_front();
            match action {
                Some(MockAction::Input { result }) => Ok(result.to_string()),
                _ => panic!("unexpected input_with_default call"),
            }
        }

        fn input_allow_empty(&self, _prompt: &str) -> Result<String> {
            let action = self.actions.borrow_mut().pop_front();
            match action {
                Some(MockAction::Input { result }) => Ok(result.to_string()),
                _ => panic!("unexpected input_allow_empty call"),
            }
        }

        fn password(&self, _prompt: &str) -> Result<String> {
            let action = self.actions.borrow_mut().pop_front();
            match action {
                Some(MockAction::Password { result }) => Ok(result.to_string()),
                _ => panic!("unexpected password call"),
            }
        }

        fn write_line(&self, line: &str) -> Result<()> {
            self.output.borrow_mut().push_str(line);
            self.output.borrow_mut().push('\n');
            Ok(())
        }
    }

    // ── Unit tests ──────────────────────────────────────────────

    #[test]
    fn setup_creates_template_config_when_config_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        let config = Config::default();
        let home = config_path.parent().unwrap();
        std::fs::create_dir_all(home).unwrap();
        loader::save_config(&config, &config_path).unwrap();

        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            template_toml()
        );
        assert!(config_path.exists());
    }

    #[test]
    fn provider_helpers_set_and_get_fields() {
        let mut config = Config::default();
        set_provider_url(&mut config, "openai", "https://custom.api/v1");
        set_provider_api_key(&mut config, "openai", "sk-test-key");

        let entry = config.providers.openai.as_ref().unwrap();
        assert_eq!(entry.base_url.as_deref(), Some("https://custom.api/v1"));
        assert_eq!(entry.api_key.as_deref(), Some("sk-test-key"));
    }

    #[test]
    fn provider_helpers_handle_unknown_provider() {
        let mut config = Config::default();
        set_provider_url(&mut config, "nonexistent", "https://example.com");
        assert!(get_provider_url(&config, "nonexistent").is_none());
    }

    #[test]
    fn wizard_keeps_existing_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        let existing = Config::default();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        loader::save_config(&existing, &config_path).unwrap();

        let mock = MockWizard::new(vec![MockAction::Confirm {
            prompt_contains: "Update",
            result: false,
        }]);

        run_wizard(&mock, &config_path).unwrap();

        // Config should be unchanged
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            template_toml()
        );
    }

    #[test]
    fn wizard_fresh_setup() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        let mock = MockWizard::new(vec![
            // Step 2: Provider
            MockAction::Select { result: 1 }, // openai
            MockAction::Input { result: "gpt-4o" },
            MockAction::Input {
                result: "https://api.openai.com/v1",
            },
            MockAction::Password {
                result: "sk-test-key",
            },
            // Step 3: Telegram (skip)
            MockAction::Confirm {
                prompt_contains: "Telegram",
                result: false,
            },
            // Step 4: Feishu (skip)
            MockAction::Confirm {
                prompt_contains: "Feishu",
                result: false,
            },
            // Step 5: WebSocket (skip)
            MockAction::Confirm {
                prompt_contains: "WebSocket",
                result: false,
            },
            // Step 5: WeChat (skip)
            MockAction::Confirm {
                prompt_contains: "WeChat",
                result: false,
            },
            // Step 6: Save
            MockAction::Confirm {
                prompt_contains: "Write",
                result: true,
            },
        ]);

        run_wizard(&mock, &config_path).unwrap();

        assert!(config_path.exists());
        let saved: Config =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(saved.agent.provider.as_deref(), Some("openai"));
        assert_eq!(saved.agent.model, "gpt-4o");
    }

    #[test]
    fn wizard_overwrite_existing_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        let existing = Config::default();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        loader::save_config(&existing, &config_path).unwrap();

        let mock = MockWizard::new(vec![
            // Step 1: Overwrite
            MockAction::Confirm {
                prompt_contains: "Update",
                result: true,
            },
            // Step 2: Provider
            MockAction::Select { result: 0 }, // anthropic
            MockAction::Input {
                result: "claude-sonnet-4-20250514",
            },
            MockAction::Input {
                result: "https://api.anthropic.com",
            },
            MockAction::Password {
                result: "sk-ant-test",
            },
            // Step 3: Telegram (skip)
            MockAction::Confirm {
                prompt_contains: "Telegram",
                result: false,
            },
            // Step 4: Feishu (skip)
            MockAction::Confirm {
                prompt_contains: "Feishu",
                result: false,
            },
            // Step 5: WebSocket (skip)
            MockAction::Confirm {
                prompt_contains: "WebSocket",
                result: false,
            },
            // Step 5: WeChat (skip)
            MockAction::Confirm {
                prompt_contains: "WeChat",
                result: false,
            },
            // Step 6: Save
            MockAction::Confirm {
                prompt_contains: "Write",
                result: true,
            },
        ]);

        run_wizard(&mock, &config_path).unwrap();

        let saved: Config =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(saved.agent.provider.as_deref(), Some("anthropic"));
        assert_eq!(saved.agent.model, "claude-sonnet-4-20250514");
    }

    fn tg(token: &str) -> TelegramConfig {
        TelegramConfig {
            token: token.to_string(),
            allowed_users: Vec::new(),
            admin_users: Vec::new(),
            enabled: true,
            streaming: false,
            proxy: None,
        }
    }

    #[test]
    fn token_masking_short_tokens() {
        let mut config = Config::default();
        config.channels.telegram = Some(tg("ab"));

        let mock = MockWizard::new(vec![]);
        show_config_summary(&mock, &config).unwrap();

        let output = mock.output();
        assert!(output.contains("(masked)"));
        assert!(!output.contains("ab…"));
    }

    #[test]
    fn token_masking_empty_token() {
        let mut config = Config::default();
        config.channels.telegram = Some(tg(""));

        let mock = MockWizard::new(vec![]);
        show_config_summary(&mock, &config).unwrap();

        let output = mock.output();
        assert!(output.contains("(not set)"));
    }
}
