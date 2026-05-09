//! Setup command — interactive wizard for configuring Kestrel.
//!
//! Supports back-navigation via a state-machine loop, Quick/Full setup modes,
//! input validation, and enhanced configuration summaries.

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
use std::time::Duration;

// ── Provider display info ──────────────────────────────────────

struct ProviderInfo {
    key: &'static str,
    display: &'static str,
    default_model: &'static str,
    default_url: &'static str,
}

const PROVIDERS: &[ProviderInfo] = &[
    ProviderInfo {
        key: "openai",
        display: "★ OpenAI",
        default_model: "gpt-4o",
        default_url: "https://api.openai.com/v1",
    },
    ProviderInfo {
        key: "anthropic",
        display: "★ Anthropic",
        default_model: "claude-sonnet-4-20250514",
        default_url: "https://api.anthropic.com",
    },
    ProviderInfo {
        key: "openrouter",
        display: "★ OpenRouter (multi-model)",
        default_model: "anthropic/claude-sonnet-4-20250514",
        default_url: "https://openrouter.ai/api/v1",
    },
    ProviderInfo {
        key: "ollama",
        display: "  Ollama (local models)",
        default_model: "llama3",
        default_url: "http://localhost:11434",
    },
    ProviderInfo {
        key: "deepseek",
        display: "  DeepSeek",
        default_model: "deepseek-chat",
        default_url: "https://api.deepseek.com",
    },
    ProviderInfo {
        key: "gemini",
        display: "  Gemini",
        default_model: "gemini-2.5-pro",
        default_url: "https://generativelanguage.googleapis.com/v1beta",
    },
    ProviderInfo {
        key: "groq",
        display: "  Groq",
        default_model: "llama-3.3-70b-versatile",
        default_url: "https://api.groq.com/openai/v1",
    },
    ProviderInfo {
        key: "moonshot",
        display: "  Moonshot (月之暗面)",
        default_model: "moonshot-v1-8k",
        default_url: "https://api.moonshot.cn/v1",
    },
    ProviderInfo {
        key: "minimax",
        display: "  MiniMax",
        default_model: "MiniMax-Text-01",
        default_url: "https://api.minimax.chat/v1",
    },
    ProviderInfo {
        key: "github_copilot",
        display: "  GitHub Copilot",
        default_model: "gpt-4o",
        default_url: "https://api.githubcopilot.com",
    },
    ProviderInfo {
        key: "openai_codex",
        display: "  OpenAI Codex",
        default_model: "codex-mini",
        default_url: "https://api.openai.com/v1",
    },
    ProviderInfo {
        key: "glm_coding_plan",
        display: "  GLM Coding Plan (智谱)",
        default_model: "glm-5.1",
        default_url: "https://open.bigmodel.cn/api/coding/paas/v4",
    },
];

const TOTAL_CONFIG_STEPS: usize = 5;

const BACK_OPTION: &str = "↩ Go back";

// ── Wizard step state machine ──────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WizardStep {
    Provider,  // Step 1
    Telegram,  // Step 2
    Feishu,    // Step 3
    WebSocket, // Step 4
    WeChat,    // Step 5
    Review,    // Final (no step number)
}

impl WizardStep {
    fn step_number(self) -> usize {
        match self {
            Self::Provider => 1,
            Self::Telegram => 2,
            Self::Feishu => 3,
            Self::WebSocket => 4,
            Self::WeChat => 5,
            Self::Review => 0,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Provider => "Provider Configuration",
            Self::Telegram => "Telegram Channel",
            Self::Feishu => "Feishu / Lark Channel",
            Self::WebSocket => "WebSocket Port",
            Self::WeChat => "WeChat Channel",
            Self::Review => "Review & Save",
        }
    }

    fn all() -> &'static [WizardStep] {
        &[
            WizardStep::Provider,
            WizardStep::Telegram,
            WizardStep::Feishu,
            WizardStep::WebSocket,
            WizardStep::WeChat,
        ]
    }
}

/// Whether a channel step was configured or skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelStatus {
    Configured,
    Skipped,
}

// ── Trait for interactive I/O (enables testability) ────────────

trait WizardIo {
    fn confirm(&self, prompt: &str, default: bool) -> Result<bool>;
    fn select(&self, prompt: &str, items: &[&str], default: usize) -> Result<usize>;
    fn input_with_default(&self, prompt: &str, default: &str) -> Result<String>;
    fn input_allow_empty(&self, prompt: &str) -> Result<String>;
    fn password(&self, prompt: &str) -> Result<String>;
    fn write_line(&self, line: &str) -> Result<()>;

    /// Show a confirmation with back option. Returns `None` if user chose back.
    fn confirm_or_back(&self, prompt: &str, default: bool) -> Result<Option<bool>> {
        let items = if default {
            &["Yes", "Skip (keep current)", BACK_OPTION] as &[&str]
        } else {
            &["Skip (keep current)", "Yes", BACK_OPTION] as &[&str]
        };
        let default_idx = 0;
        let choice = self.select(prompt, items, default_idx)?;
        match choice {
            idx if items[idx] == BACK_OPTION => Ok(None),
            idx if items[idx] == "Yes" => Ok(Some(true)),
            _ => Ok(Some(false)),
        }
    }
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

    // Set up Ctrl+C handler for graceful exit
    ctrlc::set_handler(|| {
        println!();
        println!(
            "  {} Setup interrupted. Progress has not been saved.",
            "!".yellow()
        );
        println!("  Run `kestrel setup` again to start over.");
        std::process::exit(1);
    })
    .ok(); // Ignore error if handler already set

    run_wizard(&io, &config_path)
}

// ── Wizard flow (state machine) ────────────────────────────────

fn run_wizard(io: &dyn WizardIo, config_path: &Path) -> Result<()> {
    let is_first_run = !config_path.exists();
    print_banner(io, is_first_run)?;

    // ── Load existing config or start fresh ──────────────────────
    let mut config = if config_path.exists() {
        match load_existing_config(config_path) {
            Ok(existing) => {
                show_config_summary_simple(io, &existing)?;
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
        Config::default()
    };

    // ── State machine: Review-centric navigation ────────────────
    // After banner + config load, user lands on Review menu.
    // They can freely jump to any step, configure it, and return to Review.
    let mut channel_status = build_channel_status(&config);
    let mut current_step = WizardStep::Review;

    loop {
        match current_step {
            WizardStep::Provider => {
                print_step(io, current_step)?;
                match configure_provider(io, &mut config)? {
                    StepAction::Back => {}
                    StepAction::Continue => {
                        channel_status[0] = ChannelStatus::Configured;
                    }
                }
                current_step = WizardStep::Review;
            }
            WizardStep::Telegram => {
                print_step(io, current_step)?;
                let has_existing = config.channels.telegram.is_some();
                match configure_channel_step(
                    io,
                    &mut config,
                    "Telegram",
                    has_existing,
                    configure_telegram,
                    &mut channel_status[1],
                )? {
                    StepAction::Back => {}
                    StepAction::Continue => {}
                }
                current_step = WizardStep::Review;
            }
            WizardStep::Feishu => {
                print_step(io, current_step)?;
                let has_existing = config.channels.feishu.as_ref().is_some_and(|f| f.enabled);
                match configure_channel_step(
                    io,
                    &mut config,
                    "Feishu / Lark",
                    has_existing,
                    configure_feishu,
                    &mut channel_status[2],
                )? {
                    StepAction::Back => {}
                    StepAction::Continue => {}
                }
                current_step = WizardStep::Review;
            }
            WizardStep::WebSocket => {
                print_step(io, current_step)?;
                let has_existing = config
                    .channels
                    .websocket
                    .as_ref()
                    .is_some_and(|w| w.enabled);
                match configure_channel_step(
                    io,
                    &mut config,
                    "WebSocket",
                    has_existing,
                    configure_websocket,
                    &mut channel_status[3],
                )? {
                    StepAction::Back => {}
                    StepAction::Continue => {}
                }
                current_step = WizardStep::Review;
            }
            WizardStep::WeChat => {
                print_step(io, current_step)?;
                let has_existing = config.channels.weixin.is_some();
                match configure_channel_step(
                    io,
                    &mut config,
                    "WeChat",
                    has_existing,
                    configure_weixin,
                    &mut channel_status[4],
                )? {
                    StepAction::Back => {}
                    StepAction::Continue => {}
                }
                current_step = WizardStep::Review;
            }
            WizardStep::Review => {
                // Refresh status from actual config state
                channel_status = build_channel_status(&config);
                print_review(io, &config, &channel_status)?;

                let review_items = build_review_items(&channel_status);
                let review_refs: Vec<&str> = review_items.iter().map(|s| s.as_str()).collect();
                let default_save = review_items.len() - 1;
                let choice = io.select(
                    "Select a step to configure, or save",
                    &review_refs,
                    default_save,
                )?;

                if choice == default_save {
                    if save_config(io, &config, config_path)? {
                        print_next_steps(io, &config)?;
                        return Ok(());
                    }
                    // User cancelled save, stay in review
                } else if choice < WizardStep::all().len() {
                    current_step = WizardStep::all()[choice];
                }
            }
        }
    }
}

/// Build channel status from actual config state (not just wizard tracking).
fn build_channel_status(config: &Config) -> [ChannelStatus; 5] {
    [
        if config.agent.provider.is_some() {
            ChannelStatus::Configured
        } else {
            ChannelStatus::Skipped
        },
        if config.channels.telegram.is_some() {
            ChannelStatus::Configured
        } else {
            ChannelStatus::Skipped
        },
        if config.channels.feishu.as_ref().is_some_and(|f| f.enabled) {
            ChannelStatus::Configured
        } else {
            ChannelStatus::Skipped
        },
        if config
            .channels
            .websocket
            .as_ref()
            .is_some_and(|w| w.enabled)
        {
            ChannelStatus::Configured
        } else {
            ChannelStatus::Skipped
        },
        if config.channels.weixin.is_some() {
            ChannelStatus::Configured
        } else {
            ChannelStatus::Skipped
        },
    ]
}

fn build_review_items(channel_status: &[ChannelStatus; 5]) -> Vec<String> {
    let mut items: Vec<String> = WizardStep::all()
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let num = s.step_number();
            let title = s.title();
            let status = match channel_status[i] {
                ChannelStatus::Configured => "configured".to_string(),
                ChannelStatus::Skipped => "not configured".to_string(),
            };
            format!("{}. {} ({})", num, title, status)
        })
        .collect();
    items.push("Save configuration".to_string());
    items
}

/// Result of a configure step: continue forward or go back.
enum StepAction {
    Continue,
    Back,
}

/// Generic wrapper for channel configuration steps with back support.
fn configure_channel_step(
    io: &dyn WizardIo,
    config: &mut Config,
    channel_name: &str,
    has_existing: bool,
    configure_fn: fn(&dyn WizardIo, &mut Config) -> Result<bool>,
    status: &mut ChannelStatus,
) -> Result<StepAction> {
    let prompt = if has_existing {
        format!("Update {} configuration?", channel_name)
    } else {
        format!("Set up {}?", channel_name)
    };

    match io.confirm_or_back(&prompt, has_existing)? {
        None => return Ok(StepAction::Back),
        Some(false) => {
            io.write_line("  Kept current configuration.")?;
            *status = ChannelStatus::Skipped;
            return Ok(StepAction::Continue);
        }
        Some(true) => {}
    }

    match configure_fn(io, config) {
        Ok(true) => {
            *status = ChannelStatus::Configured;
            Ok(StepAction::Continue)
        }
        Ok(false) => {
            // Configure fn handled skip internally
            *status = ChannelStatus::Skipped;
            Ok(StepAction::Continue)
        }
        Err(e) => Err(e),
    }
}

// ── Banner & step printing ─────────────────────────────────────

fn print_banner(io: &dyn WizardIo, is_first_run: bool) -> Result<()> {
    io.write_line("")?;
    if is_first_run {
        io.write_line(&format!(
            "  {} {}",
            "▸".cyan(),
            "Kestrel 初始化配置向导".bold().cyan()
        ))?;
        io.write_line("")?;
        io.write_line("  Welcome to Kestrel! Let's set up your configuration.")?;
        io.write_line("  You can re-run `kestrel setup` anytime to make changes.")?;
    } else {
        io.write_line(&format!(
            "  {} {}",
            "▸".cyan(),
            "Kestrel 配置更新向导".bold().cyan()
        ))?;
        io.write_line("")?;
        io.write_line("  Update your existing configuration.")?;
    }
    io.write_line("")?;
    Ok(())
}

fn print_step(io: &dyn WizardIo, step: WizardStep) -> Result<()> {
    io.write_line("")?;
    let num = step.step_number();
    io.write_line(&format!(
        "  {} Step {}/{}: {}",
        "▸".cyan(),
        num,
        TOTAL_CONFIG_STEPS,
        step.title().bold()
    ))?;
    io.write_line(&format!("  {}", "─".repeat(40).dimmed()))?;
    Ok(())
}

fn print_review(
    io: &dyn WizardIo,
    config: &Config,
    channel_status: &[ChannelStatus; 5],
) -> Result<()> {
    io.write_line("")?;
    io.write_line(&format!(
        "  {} {}",
        "▸".cyan(),
        "Review & Save".bold().cyan()
    ))?;
    io.write_line(&format!("  {}", "─".repeat(50).dimmed()))?;
    io.write_line("")?;
    show_config_summary(io, config, channel_status)?;
    io.write_line("")?;
    Ok(())
}

fn load_existing_config(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let config: Config =
        toml::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(config)
}

// ── Token masking & config summary ─────────────────────────────

fn mask_token(token: &str) -> String {
    if token.is_empty() {
        "(not set)".to_string()
    } else if token.len() <= 8 {
        "(masked)".to_string()
    } else {
        format!("{}…{}", &token[..3], &token[token.len() - 4..])
    }
}

fn show_config_summary(
    io: &dyn WizardIo,
    config: &Config,
    channel_status: &[ChannelStatus; 5],
) -> Result<()> {
    let provider = config.agent.provider.as_deref().unwrap_or("default");
    io.write_line(&format!("  Provider:     {}", provider))?;
    io.write_line(&format!("  Model:        {}", config.agent.model))?;

    // Show base URL
    if let Some(url) = config
        .agent
        .provider
        .as_deref()
        .and_then(|p| get_provider_url(config, p))
    {
        io.write_line(&format!("  Base URL:     {}", url))?;
    }

    // Show API key status
    let key_status = config
        .agent
        .provider
        .as_deref()
        .and_then(|p| get_provider_api_key(config, p))
        .map(mask_token)
        .unwrap_or_else(|| "(not set)".to_string());
    io.write_line(&format!("  API Key:      {}", key_status))?;

    io.write_line(&format!("  Temperature:  {}", config.agent.temperature))?;
    io.write_line(&format!("  Max tokens:   {}", config.agent.max_tokens))?;

    io.write_line("")?;
    io.write_line("  Channels:")?;

    let channel_names = ["Telegram", "Feishu", "WebSocket", "WeChat"];
    for (i, name) in channel_names.iter().enumerate() {
        let status_str = match channel_status[i + 1] {
            ChannelStatus::Configured => "✓ configured".green().to_string(),
            ChannelStatus::Skipped => "✗ skipped".dimmed().to_string(),
        };
        io.write_line(&format!("    {}: {}", name, status_str))?;
    }

    // Show details for configured channels
    if let Some(ref tg) = config.channels.telegram {
        if !tg.token.is_empty() {
            io.write_line(&format!("      Token: {}", mask_token(&tg.token)))?;
        }
    }

    if let Some(ref ws) = config.channels.websocket {
        if ws.enabled {
            io.write_line(&format!("      Address: {}", ws.listen_addr))?;
        }
    }

    if let Some(ref wx) = config.channels.weixin {
        if wx.enabled {
            let acct = wx.account_id.as_deref().unwrap_or("(unknown)");
            io.write_line(&format!("      Account: {}", mask_token(acct)))?;
        }
    }

    if let Some(ref fs) = config.channels.feishu {
        if fs.enabled {
            let app_id = fs.app_id.as_deref().unwrap_or("(unknown)");
            io.write_line(&format!("      App ID: {}", mask_token(app_id)))?;
        }
    }

    Ok(())
}

/// Legacy summary without channel status (used for existing config review).
fn show_config_summary_simple(io: &dyn WizardIo, config: &Config) -> Result<()> {
    let provider = config.agent.provider.as_deref().unwrap_or("default");
    io.write_line(&format!("  Provider:     {}", provider))?;
    io.write_line(&format!("  Model:        {}", config.agent.model))?;
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

fn save_config(io: &dyn WizardIo, config: &Config, config_path: &Path) -> Result<bool> {
    io.write_line(&format!("  Config path: {}", config_path.display()))?;

    if !io.confirm("Write this configuration?", true)? {
        io.write_line(&format!("  {} Save cancelled.", "!".yellow()))?;
        return Ok(false);
    }

    let home = config_path
        .parent()
        .context("Config path must have a parent directory")?;

    std::fs::create_dir_all(home)
        .with_context(|| format!("Failed to create config home: {}", home.display()))?;

    loader::save_config(config, config_path)?;

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

    Ok(true)
}

fn print_next_steps(io: &dyn WizardIo, config: &Config) -> Result<()> {
    io.write_line("")?;
    io.write_line(&format!("  {} Next steps:", "▸".cyan()))?;
    io.write_line("")?;
    io.write_line("    1. Try it out:")?;
    io.write_line("       kestrel agent")?;
    io.write_line("")?;

    let has_channel = config.channels.telegram.is_some()
        || config.channels.feishu.is_some()
        || config.channels.weixin.is_some()
        || config.channels.websocket.is_some();

    if has_channel {
        io.write_line("    2. Start the gateway (connect to chat platforms):")?;
        io.write_line("       kestrel gateway")?;
        io.write_line("")?;
    }

    if config.channels.websocket.is_some() {
        io.write_line("    3. Start the API server:")?;
        io.write_line("       kestrel serve")?;
        io.write_line("")?;
    }

    io.write_line("    Check system health:")?;
    io.write_line("       kestrel doctor")?;
    io.write_line("")?;
    io.write_line("    Re-run setup anytime:")?;
    io.write_line("       kestrel setup")?;
    io.write_line("")?;
    Ok(())
}

// ── Input validation ───────────────────────────────────────────

fn validate_api_key(provider: &str, key: &str) -> Result<String> {
    let key = key.trim().to_string();
    if key.is_empty() {
        bail!("API key cannot be empty");
    }
    match provider {
        "openai" | "openai_codex" if !key.starts_with("sk-") => {
            bail!("OpenAI API keys typically start with 'sk-'");
        }
        "anthropic" if !key.starts_with("sk-ant-") => {
            bail!("Anthropic API keys typically start with 'sk-ant-'");
        }
        _ => {}
    }
    Ok(key)
}

fn validate_url(input: &str) -> Result<String> {
    let url = input.trim().to_string();
    if url.is_empty() {
        return Ok(url);
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        bail!("URL must start with http:// or https://");
    }
    Ok(url)
}

// ── Connectivity validation ────────────────────────────────────

/// Result of a connectivity check.
enum ConnectivityResult {
    /// Successfully connected and authenticated.
    Ok {
        latency_ms: u64,
        models: Vec<String>,
    },
    /// Connected to server but authentication failed (HTTP 401/403).
    AuthFailed(String),
    /// Server unreachable or other network error.
    Unreachable(String),
}

/// Test API key validity by making a real request to the provider.
///
/// Most OpenAI-compatible providers support `GET /models` with Bearer auth.
/// For Anthropic we use `GET /v1/models` with `x-api-key` header.
fn test_api_key_connectivity(
    provider_key: &str,
    base_url: &str,
    api_key: &str,
) -> ConnectivityResult {
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => return ConnectivityResult::Unreachable(format!("Runtime error: {}", e)),
    };

    rt.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build();

        let client = match client {
            Ok(c) => c,
            Err(e) => return ConnectivityResult::Unreachable(format!("Client error: {}", e)),
        };

        let start = std::time::Instant::now();

        // Build the request based on provider type
        let (url, req) = build_validation_request(provider_key, base_url, api_key, &client);

        match req {
            Ok(request) => match client.execute(request).await {
                Ok(resp) => {
                    let latency = start.elapsed().as_millis() as u64;
                    let status = resp.status();

                    if status.is_success() {
                        // Try to parse models from response
                        let models = parse_models_from_response(provider_key, resp).await;
                        ConnectivityResult::Ok {
                            latency_ms: latency,
                            models,
                        }
                    } else if status.as_u16() == 401 || status.as_u16() == 403 {
                        let body = resp.text().await.unwrap_or_default();
                        let msg = extract_error_message(&body)
                            .unwrap_or_else(|| format!("HTTP {} — API key may be invalid", status));
                        ConnectivityResult::AuthFailed(msg)
                    } else {
                        // Other HTTP errors — server reachable but unexpected response
                        ConnectivityResult::Ok {
                            latency_ms: latency,
                            models: vec![],
                        }
                    }
                }
                Err(e) => {
                    let msg = if e.is_timeout() {
                        "Connection timed out (10s)".to_string()
                    } else if e.is_connect() {
                        format!("Cannot connect to {}", url)
                    } else {
                        format!("{}", e)
                    };
                    ConnectivityResult::Unreachable(msg)
                }
            },
            Err(e) => ConnectivityResult::Unreachable(format!("Request error: {}", e)),
        }
    })
}

/// Build the validation HTTP request based on provider type.
fn build_validation_request(
    provider_key: &str,
    base_url: &str,
    api_key: &str,
    client: &reqwest::Client,
) -> (String, Result<reqwest::Request, reqwest::Error>) {
    let (url, req_builder) = match provider_key {
        "anthropic" => {
            let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
            let req = client
                .get(&url)
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01");
            (url, req.build())
        }
        "gemini" => {
            let url = format!("{}?key={}", base_url.trim_end_matches('/'), api_key);
            let req = client.get(&url);
            (url, req.build())
        }
        // OpenAI-compatible providers: openai, openrouter, deepseek, groq, moonshot, minimax,
        // github_copilot, openai_codex, glm_coding_plan, etc.
        _ => {
            let base = base_url.trim_end_matches('/');
            let url = format!("{}/models", base);
            let req = client
                .get(&url)
                .header("Authorization", format!("Bearer {}", api_key));
            (url, req.build())
        }
    };
    (url, req_builder)
}

/// Try to extract model names from the validation response.
async fn parse_models_from_response(provider_key: &str, resp: reqwest::Response) -> Vec<String> {
    let body = resp.text().await.unwrap_or_default();

    if provider_key == "gemini" {
        // Gemini returns { "models": [ { "name": "models/gemini-2.5-pro", ... } ] }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
            if let Some(models) = val.get("models").and_then(|m| m.as_array()) {
                return models
                    .iter()
                    .filter_map(|m| {
                        m.get("name")
                            .and_then(|n| n.as_str())
                            .map(|n| n.trim_start_matches("models/").to_string())
                    })
                    .take(5)
                    .collect();
            }
        }
        return vec![];
    }

    if provider_key == "anthropic" {
        // Anthropic /v1/models returns { "data": [ { "id": "claude-...", ... } ] }
        return parse_openai_models(&body);
    }

    // OpenAI-compatible: { "data": [ { "id": "gpt-4o", ... } ] }
    parse_openai_models(&body)
}

/// Parse model IDs from an OpenAI-style { "data": [ ... ] } response.
fn parse_openai_models(body: &str) -> Vec<String> {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(data) = val.get("data").and_then(|d| d.as_array()) {
            return data
                .iter()
                .filter_map(|m| m.get("id").and_then(|id| id.as_str()).map(String::from))
                .take(5)
                .collect();
        }
    }
    vec![]
}

/// Extract a human-readable error message from an error response body.
fn extract_error_message(body: &str) -> Option<String> {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
        // OpenAI-style: { "error": { "message": "..." } }
        if let Some(msg) = val
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return Some(msg.to_string());
        }
        // Anthropic-style: { "error": { "message": "..." } }
        if let Some(msg) = val
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return Some(msg.to_string());
        }
        // Fallback: { "message": "..." }
        if let Some(msg) = val.get("message").and_then(|m| m.as_str()) {
            return Some(msg.to_string());
        }
    }
    None
}

/// Run connectivity validation after provider config is entered.
/// Shows results to user and asks if they want to continue on failure.
/// Accepts a connectivity test fn (for testability).
fn run_connectivity_check(
    io: &dyn WizardIo,
    provider_key: &str,
    base_url: &str,
    api_key: &str,
    test_fn: fn(&str, &str, &str) -> ConnectivityResult,
) -> Result<()> {
    // Skip for Ollama (local, no API key needed)
    if provider_key == "ollama" {
        return Ok(());
    }

    io.write_line("")?;
    io.write_line(&format!("  {} Validating API key…", "⟳".cyan()))?;

    match test_fn(provider_key, base_url, api_key) {
        ConnectivityResult::Ok { latency_ms, models } => {
            if models.is_empty() {
                io.write_line(&format!(
                    "  {} Connected! (latency: {}ms)",
                    "✓".green(),
                    latency_ms
                ))?;
            } else {
                io.write_line(&format!(
                    "  {} Connected! (latency: {}ms)",
                    "✓".green(),
                    latency_ms
                ))?;
                let models_str = models.join(", ");
                io.write_line(&format!("    Available models: {}", models_str.dimmed()))?;
            }
        }
        ConnectivityResult::AuthFailed(msg) => {
            io.write_line(&format!(
                "  {} API key validation failed: {}",
                "✗".red(),
                msg
            ))?;
            if !io.confirm("  Continue with this key anyway?", false)? {
                bail!("Provider configuration cancelled.");
            }
        }
        ConnectivityResult::Unreachable(msg) => {
            io.write_line(&format!(
                "  {} Could not verify API key: {}",
                "⚠".yellow(),
                msg
            ))?;
            io.write_line(&format!(
                "    {}",
                "The server may be unreachable from this network.".dimmed()
            ))?;
            if !io.confirm("  Continue anyway?", true)? {
                bail!("Provider configuration cancelled.");
            }
        }
    }

    Ok(())
}

// ── Provider configuration ─────────────────────────────────────

fn configure_provider(io: &dyn WizardIo, config: &mut Config) -> Result<StepAction> {
    #[cfg(test)]
    let test_fn: fn(&str, &str, &str) -> ConnectivityResult = mock_connectivity_ok;
    #[cfg(not(test))]
    let test_fn: fn(&str, &str, &str) -> ConnectivityResult = test_api_key_connectivity;
    configure_provider_inner(io, config, test_fn)
}

fn configure_provider_inner(
    io: &dyn WizardIo,
    config: &mut Config,
    connectivity_fn: fn(&str, &str, &str) -> ConnectivityResult,
) -> Result<StepAction> {
    let display_names: Vec<&str> = PROVIDERS.iter().map(|p| p.display).collect();

    let default_idx = config
        .agent
        .provider
        .as_deref()
        .and_then(|p| PROVIDERS.iter().position(|info| info.key == p))
        .unwrap_or(0); // default to OpenAI (first)

    let provider_idx = io.select("Select LLM provider", &display_names, default_idx)?;

    let provider_key = PROVIDERS[provider_idx].key;
    config.agent.provider = Some(provider_key.to_string());

    let default_model = PROVIDERS[provider_idx].default_model;
    let current_model = if config.agent.model.is_empty() {
        default_model
    } else {
        &config.agent.model
    };

    let model: String = io.input_with_default("Model name", current_model)?;
    config.agent.model = model.trim().to_string();

    let default_url = PROVIDERS[provider_idx].default_url;
    let current_url = get_provider_url(config, provider_key).unwrap_or(default_url);

    if !current_url.is_empty() {
        let base_url: String = io.input_with_default("Base URL", current_url)?;
        let base_url = validate_url(&base_url)?;
        set_provider_url(config, provider_key, &base_url);
    } else {
        let base_url: String = io.input_allow_empty("Base URL (leave empty for default)")?;
        let base_url = validate_url(&base_url)?;
        if !base_url.is_empty() {
            set_provider_url(config, provider_key, &base_url);
        }
    }

    loop {
        let api_key: String = io.password("API key")?;
        match validate_api_key(provider_key, &api_key) {
            Ok(key) => {
                set_provider_api_key(config, provider_key, &key);
                break;
            }
            Err(e) => {
                io.write_line(&format!("  {} {}", "!".yellow(), e))?;
                if !io.confirm("Try again?", true)? {
                    // Accept whatever was entered
                    let key = api_key.trim().to_string();
                    if !key.is_empty() {
                        set_provider_api_key(config, provider_key, &key);
                    }
                    break;
                }
            }
        }
    }

    // Connectivity validation
    let base_url = get_provider_url(config, provider_key)
        .unwrap_or(PROVIDERS[provider_idx].default_url)
        .to_string();
    let api_key = get_provider_api_key(config, provider_key)
        .unwrap_or("")
        .to_string();
    if !api_key.is_empty() && !base_url.is_empty() {
        run_connectivity_check(io, provider_key, &base_url, &api_key, connectivity_fn)?;
    }

    Ok(StepAction::Continue)
}

// ── Channel configurations ─────────────────────────────────────

/// Configure Telegram. Returns true if configured, false if skipped.
fn configure_telegram(io: &dyn WizardIo, config: &mut Config) -> Result<bool> {
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

    let token = token.trim().to_string();
    if token.is_empty() {
        io.write_line("  No token provided, skipping Telegram.")?;
        return Ok(false);
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

    io.write_line(&format!("  {} Telegram configured.", "✓".green()))?;
    Ok(true)
}

/// Configure Feishu. Returns true if configured, false if skipped.
fn configure_feishu(io: &dyn WizardIo, config: &mut Config) -> Result<bool> {
    let domain_options = ["Feishu (飞书)", "Lark (international)"];
    let idx = io.select("Select platform", &domain_options, 0)?;
    let domain = if idx == 1 { "lark" } else { "feishu" };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("Failed to create tokio runtime")?;

    let result = match rt.block_on(super::feishu_onboarding::run_onboarding(domain)) {
        Ok(r) => r,
        Err(e) => {
            io.write_line(&format!("  {} Feishu setup skipped: {}", "!".yellow(), e))?;
            return Ok(false);
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
        ..Default::default()
    });

    io.write_line(&format!("  {} Feishu configured.", "✓".green()))?;
    Ok(true)
}

/// Configure WebSocket. Returns true if configured, false if skipped.
fn configure_websocket(io: &dyn WizardIo, config: &mut Config) -> Result<bool> {
    let default_addr = config
        .channels
        .websocket
        .as_ref()
        .map(|ws| ws.listen_addr.as_str())
        .unwrap_or("127.0.0.1:8090");

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

    io.write_line(&format!("  {} WebSocket configured.", "✓".green()))?;
    Ok(true)
}

/// Configure WeChat. Returns true if configured, false if skipped.
fn configure_weixin(io: &dyn WizardIo, config: &mut Config) -> Result<bool> {
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
            if let Some(ref mut wx) = config.channels.weixin {
                if wx.account_id.is_some() && wx.bot_token.is_some() {
                    wx.enabled = true;
                    io.write_line(&format!(
                        "  {} Existing WeChat credentials detected and enabled.",
                        "✓".green()
                    ))?;
                    return Ok(true);
                }
            }
            if config.channels.weixin.is_none() {
                io.write_line(
                    "  No WeChat credentials found yet. Run `kestrel setup weixin` first.",
                )?;
            }
            Ok(false)
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
                return Ok(false);
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
                return Ok(false);
            }

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

            io.write_line(&format!("  {} WeChat configured.", "✓".green()))?;
            Ok(true)
        }
        _ => {
            io.write_line("  Skipped.")?;
            Ok(false)
        }
    }
}

// ── Provider field helpers ─────────────────────────────────────

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

fn get_provider_api_key<'a>(config: &'a Config, provider: &str) -> Option<&'a str> {
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
    entry.and_then(|e| e.api_key.as_deref())
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

// ── Test helpers (cfg(test) only) ─────────────────────────────

#[cfg(test)]
fn mock_connectivity_ok(
    _provider_key: &str,
    _base_url: &str,
    _api_key: &str,
) -> ConnectivityResult {
    ConnectivityResult::Ok {
        latency_ms: 42,
        models: vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()],
    }
}

// ── Tests ──────────────────────────────────────────────────────

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

        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            template_toml()
        );
    }

    #[test]
    fn wizard_fresh_quick_setup() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        let mock = MockWizard::new(vec![
            // Review: select Provider (index 0)
            MockAction::Select { result: 0 },
            // Step 1: Provider
            MockAction::Select { result: 0 }, // openai (first)
            MockAction::Input { result: "gpt-4o" },
            MockAction::Input {
                result: "https://api.openai.com/v1",
            },
            MockAction::Password {
                result: "sk-test-key",
            },
            // Review: Save (index 5 = last)
            MockAction::Select { result: 5 },
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
    fn wizard_setup_only_feishu() {
        // Test: user only configures Feishu, skips everything else
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        let mock = MockWizard::new(vec![
            // Review: save directly without configuring anything
            MockAction::Select { result: 5 }, // Save
            MockAction::Confirm {
                prompt_contains: "Write",
                result: true,
            },
        ]);

        run_wizard(&mock, &config_path).unwrap();

        assert!(config_path.exists());
        // Default config should be saved
        let saved: Config =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(saved.agent.provider.is_none());
    }

    #[test]
    fn wizard_overwrite_existing_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        let existing = Config::default();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        loader::save_config(&existing, &config_path).unwrap();

        let mock = MockWizard::new(vec![
            // Update existing
            MockAction::Confirm {
                prompt_contains: "Update",
                result: true,
            },
            // Review: select Provider (index 0)
            MockAction::Select { result: 0 },
            // Step 1: Provider — select anthropic (index 1)
            MockAction::Select { result: 1 },
            MockAction::Input {
                result: "claude-sonnet-4-20250514",
            },
            MockAction::Input {
                result: "https://api.anthropic.com",
            },
            MockAction::Password {
                result: "sk-ant-test",
            },
            // Back to Review: Save (index 5)
            MockAction::Select { result: 5 },
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

    #[test]
    fn token_masking_short_tokens() {
        assert_eq!(mask_token("ab"), "(masked)");
        assert_eq!(mask_token(""), "(not set)");
    }

    #[test]
    fn token_masking_long_token() {
        let masked = mask_token("sk-proj-abcdefghijklmnop-b2cF");
        assert!(masked.starts_with("sk-"));
        assert!(masked.ends_with("b2cF"));
        assert!(masked.contains("…"));
    }

    #[test]
    fn validate_api_key_openai() {
        assert!(validate_api_key("openai", "sk-test123").is_ok());
        assert!(validate_api_key("openai", "bad-key").is_err());
    }

    #[test]
    fn validate_api_key_anthropic() {
        assert!(validate_api_key("anthropic", "sk-ant-test123").is_ok());
        assert!(validate_api_key("anthropic", "bad-key").is_err());
    }

    #[test]
    fn validate_api_key_other_providers() {
        assert!(validate_api_key("ollama", "any-key").is_ok());
    }

    #[test]
    fn validate_api_key_empty() {
        assert!(validate_api_key("openai", "").is_err());
    }

    #[test]
    fn validate_url_valid() {
        assert_eq!(
            validate_url("https://api.openai.com/v1").unwrap(),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            validate_url("http://localhost:11434").unwrap(),
            "http://localhost:11434"
        );
    }

    #[test]
    fn validate_url_empty() {
        assert_eq!(validate_url("").unwrap(), "");
    }

    #[test]
    fn validate_url_invalid() {
        assert!(validate_url("not-a-url").is_err());
        assert!(validate_url("ftp://example.com").is_err());
    }

    #[test]
    fn wizard_step_ordering() {
        assert_eq!(WizardStep::Provider.step_number(), 1);
        assert_eq!(WizardStep::Review.step_number(), 0);
        assert_eq!(WizardStep::all().len(), 5);
        assert_eq!(WizardStep::all()[0], WizardStep::Provider);
        assert_eq!(WizardStep::all()[4], WizardStep::WeChat);
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
    fn token_masking_empty_token() {
        let mut config = Config::default();
        config.channels.telegram = Some(tg(""));

        let mock = MockWizard::new(vec![]);
        show_config_summary_simple(&mock, &config).unwrap();

        let output = mock.output();
        assert!(output.contains("(not set)"));
    }

    #[test]
    fn configure_weixin_qr_enables_existing_credentials() {
        let mock = MockWizard::new(vec![MockAction::Select { result: 0 }]);
        let mut config = Config::default();
        config.channels.weixin = Some(WeixinConfig {
            app_id: None,
            app_secret: None,
            token: None,
            encoding_aes_key: None,
            account_id: Some("wxid_existing@im.bot".to_string()),
            bot_token: Some("tok_existing".to_string()),
            base_url: None,
            cdn_base_url: None,
            dm_policy: "open".to_string(),
            group_policy: "disabled".to_string(),
            allowed_users: vec![],
            group_allowed_users: vec![],
            enabled: false,
        });

        configure_weixin(&mock, &mut config).unwrap();

        let weixin = config.channels.weixin.unwrap();
        assert!(weixin.enabled);
        assert_eq!(weixin.account_id.as_deref(), Some("wxid_existing@im.bot"));
        assert_eq!(weixin.bot_token.as_deref(), Some("tok_existing"));
        assert!(mock.output().contains("detected and enabled"));
    }

    #[test]
    fn configure_weixin_manual_preserves_existing_fields() {
        let mock = MockWizard::new(vec![
            MockAction::Select { result: 1 },
            MockAction::Input {
                result: "wxid_new@im.bot",
            },
            MockAction::Input { result: "tok_new" },
        ]);
        let mut config = Config::default();
        config.channels.weixin = Some(WeixinConfig {
            app_id: Some("app_existing".to_string()),
            app_secret: Some("secret_existing".to_string()),
            token: Some("verify_token".to_string()),
            encoding_aes_key: Some("aes_key".to_string()),
            account_id: Some("wxid_old@im.bot".to_string()),
            bot_token: Some("tok_old".to_string()),
            base_url: Some("https://ilinkai.weixin.qq.com".to_string()),
            cdn_base_url: Some("https://novac2c.cdn.weixin.qq.com/c2c".to_string()),
            dm_policy: "allowlist".to_string(),
            group_policy: "open".to_string(),
            allowed_users: vec!["u1".to_string()],
            group_allowed_users: vec!["g1".to_string()],
            enabled: false,
        });

        configure_weixin(&mock, &mut config).unwrap();

        let weixin = config.channels.weixin.unwrap();
        assert_eq!(weixin.account_id.as_deref(), Some("wxid_new@im.bot"));
        assert_eq!(weixin.bot_token.as_deref(), Some("tok_new"));
        assert_eq!(weixin.app_id.as_deref(), Some("app_existing"));
        assert_eq!(weixin.app_secret.as_deref(), Some("secret_existing"));
        assert_eq!(weixin.token.as_deref(), Some("verify_token"));
        assert_eq!(weixin.encoding_aes_key.as_deref(), Some("aes_key"));
        assert_eq!(
            weixin.base_url.as_deref(),
            Some("https://ilinkai.weixin.qq.com")
        );
        assert_eq!(
            weixin.cdn_base_url.as_deref(),
            Some("https://novac2c.cdn.weixin.qq.com/c2c")
        );
        assert_eq!(weixin.dm_policy, "allowlist");
        assert_eq!(weixin.group_policy, "open");
        assert_eq!(weixin.allowed_users, vec!["u1"]);
        assert_eq!(weixin.group_allowed_users, vec!["g1"]);
        assert!(weixin.enabled);
    }

    // ── Connectivity validation tests ──────────────────────────

    fn mock_connectivity_auth_fail(
        _provider_key: &str,
        _base_url: &str,
        _api_key: &str,
    ) -> ConnectivityResult {
        ConnectivityResult::AuthFailed("HTTP 401 Unauthorized".to_string())
    }

    fn mock_connectivity_unreachable(
        _provider_key: &str,
        _base_url: &str,
        _api_key: &str,
    ) -> ConnectivityResult {
        ConnectivityResult::Unreachable("Connection timed out".to_string())
    }

    #[test]
    fn connectivity_check_ok_shows_models() {
        let mock = MockWizard::new(vec![]);
        let result = run_connectivity_check(
            &mock,
            "openai",
            "https://api.openai.com/v1",
            "sk-test-key",
            mock_connectivity_ok,
        );
        assert!(result.is_ok());
        let output = mock.output();
        assert!(output.contains("Connected!"));
        assert!(output.contains("gpt-4o"));
    }

    #[test]
    fn connectivity_check_auth_fail_asks_continue() {
        let mock = MockWizard::new(vec![MockAction::Confirm {
            prompt_contains: "Continue",
            result: true,
        }]);
        let result = run_connectivity_check(
            &mock,
            "openai",
            "https://api.openai.com/v1",
            "bad-key",
            mock_connectivity_auth_fail,
        );
        assert!(result.is_ok());
        let output = mock.output();
        assert!(output.contains("validation failed"));
    }

    #[test]
    fn connectivity_check_auth_fail_user_cancels() {
        let mock = MockWizard::new(vec![MockAction::Confirm {
            prompt_contains: "Continue",
            result: false,
        }]);
        let result = run_connectivity_check(
            &mock,
            "openai",
            "https://api.openai.com/v1",
            "bad-key",
            mock_connectivity_auth_fail,
        );
        assert!(result.is_err());
    }

    #[test]
    fn connectivity_check_unreachable_asks_continue() {
        let mock = MockWizard::new(vec![MockAction::Confirm {
            prompt_contains: "Continue",
            result: true,
        }]);
        let result = run_connectivity_check(
            &mock,
            "openai",
            "https://api.openai.com/v1",
            "sk-test",
            mock_connectivity_unreachable,
        );
        assert!(result.is_ok());
        let output = mock.output();
        assert!(output.contains("Could not verify"));
    }

    #[test]
    fn connectivity_check_skips_ollama() {
        let mock = MockWizard::new(vec![]);
        let result = run_connectivity_check(
            &mock,
            "ollama",
            "http://localhost:11434",
            "",
            mock_connectivity_ok,
        );
        assert!(result.is_ok());
        // No output for Ollama (skipped)
        assert!(mock.output().is_empty());
    }

    #[test]
    fn parse_models_from_openai_response() {
        let body = r#"{"data":[{"id":"gpt-4o"},{"id":"gpt-4o-mini"},{"id":"gpt-3.5-turbo"}]}"#;
        let models = parse_openai_models(body);
        assert_eq!(models, vec!["gpt-4o", "gpt-4o-mini", "gpt-3.5-turbo"]);
    }

    #[test]
    fn parse_models_empty_response() {
        let models = parse_openai_models("not json");
        assert!(models.is_empty());
    }

    #[test]
    fn extract_error_message_from_openai_format() {
        let body = r#"{"error":{"message":"Incorrect API key","type":"invalid_request_error"}}"#;
        let msg = extract_error_message(body).unwrap();
        assert_eq!(msg, "Incorrect API key");
    }

    #[test]
    fn extract_error_message_no_message() {
        let body = r#"{"status":"ok"}"#;
        assert!(extract_error_message(body).is_none());
    }

    #[test]
    fn configure_provider_with_connectivity() {
        let mock = MockWizard::new(vec![
            MockAction::Select { result: 0 }, // openai
            MockAction::Input { result: "gpt-4o" },
            MockAction::Input {
                result: "https://api.openai.com/v1",
            },
            MockAction::Password {
                result: "sk-testkey123456",
            },
        ]);

        let mut config = Config::default();
        configure_provider_inner(&mock, &mut config, mock_connectivity_ok).unwrap();

        assert_eq!(config.agent.provider.as_deref(), Some("openai"));
        assert_eq!(config.agent.model, "gpt-4o");
        let output = mock.output();
        assert!(output.contains("Connected!"));
    }
}
