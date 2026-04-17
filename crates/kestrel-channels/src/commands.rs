//! Built-in slash command handlers.
//!
//! Provides channel-level command handling that runs *before* messages are
//! forwarded to the message bus.  This means commands like `/validate` and
//! `/status` work even when the LLM provider is misconfigured.

use kestrel_config::validate::ValidationFinding;
use kestrel_config::{load_config, validate, Config};
use kestrel_session::SessionManager;
use kestrel_skill::{Skill, SkillRegistry};
use std::fmt::Write;
use std::sync::{Arc, LazyLock};

use parking_lot::RwLock;

use crate::platforms::telegram::{InlineKeyboardBuilder, InlineKeyboardMarkup};

/// Model names available for cycling via /settings.
const MODEL_CYCLE: &[&str] = &["gpt-4o", "claude-sonnet-4-6", "deepseek-chat"];

static SKILL_REGISTRY: LazyLock<RwLock<Option<Arc<SkillRegistry>>>> =
    LazyLock::new(|| RwLock::new(None));

/// Configure the shared skill registry used by built-in `/skill` commands.
pub fn set_skill_registry(registry: Option<Arc<SkillRegistry>>) {
    *SKILL_REGISTRY.write() = registry;
}

fn configured_skill_registry() -> Option<Arc<SkillRegistry>> {
    SKILL_REGISTRY.read().clone()
}

// ---------------------------------------------------------------------------
// Command response type
// ---------------------------------------------------------------------------

/// Result of a built-in command handler.
///
/// Commands that need an inline keyboard (e.g. `/settings`, `/history`) attach
/// one here.  Text-only commands set `keyboard: None`.
#[derive(Debug, Clone)]
pub struct CommandResponse {
    /// The text body of the response.
    pub text: String,
    /// Optional inline keyboard (Telegram only — ignored on Discord).
    pub keyboard: Option<InlineKeyboardMarkup>,
}

impl CommandResponse {
    /// Shorthand for a text-only response.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            keyboard: None,
        }
    }

    /// Shorthand for a response with a keyboard.
    pub fn with_keyboard(text: impl Into<String>, keyboard: InlineKeyboardMarkup) -> Self {
        Self {
            text: text.into(),
            keyboard: Some(keyboard),
        }
    }
}

/// Outcome of channel-level slash command dispatch.
#[derive(Debug, Clone)]
pub enum CommandDispatch {
    /// Reply directly and do not forward to the agent loop.
    Respond(CommandResponse),
    /// Rewrite the slash command into a normal user message and forward it.
    Rewrite(String),
}

// ---------------------------------------------------------------------------
// Command matching
// ---------------------------------------------------------------------------

/// Check whether `text` matches a slash `command`.
///
/// Handles:
/// - Exact match: `/validate`
/// - Case-insensitive: `/VALIDATE`, `/Validate`
/// - Telegram group mentions: `/validate@MyBot`
/// - Trailing arguments: `/validate --verbose`
pub fn matches_command(text: &str, command: &str) -> bool {
    let text = text.trim();
    if !text.starts_with('/') {
        return false;
    }
    let rest = &text[1..];
    // Strip "@botname" suffix (Telegram groups).
    let cmd_part = rest.split('@').next().unwrap_or(rest);
    // Take only the first word (ignore trailing args).
    let cmd_word = cmd_part.split_whitespace().next().unwrap_or(cmd_part);
    cmd_word.eq_ignore_ascii_case(command)
}

fn command_name(text: &str) -> Option<&str> {
    let text = text.trim();
    if !text.starts_with('/') {
        return None;
    }

    let rest = &text[1..];
    let cmd_part = rest.split('@').next().unwrap_or(rest);
    let cmd_word = cmd_part
        .split_whitespace()
        .next()
        .unwrap_or(cmd_part)
        .trim();
    if cmd_word.is_empty() {
        None
    } else {
        Some(cmd_word)
    }
}

fn is_reserved_command(command: &str) -> bool {
    matches!(
        command.to_ascii_lowercase().as_str(),
        "help" | "status" | "validate" | "skill" | "settings" | "history" | "reset" | "menu"
    )
}

fn normalize_skill_command_key(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut last_was_dash = false;

    for ch in name.chars().flat_map(char::to_lowercase) {
        let mapped = match ch {
            'a'..='z' | '0'..='9' => Some(ch),
            '-' | '_' | ' ' => Some('-'),
            _ => None,
        };

        match mapped {
            Some('-') if !last_was_dash && !normalized.is_empty() => {
                normalized.push('-');
                last_was_dash = true;
            }
            Some('-') => {}
            Some(value) => {
                normalized.push(value);
                last_was_dash = false;
            }
            None => {}
        }
    }

    normalized.trim_matches('-').to_string()
}

async fn resolve_skill_command(
    registry: &SkillRegistry,
    command: &str,
) -> Option<(String, String)> {
    let requested = normalize_skill_command_key(command);
    if requested.is_empty() {
        return None;
    }

    let mut names = registry.skill_names().await;
    names.sort();
    for name in names {
        let Some(skill) = registry.get(&name).await else {
            continue;
        };
        let guard = skill.read();
        if guard.is_deprecated() {
            continue;
        }

        if normalize_skill_command_key(guard.name()) == requested {
            return Some((guard.name().to_string(), requested));
        }
    }

    None
}

/// Return dynamic `/<skill-name>` commands discovered from the skill registry.
pub async fn dynamic_skill_commands() -> Vec<(String, String)> {
    let Some(registry) = configured_skill_registry() else {
        return Vec::new();
    };

    let mut names = registry.skill_names().await;
    names.sort();

    let mut commands = Vec::new();
    for name in names {
        let Some(skill) = registry.get(&name).await else {
            continue;
        };
        let guard = skill.read();
        if guard.is_deprecated() {
            continue;
        }

        let command = normalize_skill_command_key(guard.name());
        if command.is_empty() || is_reserved_command(&command) {
            continue;
        }

        commands.push((command, guard.description().to_string()));
    }

    commands
}

/// Try to handle a built-in command.
///
/// If `text` matches a known built-in command, returns `Some(response)`.
/// Otherwise returns `None`, signalling the caller to forward the message
/// through the normal bus path.
pub async fn try_handle_command(text: &str) -> Option<CommandDispatch> {
    if matches_command(text, "help") {
        Some(CommandDispatch::Respond(CommandResponse::text(
            handle_help(),
        )))
    } else if matches_command(text, "status") {
        Some(CommandDispatch::Respond(CommandResponse::text(
            handle_status(),
        )))
    } else if matches_command(text, "validate") {
        Some(CommandDispatch::Respond(CommandResponse::text(
            handle_validate(),
        )))
    } else if matches_command(text, "menu") {
        Some(CommandDispatch::Respond(handle_menu()))
    } else if matches_command(text, "settings") {
        Some(CommandDispatch::Respond(handle_settings()))
    } else if matches_command(text, "history") {
        Some(CommandDispatch::Respond(handle_history_page(0)))
    } else if matches_command(text, "skill") {
        Some(CommandDispatch::Respond(handle_skill_command(text).await))
    } else if let (Some(registry), Some(name)) = (configured_skill_registry(), command_name(text)) {
        if let Some((skill_name, invoked_as)) = resolve_skill_command(&registry, name).await {
            let user_input = command_arguments(text);
            let rewritten =
                build_skill_invocation_message(&registry, &skill_name, &invoked_as, user_input)
                    .await;
            Some(CommandDispatch::Rewrite(rewritten))
        } else {
            None
        }
    } else {
        None
    }
}

fn command_arguments(text: &str) -> &str {
    let text = text.trim();
    if !text.starts_with('/') {
        return "";
    }

    let rest = &text[1..];
    match rest.find(char::is_whitespace) {
        Some(index) => rest[index..].trim(),
        None => "",
    }
}

async fn handle_skill_command(text: &str) -> CommandResponse {
    let Some(registry) = configured_skill_registry() else {
        return CommandResponse::text("Skill registry is not available.");
    };

    let args = command_arguments(text);
    if args.is_empty() || args.eq_ignore_ascii_case("list") {
        return CommandResponse::text(handle_skill_list(&registry).await);
    }

    let mut parts = args.splitn(2, char::is_whitespace);
    let subcommand = parts.next().unwrap_or_default();
    let remainder = parts.next().unwrap_or("").trim();

    match subcommand.to_ascii_lowercase().as_str() {
        "list" if remainder.is_empty() => CommandResponse::text(handle_skill_list(&registry).await),
        "view" if !remainder.is_empty() => {
            CommandResponse::text(handle_skill_view(&registry, remainder).await)
        }
        "search" if !remainder.is_empty() => {
            CommandResponse::text(handle_skill_search(&registry, remainder).await)
        }
        _ => CommandResponse::text(
            "Usage:\n/skill\n/skill list\n/skill view <name>\n/skill search <query>",
        ),
    }
}

async fn handle_skill_list(registry: &SkillRegistry) -> String {
    let mut names = registry.skill_names().await;
    names.sort();

    if names.is_empty() {
        return "No skills are registered.".to_string();
    }

    let mut out = String::new();
    let _ = writeln!(out, "Registered skills:\n");

    for name in names {
        let Some(skill) = registry.get(&name).await else {
            continue;
        };
        let guard = skill.read();
        let deprecated = if guard.is_deprecated() {
            " [deprecated]"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "- {}{} — {} (confidence {:.2})",
            guard.name(),
            deprecated,
            guard.description(),
            guard.confidence()
        );
    }

    out.trim_end().to_string()
}

async fn handle_skill_view(registry: &SkillRegistry, name: &str) -> String {
    let Some(skill) = registry.get(name).await else {
        return format!("Skill '{name}' not found.");
    };

    let guard = skill.read();
    let manifest = match toml::to_string_pretty(guard.manifest()) {
        Ok(manifest) => manifest,
        Err(error) => format!("failed to render manifest: {error}"),
    };
    let instructions = if guard.instructions().trim().is_empty() {
        "(none)"
    } else {
        guard.instructions()
    };

    format!(
        "Skill: {}\n\nManifest:\n{}\nInstructions:\n{}",
        guard.name(),
        manifest.trim_end(),
        instructions
    )
}

async fn handle_skill_search(registry: &SkillRegistry, query: &str) -> String {
    let matches = registry.match_skills(query).await;
    if matches.is_empty() {
        return format!("No skills matched '{query}'.");
    }

    let mut out = String::new();
    let _ = writeln!(out, "Skill matches for '{query}':\n");

    for matched in matches {
        let Some(skill) = registry.get(&matched.name).await else {
            continue;
        };
        let guard = skill.read();
        let _ = writeln!(
            out,
            "- {} — {} (score {:.2}, confidence {:.2})",
            guard.name(),
            guard.description(),
            matched.score,
            guard.confidence()
        );
    }

    out.trim_end().to_string()
}

async fn build_skill_invocation_message(
    registry: &SkillRegistry,
    skill_name: &str,
    invoked_as: &str,
    user_input: &str,
) -> String {
    let Some(skill) = registry.get(skill_name).await else {
        return format!("Use the skill '{skill_name}'.");
    };

    let guard = skill.read();
    let manifest = match toml::to_string_pretty(guard.manifest()) {
        Ok(manifest) => manifest,
        Err(error) => format!("failed to render manifest: {error}"),
    };
    let instructions = if guard.instructions().trim().is_empty() {
        "(none)"
    } else {
        guard.instructions()
    };
    let supplemental = if user_input.trim().is_empty() {
        "No additional user instruction was provided after the slash command.".to_string()
    } else {
        format!("Additional user instruction:\n{}", user_input.trim())
    };

    format!(
        "[SYSTEM: The user explicitly invoked the /{invoked_as} skill. Treat the following skill contents as active context for this request.]\n\n\
Skill name: {skill_name}\n\n\
Manifest:\n{manifest}\n\n\
Instructions:\n{instructions}\n\n\
{supplemental}"
    )
}

// ---------------------------------------------------------------------------
// /help implementation
// ---------------------------------------------------------------------------

/// Return a formatted help text listing all available commands.
fn handle_help() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Available commands:\n");
    let _ = writeln!(out, "/help     - Show this help message");
    let _ = writeln!(
        out,
        "/status   - Show bot status, channels, and config summary"
    );
    let _ = writeln!(out, "/skill    - List, view, and search registered skills");
    let _ = writeln!(out, "/validate - Validate config.yaml and show results");
    let _ = writeln!(out, "/settings - Toggle preferences (notifications, model)");
    let _ = writeln!(out, "/history  - Browse recent conversation history");
    let _ = writeln!(out, "/reset    - Reset conversation context for this chat");
    let _ = writeln!(out, "/menu     - Show interactive menu");
    out
}

// ---------------------------------------------------------------------------
// /menu implementation
// ---------------------------------------------------------------------------

/// Build the main-menu inline keyboard.
///
/// Buttons use `menu:<action>` callback_data format so the
/// `CallbackRouter` can dispatch them.
pub fn menu_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardBuilder::new()
        .button("Status", "menu:status")
        .button("Help", "menu:help")
        .new_row()
        .button("Validate Config", "menu:validate")
        .button("Cancel", "menu:cancel")
        .build()
}

/// Handle the `/menu` command — returns a greeting with an inline keyboard.
fn handle_menu() -> CommandResponse {
    CommandResponse {
        text: "What would you like to do?".to_string(),
        keyboard: Some(menu_keyboard()),
    }
}

/// Handle a menu button callback.
///
/// Returns the text to display and whether to replace the keyboard.
pub fn handle_menu_callback(action: &str) -> (String, Option<InlineKeyboardMarkup>) {
    match action {
        "status" => {
            let config = load_config(None);
            let text = match config {
                Ok(c) => {
                    let mut out = String::new();
                    let name = c.name.as_deref().unwrap_or("unnamed");
                    let _ = writeln!(out, "Agent: {} | Name: {}", c.agent.model, name);
                    let _ = writeln!(out, "Streaming: {}", c.agent.streaming);
                    let _ = writeln!(out, "Max tokens: {}", c.agent.max_tokens);
                    let _ = writeln!(out, "Temperature: {}", c.agent.temperature);
                    out
                }
                Err(e) => format!("Failed to load config: {e}"),
            };
            (text, Some(menu_keyboard()))
        }
        "help" => (
            "Available commands:\n\
             /menu — Show this menu\n\
             /help — Show help\n\
             /skill — Browse loaded skills\n\
             /validate — Check configuration\n\
             /start — Start a conversation"
                .to_string(),
            Some(menu_keyboard()),
        ),
        "validate" => (handle_validate(), Some(menu_keyboard())),
        "cancel" => ("Menu closed.".to_string(), None),
        _ => (format!("Unknown action: {action}"), Some(menu_keyboard())),
    }
}

// ---------------------------------------------------------------------------
// /settings implementation (toggle version — from cc-feat)
// ---------------------------------------------------------------------------

/// Show user preferences as an inline keyboard for toggling.
///
/// Loads the current config and renders a keyboard with model switch and
/// streaming toggle buttons.  Button presses are handled by [`handle_callback`].
fn handle_settings() -> CommandResponse {
    let config = match load_config(None) {
        Ok(c) => c,
        Err(e) => return CommandResponse::text(format!("Failed to load config: {e}")),
    };
    build_settings_response(&config)
}

/// Build the settings CommandResponse with current config state.
fn build_settings_response(config: &Config) -> CommandResponse {
    let mut out = String::new();
    let _ = writeln!(out, "Settings");
    let _ = writeln!(out, "Model: {}", config.agent.model);
    let _ = writeln!(
        out,
        "Streaming: {}",
        if config.agent.streaming { "on" } else { "off" }
    );
    let _ = writeln!(out, "\nTap a button to change:");

    let keyboard = InlineKeyboardBuilder::new()
        .row_pair(
            "Model: switch",
            "settings:model:switch",
            "Streaming: toggle",
            "settings:streaming:toggle",
        )
        .build();

    CommandResponse::with_keyboard(out, keyboard)
}

/// Cycle the default model through the predefined list and persist to config.
fn handle_settings_model_switch() -> CommandResponse {
    let mut config = match load_config(None) {
        Ok(c) => c,
        Err(e) => return CommandResponse::text(format!("Failed to load config: {e}")),
    };

    // Find current model in the cycle list and advance.
    let current = config.agent.model.to_lowercase();
    let idx = MODEL_CYCLE
        .iter()
        .position(|m| m.eq_ignore_ascii_case(&current))
        .map(|i| (i + 1) % MODEL_CYCLE.len())
        .unwrap_or(0);
    config.agent.model = MODEL_CYCLE[idx].to_string();

    if let Err(e) = save_config_to_default(&config) {
        return CommandResponse::text(format!("Failed to save config: {e}"));
    }

    build_settings_response(&config)
}

/// Toggle the streaming setting and persist to config.
fn handle_settings_streaming_toggle() -> CommandResponse {
    let mut config = match load_config(None) {
        Ok(c) => c,
        Err(e) => return CommandResponse::text(format!("Failed to load config: {e}")),
    };

    config.agent.streaming = !config.agent.streaming;

    if let Err(e) = save_config_to_default(&config) {
        return CommandResponse::text(format!("Failed to save config: {e}"));
    }

    build_settings_response(&config)
}

/// Save config to the default path.
fn save_config_to_default(config: &Config) -> Result<(), String> {
    let path = kestrel_config::paths::get_config_path().map_err(|e| e.to_string())?;
    kestrel_config::loader::save_config(config, &path).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// /settings implementation (paginated view — from agent-a)
// ---------------------------------------------------------------------------

/// Number of settings items displayed per page.
pub const SETTINGS_PER_PAGE: usize = 5;

/// Collect config settings into a flat list of "key: value" strings.
fn collect_settings(config: &Config) -> Vec<String> {
    let mut settings = Vec::new();

    let name = config.name.as_deref().unwrap_or("(unnamed)");
    settings.push(format!("Name: {}", name));
    settings.push(format!("Model: {}", config.agent.model));
    settings.push(format!("Streaming: {}", config.agent.streaming));
    settings.push(format!("Max tokens: {}", config.agent.max_tokens));
    settings.push(format!("Temperature: {}", config.agent.temperature));

    // Providers
    let mut providers: Vec<&str> = Vec::new();
    if config.providers.anthropic.is_some() {
        providers.push("anthropic");
    }
    if config.providers.openai.is_some() {
        providers.push("openai");
    }
    if config.providers.openrouter.is_some() {
        providers.push("openrouter");
    }
    if config.providers.ollama.is_some() {
        providers.push("ollama");
    }
    if config.providers.deepseek.is_some() {
        providers.push("deepseek");
    }
    if config.providers.gemini.is_some() {
        providers.push("gemini");
    }
    if config.providers.groq.is_some() {
        providers.push("groq");
    }
    if config.providers.moonshot.is_some() {
        providers.push("moonshot");
    }
    if config.providers.minimax.is_some() {
        providers.push("minimax");
    }
    if config.providers.azure_openai.is_some() {
        providers.push("azure_openai");
    }
    if config.providers.github_copilot.is_some() {
        providers.push("github_copilot");
    }
    if config.providers.openai_codex.is_some() {
        providers.push("openai_codex");
    }
    for cp in &config.custom_providers {
        providers.push(&cp.name);
    }
    if providers.is_empty() {
        providers.push("(none)");
    }
    settings.push(format!("Providers: {}", providers.join(", ")));

    // Channels
    let mut channels: Vec<String> = Vec::new();
    if let Some(ref tg) = config.channels.telegram {
        let state = if tg.enabled { "enabled" } else { "disabled" };
        channels.push(format!("telegram ({})", state));
    }
    if let Some(ref dc) = config.channels.discord {
        let state = if dc.enabled { "enabled" } else { "disabled" };
        channels.push(format!("discord ({})", state));
    }
    if config.channels.slack.is_some() {
        channels.push("slack".to_string());
    }
    if config.channels.matrix.is_some() {
        channels.push("matrix".to_string());
    }
    if config.channels.whatsapp.is_some() {
        channels.push("whatsapp".to_string());
    }
    if config.channels.email.is_some() {
        channels.push("email".to_string());
    }
    if config.channels.dingtalk.is_some() {
        channels.push("dingtalk".to_string());
    }
    if config.channels.feishu.is_some() {
        channels.push("feishu".to_string());
    }
    if config.channels.wecom.is_some() {
        channels.push("wecom".to_string());
    }
    if config.channels.weixin.is_some() {
        channels.push("weixin".to_string());
    }
    if config.channels.qq.is_some() {
        channels.push("qq".to_string());
    }
    if config.channels.mochat.is_some() {
        channels.push("mochat".to_string());
    }
    if channels.is_empty() {
        channels.push("(none)".to_string());
    }
    settings.push(format!("Channels: {}", channels.join(", ")));

    settings
}

/// Build a paginated settings view from a loaded config.
///
/// Returns a `CommandResponse` with the current page's settings text and
/// a pagination keyboard when there are multiple pages.
pub fn handle_settings_paged(config: &Config, page: usize) -> CommandResponse {
    let settings = collect_settings(config);
    let total_pages = settings.len().div_ceil(SETTINGS_PER_PAGE);
    let total_pages = total_pages.max(1);
    let page = page.min(total_pages.saturating_sub(1));

    let start = page * SETTINGS_PER_PAGE;
    let end = (start + SETTINGS_PER_PAGE).min(settings.len());

    let mut text = format!("Settings (page {}/{}):\n\n", page + 1, total_pages);
    for s in &settings[start..end] {
        let _ = writeln!(text, "  {}", s);
    }

    let keyboard = if total_pages > 1 {
        Some(InlineKeyboardBuilder::pagination("settings_view", page, total_pages).build())
    } else {
        None
    };

    CommandResponse { text, keyboard }
}

/// Handle a `/settings` pagination callback (paginated view).
///
/// Loads the config from the default path and returns the paginated view.
pub fn handle_settings_callback(page: usize) -> CommandResponse {
    let config = match load_config(None) {
        Ok(c) => c,
        Err(e) => {
            return CommandResponse {
                text: format!("Failed to load config: {e}"),
                keyboard: None,
            }
        }
    };
    handle_settings_paged(&config, page)
}

// ---------------------------------------------------------------------------
// /history implementation (from cc-feat — reads from SessionManager)
// ---------------------------------------------------------------------------

/// Messages shown per page.
const HISTORY_PAGE_SIZE: usize = 5;

/// Render a page of recent conversation history.
///
/// `page` is zero-indexed.  Returns a `CommandResponse` with a pagination
/// keyboard when there are multiple pages.
///
/// If `data_dir` is `None`, uses `~/.kestrel/data` (derived from config).
pub fn handle_history_page(page: usize) -> CommandResponse {
    let home = match kestrel_config::paths::get_kestrel_home() {
        Ok(h) => h,
        Err(_) => return CommandResponse::text("Cannot determine data directory."),
    };
    let data_dir = home.join("data");
    handle_history_page_impl(page, &data_dir)
}

/// Implementation that accepts an explicit data directory (for testability).
fn handle_history_page_impl(page: usize, data_dir: &std::path::Path) -> CommandResponse {
    let mgr = match SessionManager::new(data_dir.to_path_buf()) {
        Ok(m) => m,
        Err(e) => return CommandResponse::text(format!("Session store error: {e}")),
    };

    // Discover session keys by scanning for .jsonl files in the sessions subdirectory.
    let sessions_dir = data_dir.join("sessions");
    let keys = discover_session_keys(&sessions_dir);
    if keys.is_empty() {
        return CommandResponse::text("No conversation history found.");
    }

    // Collect recent messages across all sessions (newest first).
    let mut all_entries: Vec<(String, kestrel_session::SessionEntry)> = Vec::new();
    for key in &keys {
        let session = mgr.get_or_create(key, None);
        for entry in &session.messages {
            all_entries.push((key.clone(), entry.clone()));
        }
    }

    // Sort by timestamp descending (newest first).
    all_entries.sort_by(|a, b| {
        let ta = a.1.timestamp.unwrap_or_default();
        let tb = b.1.timestamp.unwrap_or_default();
        tb.cmp(&ta)
    });

    let total = all_entries.len();
    let total_pages = total.max(1).div_ceil(HISTORY_PAGE_SIZE);
    let page = page.min(total_pages.saturating_sub(1));

    let start = page * HISTORY_PAGE_SIZE;
    let end = (start + HISTORY_PAGE_SIZE).min(total);
    let slice = &all_entries[start..end];

    let mut out = String::new();
    let _ = writeln!(out, "History (page {}/{})", page + 1, total_pages);

    for (key, entry) in slice {
        let role = match entry.role {
            kestrel_core::MessageRole::User => "You",
            kestrel_core::MessageRole::Assistant => "Bot",
            kestrel_core::MessageRole::System => "Sys",
            kestrel_core::MessageRole::Tool => "Tool",
        };
        let ts = entry
            .timestamp
            .map(|t| t.format("%H:%M").to_string())
            .unwrap_or_default();
        let preview = truncate_str(&entry.content, 60);
        let _ = writeln!(out, "[{}] {} {}: {}", ts, role, short_key(key), preview);
    }

    if total_pages > 1 {
        let keyboard = InlineKeyboardBuilder::pagination("history", page, total_pages).build();
        CommandResponse::with_keyboard(out, keyboard)
    } else {
        CommandResponse::text(out)
    }
}

/// Scan the data directory for `.jsonl` session files and return their keys.
fn discover_session_keys(data_dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(data_dir) else {
        return Vec::new();
    };
    let mut keys: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                path.file_stem().map(|s| s.to_string_lossy().to_string())
            } else {
                None
            }
        })
        .collect();
    keys.sort();
    keys
}

/// Shorten a session key for display (e.g. "telegram:123" → "tg:123").
fn short_key(key: &str) -> String {
    let mut parts = key.splitn(2, ':');
    let platform = parts.next().unwrap_or(key);
    let rest = parts.next().unwrap_or("");
    let short = match platform {
        "telegram" => "tg",
        "discord" => "dc",
        other => other,
    };
    if rest.is_empty() {
        short.to_string()
    } else {
        format!("{}:{}", short, rest)
    }
}

/// Truncate a string to `max` chars, appending "..." if truncated.
fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.ceil_char_boundary(max).min(s.len());
        format!("{}...", &s[..end])
    }
}

// ---------------------------------------------------------------------------
// /history implementation (from agent-a — session-key-based pagination)
// ---------------------------------------------------------------------------

/// Number of history sessions displayed per page.
pub const HISTORY_PER_PAGE: usize = 5;

/// Build a paginated session-history view from a list of session keys.
///
/// Returns a `CommandResponse` with the current page's session list and
/// a pagination keyboard when there are multiple pages.
pub fn handle_history(session_keys: &[String], page: usize) -> CommandResponse {
    if session_keys.is_empty() {
        return CommandResponse {
            text: "No active sessions.".to_string(),
            keyboard: None,
        };
    }

    let total_pages = session_keys.len().div_ceil(HISTORY_PER_PAGE);
    let total_pages = total_pages.max(1);
    let page = page.min(total_pages.saturating_sub(1));

    let start = page * HISTORY_PER_PAGE;
    let end = (start + HISTORY_PER_PAGE).min(session_keys.len());

    let mut text = format!("Sessions (page {}/{}):\n\n", page + 1, total_pages);
    for (i, key) in session_keys[start..end].iter().enumerate() {
        let _ = writeln!(text, "  {}. {}", start + i + 1, key);
    }

    let keyboard = if total_pages > 1 {
        Some(InlineKeyboardBuilder::pagination("history", page, total_pages).build())
    } else {
        None
    };

    CommandResponse { text, keyboard }
}

/// Handle a `/history` pagination callback with the provided session keys.
pub fn handle_history_callback(session_keys: &[String], page: usize) -> CommandResponse {
    handle_history(session_keys, page)
}

// ---------------------------------------------------------------------------
// /validate implementation
// ---------------------------------------------------------------------------

/// Load config from the default path, validate it, and return a
/// human-friendly multi-line string.
fn handle_validate() -> String {
    let config = match load_config(None) {
        Ok(c) => c,
        Err(e) => {
            return format!(
                "Failed to load configuration.\n\n\
                 Error: {e}\n\n\
                 Create one at ~/.kestrel/config.yaml or run `kestrel setup`."
            );
        }
    };

    let report = validate(&config);
    let mut out = String::new();

    // Header.
    if report.is_empty() {
        let _ = writeln!(out, "Configuration is valid. No issues found.");
    } else {
        let n_err = report.errors().len();
        let n_warn = report.warnings().len();
        let _ = writeln!(
            out,
            "Configuration has {} error(s) and {} warning(s).",
            n_err, n_warn
        );
    }

    // Errors section.
    let errors = report.errors();
    if !errors.is_empty() {
        let _ = writeln!(out, "\nErrors ({}):", errors.len());
        for e in &errors {
            let _ = writeln!(out, "  {}", format_finding(e));
        }
    }

    // Warnings section.
    let warnings = report.warnings();
    if !warnings.is_empty() {
        let _ = writeln!(out, "\nWarnings ({}):", warnings.len());
        for w in &warnings {
            let _ = writeln!(out, "  {}", format_finding(w));
        }
    }

    // Config summary.
    let _ = writeln!(out, "\n{}", build_summary(&config));

    out
}

/// Format a single finding as `[SEVERITY] path: message`.
fn format_finding(f: &ValidationFinding) -> String {
    format!("{}", f)
}

/// Build a short config summary block.
fn build_summary(config: &Config) -> String {
    let mut out = String::new();

    // Agent line.
    let name = config.name.as_deref().unwrap_or("unnamed");
    let _ = writeln!(out, "Agent: {} | Name: {}", config.agent.model, name);

    // Providers line.
    let mut providers: Vec<&str> = Vec::new();
    if config.providers.anthropic.is_some() {
        providers.push("anthropic");
    }
    if config.providers.openai.is_some() {
        providers.push("openai");
    }
    if config.providers.openrouter.is_some() {
        providers.push("openrouter");
    }
    if config.providers.ollama.is_some() {
        providers.push("ollama");
    }
    if config.providers.deepseek.is_some() {
        providers.push("deepseek");
    }
    if config.providers.gemini.is_some() {
        providers.push("gemini");
    }
    if config.providers.groq.is_some() {
        providers.push("groq");
    }
    if config.providers.moonshot.is_some() {
        providers.push("moonshot");
    }
    if config.providers.minimax.is_some() {
        providers.push("minimax");
    }
    if config.providers.azure_openai.is_some() {
        providers.push("azure_openai");
    }
    if config.providers.github_copilot.is_some() {
        providers.push("github_copilot");
    }
    if config.providers.openai_codex.is_some() {
        providers.push("openai_codex");
    }
    for cp in &config.custom_providers {
        providers.push(&cp.name);
    }
    if providers.is_empty() {
        providers.push("(none)");
    }
    let _ = writeln!(out, "Providers: {}", providers.join(", "));

    // Channels line.
    let mut channels: Vec<String> = Vec::new();
    if let Some(ref tg) = config.channels.telegram {
        let state = if tg.enabled { "enabled" } else { "disabled" };
        channels.push(format!("telegram ({})", state));
    }
    if let Some(ref dc) = config.channels.discord {
        let state = if dc.enabled { "enabled" } else { "disabled" };
        channels.push(format!("discord ({})", state));
    }
    if config.channels.slack.is_some() {
        channels.push("slack".to_string());
    }
    if config.channels.matrix.is_some() {
        channels.push("matrix".to_string());
    }
    if config.channels.whatsapp.is_some() {
        channels.push("whatsapp".to_string());
    }
    if config.channels.email.is_some() {
        channels.push("email".to_string());
    }
    if config.channels.dingtalk.is_some() {
        channels.push("dingtalk".to_string());
    }
    if config.channels.feishu.is_some() {
        channels.push("feishu".to_string());
    }
    if config.channels.wecom.is_some() {
        channels.push("wecom".to_string());
    }
    if config.channels.weixin.is_some() {
        channels.push("weixin".to_string());
    }
    if config.channels.qq.is_some() {
        channels.push("qq".to_string());
    }
    if config.channels.mochat.is_some() {
        channels.push("mochat".to_string());
    }
    if channels.is_empty() {
        channels.push("(none)".to_string());
    }
    let _ = writeln!(out, "Channels: {}", channels.join(", "));

    out
}

// ---------------------------------------------------------------------------
// /status implementation
// ---------------------------------------------------------------------------

/// Return a status snapshot: agent info, provider availability, connected
/// channels, and heartbeat info derived from config + environment.
fn handle_status() -> String {
    let config = match load_config(None) {
        Ok(c) => c,
        Err(e) => {
            return format!(
                "Status: config load failed\n\n\
                 Error: {e}\n\n\
                 The bot is receiving messages (you just sent one), but the \
                 config file could not be loaded. Try /validate for details."
            );
        }
    };

    let mut out = String::new();

    // Agent info.
    let name = config.name.as_deref().unwrap_or("unnamed");
    let _ = writeln!(out, "Agent: {} ({})", config.agent.model, name);

    // Channel status.
    let mut channels: Vec<String> = Vec::new();
    if let Some(ref tg) = config.channels.telegram {
        let state = if tg.enabled {
            if std::env::var("TELEGRAM_BOT_TOKEN").is_ok() {
                "connected"
            } else {
                "configured (no token)"
            }
        } else {
            "disabled"
        };
        channels.push(format!("telegram: {}", state));
    }
    if let Some(ref dc) = config.channels.discord {
        let state = if dc.enabled {
            if std::env::var("DISCORD_BOT_TOKEN").is_ok() {
                "connected"
            } else {
                "configured (no token)"
            }
        } else {
            "disabled"
        };
        channels.push(format!("discord: {}", state));
    }
    if channels.is_empty() {
        channels.push("(none)".to_string());
    }
    let _ = writeln!(out, "Channels: {}", channels.join(", "));

    // Provider availability.
    let mut providers: Vec<String> = Vec::new();
    if let Some(ref p) = config.providers.openai {
        providers.push(format_key_status("openai", p.api_key.as_deref()));
    }
    if let Some(ref p) = config.providers.anthropic {
        providers.push(format_key_status("anthropic", p.api_key.as_deref()));
    }
    if let Some(ref p) = config.providers.openrouter {
        providers.push(format_key_status("openrouter", p.api_key.as_deref()));
    }
    if let Some(ref p) = config.providers.ollama {
        providers.push(format_key_status("ollama", p.api_key.as_deref()));
    }
    if let Some(ref p) = config.providers.deepseek {
        providers.push(format_key_status("deepseek", p.api_key.as_deref()));
    }
    if let Some(ref p) = config.providers.gemini {
        providers.push(format_key_status("gemini", p.api_key.as_deref()));
    }
    if let Some(ref p) = config.providers.groq {
        providers.push(format_key_status("groq", p.api_key.as_deref()));
    }
    if let Some(ref p) = config.providers.moonshot {
        providers.push(format_key_status("moonshot", p.api_key.as_deref()));
    }
    if let Some(ref p) = config.providers.minimax {
        providers.push(format_key_status("minimax", p.api_key.as_deref()));
    }
    if config.providers.azure_openai.is_some() {
        providers.push("azure_openai".to_string());
    }
    if let Some(ref p) = config.providers.github_copilot {
        providers.push(format_key_status("github_copilot", p.api_key.as_deref()));
    }
    if let Some(ref p) = config.providers.openai_codex {
        providers.push(format_key_status("openai_codex", p.api_key.as_deref()));
    }
    for cp in &config.custom_providers {
        providers.push(format!("{} (custom)", cp.name));
    }
    if providers.is_empty() {
        providers.push("(none)".to_string());
    }
    let _ = writeln!(out, "Providers: {}", providers.join(", "));

    // Heartbeat.
    let hb = if config.heartbeat.enabled {
        format!("enabled (interval: {}s)", config.heartbeat.interval_secs)
    } else {
        "disabled".to_string()
    };
    let _ = writeln!(out, "Heartbeat: {}", hb);

    out
}

/// Format provider key status for display.
fn format_key_status(name: &str, key: Option<&str>) -> String {
    match key {
        Some(k) if !k.is_empty() && !k.starts_with("${") => format!("{}: ok", name),
        Some(k) if !k.is_empty() => format!("{}: key unresolved", name),
        _ => format!("{}: no key", name),
    }
}

// ---------------------------------------------------------------------------
// /reset implementation
// ---------------------------------------------------------------------------

/// Reset (clear) the conversation history for a session.
///
/// Deletes the persisted session file and notes, then returns a confirmation
/// message.  The session will be re-created with empty history on the next
/// message.
pub fn handle_reset(session_key: &str) -> String {
    let home = match kestrel_config::paths::get_kestrel_home() {
        Ok(h) => h,
        Err(e) => return format!("Cannot determine data directory: {e}"),
    };
    let data_dir = home.join("data");

    let mgr = match SessionManager::new(data_dir) {
        Ok(m) => m,
        Err(e) => return format!("Session store error: {e}"),
    };

    match mgr.reset_session(session_key) {
        Ok(()) => "Session reset. Conversation history cleared.".to_string(),
        Err(e) => format!("Failed to reset session: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Callback handler (for inline keyboard button presses)
// ---------------------------------------------------------------------------

/// Handle a callback from an inline keyboard button press.
///
/// Parses the `callback_data` string and dispatches to the appropriate handler.
/// Returns `Some(CommandResponse)` if the callback was handled, `None` if
/// unrecognized (caller should fall through to the bus).
///
/// For settings callbacks, this modifies and saves the config file.
/// For history callbacks, this renders the requested page.
pub fn handle_callback(data: &str) -> Option<CommandResponse> {
    let mut parts = data.splitn(3, ':');
    let prefix = parts.next()?;
    let action = parts.next()?;
    let payload = parts.next();

    match prefix {
        "settings" => match action {
            "model" if payload == Some("switch") => Some(handle_settings_model_switch()),
            "streaming" if payload == Some("toggle") => Some(handle_settings_streaming_toggle()),
            _ => None,
        },
        "history" => {
            if action == "page" {
                let page: usize = payload.and_then(|p| p.parse().ok()).unwrap_or(0);
                Some(handle_history_page(page))
            } else {
                None
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EnvVarGuard;
    use kestrel_skill::{manifest::SkillManifestBuilder, skill::CompiledSkill};
    use parking_lot::{Mutex, MutexGuard};
    use std::io::Write as IoWrite;
    use std::sync::{Arc, LazyLock};

    static SKILL_REGISTRY_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    struct SkillRegistryGuard {
        _lock: MutexGuard<'static, ()>,
    }

    impl SkillRegistryGuard {
        fn install(registry: Arc<SkillRegistry>) -> Self {
            let lock = SKILL_REGISTRY_TEST_LOCK.lock();
            set_skill_registry(Some(registry));
            Self { _lock: lock }
        }
    }

    impl Drop for SkillRegistryGuard {
        fn drop(&mut self) {
            set_skill_registry(None);
        }
    }

    fn make_skill(name: &str, description: &str, triggers: &[&str]) -> CompiledSkill {
        let manifest = SkillManifestBuilder::new(name, "1.0.0", description)
            .triggers(triggers.iter().copied())
            .build();
        let mut skill = CompiledSkill::new(manifest);
        skill.set_instructions(format!("Instructions for {name}"));
        skill
    }

    fn make_deprecated_skill(name: &str, description: &str, triggers: &[&str]) -> CompiledSkill {
        let manifest = SkillManifestBuilder::new(name, "1.0.0", description)
            .triggers(triggers.iter().copied())
            .deprecated("replaced")
            .build();
        let mut skill = CompiledSkill::new(manifest);
        skill.set_instructions(format!("Instructions for {name}"));
        skill
    }

    fn expect_response(dispatch: CommandDispatch) -> CommandResponse {
        match dispatch {
            CommandDispatch::Respond(response) => response,
            CommandDispatch::Rewrite(_) => panic!("expected direct response"),
        }
    }

    fn expect_rewrite(dispatch: CommandDispatch) -> String {
        match dispatch {
            CommandDispatch::Rewrite(text) => text,
            CommandDispatch::Respond(_) => panic!("expected rewritten user message"),
        }
    }

    // -- matches_command tests ------------------------------------------------

    #[test]
    fn test_matches_command_exact() {
        assert!(matches_command("/validate", "validate"));
    }

    #[test]
    fn test_matches_command_case_insensitive() {
        assert!(matches_command("/VALIDATE", "validate"));
        assert!(matches_command("/Validate", "validate"));
        assert!(matches_command("/vAlIdAtE", "validate"));
    }

    #[test]
    fn test_matches_command_with_bot_mention() {
        assert!(matches_command("/validate@MyBot", "validate"));
        assert!(matches_command("/validate@some_bot_name", "validate"));
    }

    #[test]
    fn test_matches_command_with_trailing_args() {
        assert!(matches_command("/validate --verbose", "validate"));
        assert!(matches_command("/validate extra stuff", "validate"));
    }

    #[test]
    fn test_matches_command_no_match() {
        assert!(!matches_command("/help", "validate"));
        assert!(!matches_command("/start", "validate"));
        assert!(!matches_command("validate", "validate")); // no slash
        assert!(!matches_command("", "validate"));
        assert!(!matches_command("hello world", "validate"));
    }

    // -- try_handle_command tests --------------------------------------------

    #[tokio::test]
    async fn test_try_handle_command_validate() {
        let result = expect_response(try_handle_command("/validate").await.unwrap());
        assert!(result.text.contains("Configuration"));
        assert!(result.keyboard.is_none());
    }

    #[tokio::test]
    async fn test_try_handle_command_menu() {
        let result = try_handle_command("/menu").await;
        assert!(result.is_some());
        let resp = expect_response(result.unwrap());
        assert_eq!(resp.text, "What would you like to do?");
        assert!(resp.keyboard.is_some());
        let kb = resp.keyboard.unwrap();
        assert_eq!(kb.inline_keyboard.len(), 2);
        assert_eq!(kb.inline_keyboard[0][0].text, "Status");
        assert_eq!(kb.inline_keyboard[0][1].text, "Help");
        assert_eq!(kb.inline_keyboard[1][0].text, "Validate Config");
        assert_eq!(kb.inline_keyboard[1][1].text, "Cancel");
    }

    #[tokio::test]
    async fn test_try_handle_command_other() {
        assert!(try_handle_command("/unknown_cmd").await.is_none());
        assert!(try_handle_command("/help").await.is_some());
        assert!(try_handle_command("hello").await.is_none());
        assert!(try_handle_command("").await.is_none());
    }

    // -- CommandResponse tests -----------------------------------------------

    #[test]
    fn test_command_response_text() {
        let r = CommandResponse::text("hello");
        assert_eq!(r.text, "hello");
        assert!(r.keyboard.is_none());
    }

    #[test]
    fn test_command_response_with_keyboard() {
        let kb = InlineKeyboardBuilder::new()
            .button("A", "a")
            .new_row()
            .build();
        let r = CommandResponse::with_keyboard("pick", kb);
        assert_eq!(r.text, "pick");
        assert!(r.keyboard.is_some());
    }

    // -- helpers -------------------------------------------------------------

    /// Helper: create a temp dir with a config.yaml and set KESTREL_HOME.
    struct TestHome {
        _dir: tempfile::TempDir,
        _env: EnvVarGuard,
    }

    fn with_temp_config(yaml: &str) -> TestHome {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        let mut f = std::fs::File::create(&config_path).unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        let env = EnvVarGuard::set("KESTREL_HOME", dir.path());
        TestHome {
            _dir: dir,
            _env: env,
        }
    }

    /// Helper: create a temp dir with no config file.
    fn with_empty_home() -> TestHome {
        let dir = tempfile::tempdir().unwrap();
        let env = EnvVarGuard::set("KESTREL_HOME", dir.path());
        TestHome {
            _dir: dir,
            _env: env,
        }
    }

    // -- handle_validate tests -----------------------------------------------

    #[test]
    fn test_handle_validate_default_config() {
        // No config file → load_config returns default Config which has no
        // providers → validation reports errors.
        let _dir = with_empty_home();
        let result = handle_validate();
        assert!(result.contains("Configuration"));
        // Default config should report about missing providers or similar.
        assert!(result.contains("Agent:"));
        assert!(result.contains("Providers:"));
    }

    #[test]
    fn test_handle_validate_valid_config() {
        let yaml = r#"
providers:
  openai:
    api_key: "sk-test123"
channels:
  telegram:
    token: "123456:ABC"
"#;
        let _dir = with_temp_config(yaml);
        let result = handle_validate();
        // With a valid provider and channel, the config should be valid or
        // at least parseable.
        assert!(result.contains("Configuration"));
        assert!(result.contains("Agent:"));
        assert!(result.contains("openai"));
        assert!(result.contains("telegram"));
    }

    #[test]
    fn test_handle_validate_invalid_config() {
        // Empty agent model triggers an error.
        let yaml = r#"
agent:
  model: ""
"#;
        let _dir = with_temp_config(yaml);
        let result = handle_validate();
        assert!(result.contains("error(s)") || result.contains("[ERROR]"));
    }

    #[test]
    fn test_handle_validate_summary_shows_name() {
        let yaml = r#"
name: "testbot"
providers:
  openai:
    api_key: "sk-test"
"#;
        let _dir = with_temp_config(yaml);
        let result = handle_validate();
        assert!(result.contains("Name: testbot"));
    }

    #[test]
    fn test_handle_validate_summary_shows_unnamed() {
        let yaml = r#"
providers:
  openai:
    api_key: "sk-test"
"#;
        let _dir = with_temp_config(yaml);
        let result = handle_validate();
        assert!(result.contains("Name: unnamed"));
    }

    #[test]
    fn test_handle_validate_summary_shows_providers() {
        let yaml = r#"
providers:
  openai:
    api_key: "sk-test"
  anthropic:
    api_key: "sk-ant-test"
"#;
        let _dir = with_temp_config(yaml);
        let result = handle_validate();
        assert!(result.contains("openai"));
        assert!(result.contains("anthropic"));
    }

    #[test]
    fn test_handle_validate_summary_shows_channels() {
        let yaml = r#"
channels:
  telegram:
    token: "123:ABC"
  discord:
    token: "discord-token"
"#;
        let _dir = with_temp_config(yaml);
        let result = handle_validate();
        assert!(result.contains("telegram (enabled)"));
        assert!(result.contains("discord (enabled)"));
    }

    #[test]
    fn test_handle_validate_summary_channels_disabled() {
        let yaml = r#"
channels:
  telegram:
    token: "123:ABC"
    enabled: false
"#;
        let _dir = with_temp_config(yaml);
        let result = handle_validate();
        assert!(result.contains("telegram (disabled)"));
    }

    #[test]
    fn test_handle_validate_no_providers() {
        let yaml = r#"
# empty config
"#;
        let _dir = with_temp_config(yaml);
        let result = handle_validate();
        assert!(result.contains("Providers: (none)") || result.contains("error"));
    }

    #[test]
    fn test_handle_validate_no_channels() {
        let yaml = r#"
providers:
  openai:
    api_key: "sk-test"
"#;
        let _dir = with_temp_config(yaml);
        let result = handle_validate();
        assert!(result.contains("Channels: (none)") || result.contains("Channel"));
    }

    // -- /help tests ---------------------------------------------------------

    #[test]
    fn test_handle_help_lists_commands() {
        let result = handle_help();
        assert!(result.contains("/help"));
        assert!(result.contains("/status"));
        assert!(result.contains("/skill"));
        assert!(result.contains("/validate"));
        assert!(result.contains("/settings"));
        assert!(result.contains("/history"));
    }

    #[tokio::test]
    async fn test_try_handle_command_help() {
        let r = expect_response(try_handle_command("/help").await.unwrap());
        assert!(r.text.contains("/help"));
    }

    #[test]
    fn test_handle_help_includes_reset() {
        let result = handle_help();
        assert!(result.contains("/reset"));
    }

    // -- /status tests -------------------------------------------------------

    #[test]
    fn test_handle_status_basic() {
        let _dir = with_empty_home();
        let result = handle_status();
        assert!(result.contains("Agent:"));
        assert!(result.contains("Heartbeat:"));
    }

    #[test]
    fn test_handle_status_no_key() {
        let yaml = r#"
providers:
  openai:
    api_key: ""
"#;
        let _dir = with_temp_config(yaml);
        let result = handle_status();
        assert!(result.contains("openai: no key"));
    }

    #[test]
    fn test_handle_status_heartbeat_disabled() {
        let yaml = r#"
heartbeat:
  enabled: false
"#;
        let _dir = with_temp_config(yaml);
        let result = handle_status();
        assert!(result.contains("Heartbeat: disabled"));
    }

    // -- format_key_status tests ---------------------------------------------

    #[test]
    fn test_format_key_status_ok() {
        assert_eq!(format_key_status("openai", Some("sk-abc")), "openai: ok");
    }

    #[test]
    fn test_format_key_status_no_key() {
        assert_eq!(format_key_status("openai", None), "openai: no key");
    }

    #[test]
    fn test_format_key_status_empty() {
        assert_eq!(format_key_status("openai", Some("")), "openai: no key");
    }

    // -- /menu tests ---------------------------------------------------------

    #[test]
    fn test_menu_keyboard_structure() {
        let kb = menu_keyboard();
        assert_eq!(kb.inline_keyboard.len(), 2);
        // Row 1: Status, Help
        assert_eq!(kb.inline_keyboard[0].len(), 2);
        assert_eq!(
            kb.inline_keyboard[0][0].callback_data,
            Some("menu:status".to_string())
        );
        assert_eq!(
            kb.inline_keyboard[0][1].callback_data,
            Some("menu:help".to_string())
        );
        // Row 2: Validate Config, Cancel
        assert_eq!(kb.inline_keyboard[1].len(), 2);
        assert_eq!(
            kb.inline_keyboard[1][0].callback_data,
            Some("menu:validate".to_string())
        );
        assert_eq!(
            kb.inline_keyboard[1][1].callback_data,
            Some("menu:cancel".to_string())
        );
    }

    #[test]
    fn test_handle_menu_callback_status() {
        let _dir = with_temp_config("providers:\n  openai:\n    api_key: sk-test\n");
        let (text, kb) = handle_menu_callback("status");
        assert!(text.contains("Agent:"));
        assert!(kb.is_some());
    }

    #[test]
    fn test_handle_menu_callback_help() {
        let (text, kb) = handle_menu_callback("help");
        assert!(text.contains("/menu"));
        assert!(text.contains("/validate"));
        assert!(kb.is_some());
    }

    #[test]
    fn test_handle_menu_callback_validate() {
        let _dir = with_temp_config("providers:\n  openai:\n    api_key: sk-test\n");
        let (text, kb) = handle_menu_callback("validate");
        assert!(text.contains("Configuration"));
        assert!(kb.is_some());
    }

    #[test]
    fn test_handle_menu_callback_cancel() {
        let (text, kb) = handle_menu_callback("cancel");
        assert_eq!(text, "Menu closed.");
        assert!(kb.is_none());
    }

    #[test]
    fn test_handle_menu_callback_unknown() {
        let (text, kb) = handle_menu_callback("nonexistent");
        assert!(text.contains("Unknown action"));
        assert!(kb.is_some());
    }

    // -- /settings tests (toggle version) -------------------------------------

    #[test]
    fn test_handle_settings_has_keyboard() {
        let yaml = r#"
providers:
  openai:
    api_key: "sk-test"
"#;
        let _dir = with_temp_config(yaml);
        let r = handle_settings();
        assert!(r.text.contains("Settings"));
        assert!(r.text.contains("Model:"));
        assert!(r.keyboard.is_some());
    }

    #[tokio::test]
    async fn test_try_handle_command_settings() {
        let yaml = r#"
providers:
  openai:
    api_key: "sk-test"
"#;
        let _dir = with_temp_config(yaml);
        let r = expect_response(try_handle_command("/settings").await.unwrap());
        assert!(r.text.contains("Settings"));
        assert!(r.keyboard.is_some());
    }

    #[test]
    fn test_settings_keyboard_buttons() {
        let yaml = r#"
providers:
  openai:
    api_key: "sk-test"
"#;
        let _dir = with_temp_config(yaml);
        let r = handle_settings();
        let kb = r.keyboard.unwrap();
        // Should have the row with model switch + streaming toggle.
        assert!(!kb.inline_keyboard.is_empty());
        let row = &kb.inline_keyboard[0];
        assert_eq!(row.len(), 2);
        assert!(row[0].callback_data.as_ref().unwrap().contains("settings"));
        assert!(row[1].callback_data.as_ref().unwrap().contains("settings"));
    }

    // -- /settings tests (paginated view) -------------------------------------

    #[test]
    fn test_handle_settings_single_page() {
        let yaml = r#"
providers:
  openai:
    api_key: "sk-test"
"#;
        let _dir = with_temp_config(yaml);
        let resp = handle_settings_callback(0);
        // Page 0 contains Name, Model, Streaming, Max tokens, Temperature.
        assert!(resp.text.contains("Model:"));
        assert!(resp.text.contains("Settings"));
    }

    #[test]
    fn test_handle_settings_first_page() {
        let _dir = with_empty_home();
        let resp = handle_settings_callback(0);
        assert!(resp.text.contains("page 1/"));
        assert!(resp.text.contains("Name:"));
    }

    #[test]
    fn test_handle_settings_page_clamped_to_last() {
        let _dir = with_empty_home();
        // Requesting page 999 should clamp to the last valid page.
        let resp = handle_settings_callback(999);
        // Should not panic and should still show settings.
        assert!(resp.text.contains("Settings"));
    }

    #[test]
    fn test_handle_settings_shows_name() {
        let yaml = r#"
name: "mybot"
providers:
  openai:
    api_key: "sk-test"
"#;
        let _dir = with_temp_config(yaml);
        let resp = handle_settings_callback(0);
        assert!(resp.text.contains("mybot"));
    }

    #[test]
    fn test_handle_settings_shows_providers() {
        let yaml = r#"
providers:
  openai:
    api_key: "sk-test"
  anthropic:
    api_key: "sk-ant-test"
"#;
        let _dir = with_temp_config(yaml);
        // Providers are on page 1 (index 5+ out of 7 settings with page size 5).
        let resp = handle_settings_callback(1);
        assert!(resp.text.contains("openai"));
        assert!(resp.text.contains("anthropic"));
    }

    #[test]
    fn test_handle_settings_no_providers() {
        let yaml = "# empty\n";
        let _dir = with_temp_config(yaml);
        // Providers on page 1.
        let resp = handle_settings_callback(1);
        assert!(resp.text.contains("(none)") || resp.text.contains("Providers:"));
    }

    // -- /history tests (from cc-feat — SessionManager-based) ----------------

    #[test]
    fn test_handle_history_no_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let r = handle_history_page_impl(0, &data_dir);
        assert!(r.text.contains("No conversation history"));
    }

    #[test]
    fn test_handle_history_with_data() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let mgr = SessionManager::new(data_dir.clone()).unwrap();
        let mut session = mgr.get_or_create("telegram:123", None);
        session.add_user_message("hello".to_string());
        session.add_assistant_message("hi there".to_string());
        mgr.save_session(&session).unwrap();

        let r = handle_history_page_impl(0, &data_dir);
        assert!(r.text.contains("History"));
        assert!(r.text.contains("hello"));
        assert!(r.text.contains("hi there"));
    }

    #[test]
    fn test_handle_history_pagination_keyboard() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let mgr = SessionManager::new(data_dir.clone()).unwrap();
        let mut session = mgr.get_or_create("telegram:999", None);
        // Add more than HISTORY_PAGE_SIZE entries to trigger pagination.
        for i in 0..=HISTORY_PAGE_SIZE {
            session.add_user_message(format!("msg {}", i));
        }
        mgr.save_session(&session).unwrap();

        let r = handle_history_page_impl(0, &data_dir);
        assert!(r.keyboard.is_some(), "should have pagination keyboard");
    }

    // -- /history tests (from agent-a — session-key-based) --------------------

    #[tokio::test]
    async fn test_try_handle_command_history() {
        let result = try_handle_command("/history").await;
        assert!(result.is_some());
        let resp = expect_response(result.unwrap());
        assert_eq!(resp.text, "No conversation history found.");
        assert!(resp.keyboard.is_none());
    }

    #[tokio::test]
    async fn test_try_handle_command_skill_list() {
        let registry = Arc::new(SkillRegistry::new());
        registry
            .register(make_skill(
                "deploy-k8s",
                "Deploy to Kubernetes",
                &["deploy", "k8s"],
            ))
            .await
            .unwrap();
        let _guard = SkillRegistryGuard::install(registry);

        let response = expect_response(try_handle_command("/skill").await.unwrap());

        assert!(response.text.contains("Registered skills"));
        assert!(response.text.contains("deploy-k8s"));
        assert!(response.text.contains("confidence 0.50"));
    }

    #[tokio::test]
    async fn test_try_handle_command_skill_view() {
        let registry = Arc::new(SkillRegistry::new());
        registry
            .register(make_skill(
                "plan-release",
                "Plan a release",
                &["plan", "release"],
            ))
            .await
            .unwrap();
        let _guard = SkillRegistryGuard::install(registry);

        let response = expect_response(
            try_handle_command("/skill view plan-release")
                .await
                .unwrap(),
        );

        assert!(response.text.contains("Skill: plan-release"));
        assert!(response.text.contains("description = \"Plan a release\""));
        assert!(response.text.contains("Instructions for plan-release"));
    }

    #[tokio::test]
    async fn test_try_handle_command_skill_search() {
        let registry = Arc::new(SkillRegistry::new());
        registry
            .register(make_skill(
                "incident-response",
                "Handle incidents",
                &["incident", "pager"],
            ))
            .await
            .unwrap();
        let _guard = SkillRegistryGuard::install(registry);

        let response = expect_response(
            try_handle_command("/skill search pager alert")
                .await
                .unwrap(),
        );

        assert!(response.text.contains("Skill matches"));
        assert!(response.text.contains("incident-response"));
        assert!(response.text.contains("score"));
    }

    #[tokio::test]
    async fn test_try_handle_dynamic_skill_command_rewrites_to_user_message() {
        let registry = Arc::new(SkillRegistry::new());
        registry
            .register(make_skill(
                "plan-release",
                "Plan a release",
                &["plan", "release"],
            ))
            .await
            .unwrap();
        let _guard = SkillRegistryGuard::install(registry);

        let rewritten = expect_rewrite(
            try_handle_command("/plan-release prepare the rollback checklist")
                .await
                .unwrap(),
        );

        assert!(rewritten.contains("The user explicitly invoked the /plan-release skill"));
        assert!(rewritten.contains("Skill name: plan-release"));
        assert!(rewritten.contains("Instructions for plan-release"));
        assert!(rewritten.contains("prepare the rollback checklist"));
    }

    #[tokio::test]
    async fn test_try_handle_dynamic_skill_command_ignores_deprecated_skills() {
        let registry = Arc::new(SkillRegistry::new());
        registry
            .register(make_deprecated_skill(
                "legacy-plan",
                "Old plan skill",
                &["legacy"],
            ))
            .await
            .unwrap();
        let _guard = SkillRegistryGuard::install(registry);

        assert!(try_handle_command("/legacy-plan").await.is_none());
    }

    #[tokio::test]
    async fn test_dynamic_skill_commands_excludes_reserved_names() {
        let registry = Arc::new(SkillRegistry::new());
        registry
            .register(make_skill("skill", "Conflicting name", &["skill"]))
            .await
            .unwrap();
        registry
            .register(make_skill("release-plan", "Release planning", &["release"]))
            .await
            .unwrap();
        let _guard = SkillRegistryGuard::install(registry);

        let commands = dynamic_skill_commands().await;
        assert!(commands
            .iter()
            .any(|(command, _)| command == "release-plan"));
        assert!(!commands.iter().any(|(command, _)| command == "skill"));
    }

    #[test]
    fn test_handle_history_empty() {
        let resp = handle_history(&[], 0);
        assert_eq!(resp.text, "No active sessions.");
        assert!(resp.keyboard.is_none());
    }

    #[test]
    fn test_handle_history_single_page() {
        let keys: Vec<String> = vec!["telegram:123".to_string(), "discord:456".to_string()];
        let resp = handle_history(&keys, 0);
        assert!(resp.text.contains("Sessions"));
        assert!(resp.text.contains("telegram:123"));
        assert!(resp.text.contains("discord:456"));
        assert!(resp.keyboard.is_none());
    }

    #[test]
    fn test_handle_history_multi_page() {
        let keys: Vec<String> = (0..12).map(|i| format!("session:{}", i)).collect();
        let resp = handle_history(&keys, 0);
        assert!(resp.text.contains("page 1/"));
        assert!(resp.keyboard.is_some());
        let kb = resp.keyboard.unwrap();
        let row = &kb.inline_keyboard[0];
        assert!(row.iter().any(|b| b.text.contains("Next")));
    }

    #[test]
    fn test_handle_history_second_page() {
        let keys: Vec<String> = (0..12).map(|i| format!("session:{}", i)).collect();
        let resp = handle_history(&keys, 1);
        assert!(resp.text.contains("page 2/"));
        assert!(resp.text.contains("session:5"));
        assert!(!resp.text.contains("session:4"));
    }

    #[test]
    fn test_handle_history_page_clamped() {
        let keys: Vec<String> = vec!["a".to_string(), "b".to_string()];
        let resp = handle_history(&keys, 999);
        assert!(resp.text.contains("a"));
    }

    #[test]
    fn test_handle_history_callback_delegates() {
        let keys: Vec<String> = vec!["x".to_string(), "y".to_string()];
        let resp = handle_history_callback(&keys, 0);
        assert!(resp.text.contains("x"));
        assert!(resp.text.contains("y"));
    }

    #[test]
    fn test_handle_history_shows_index_numbers() {
        let keys: Vec<String> = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
        let resp = handle_history(&keys, 0);
        assert!(resp.text.contains("1. alpha"));
        assert!(resp.text.contains("2. beta"));
        assert!(resp.text.contains("3. gamma"));
    }

    #[test]
    fn test_short_key() {
        assert_eq!(short_key("telegram:123"), "tg:123");
        assert_eq!(short_key("discord:456"), "dc:456");
        assert_eq!(short_key("web:789"), "web:789");
        assert_eq!(short_key("single"), "single");
    }

    #[test]
    fn test_truncate_str_short() {
        assert_eq!(truncate_str("hi", 10), "hi");
    }

    #[test]
    fn test_truncate_str_exact() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_str_long() {
        let result = truncate_str("hello world this is long", 11);
        assert_eq!(result, "hello world...");
        assert!(result.len() <= 14);
    }

    #[test]
    fn test_truncate_str_multibyte() {
        let result = truncate_str("日本語テストです", 6);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_handle_reset_success() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let mgr = SessionManager::new(data_dir).unwrap();
        let mut session = mgr.get_or_create("telegram:123", None);
        session.add_user_message("hello".to_string());
        session.add_assistant_message("hi".to_string());
        mgr.save_session(&session).unwrap();

        assert!(!mgr.get_or_create("telegram:123", None).messages.is_empty());

        let _env = EnvVarGuard::set("KESTREL_HOME", dir.path());
        let result = handle_reset("telegram:123");
        assert!(result.contains("cleared") || result.contains("reset"));
    }

    #[test]
    fn test_handle_reset_no_session() {
        let dir = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set("KESTREL_HOME", dir.path());
        let result = handle_reset("telegram:99999");
        assert!(result.contains("cleared") || result.contains("reset") || result.contains("ok"));
    }

    #[test]
    fn test_handle_callback_unknown() {
        assert!(handle_callback("unknown:action").is_none());
        assert!(handle_callback("").is_none());
        assert!(handle_callback("foo:bar:baz").is_none());
    }

    #[test]
    fn test_handle_callback_history_page() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();

        let mgr = SessionManager::new(data_dir.clone()).unwrap();
        let mut session = mgr.get_or_create("telegram:456", None);
        session.add_user_message("hello".to_string());
        mgr.save_session(&session).unwrap();

        let _env = EnvVarGuard::set("KESTREL_HOME", dir.path());
        let resp = handle_callback("history:page:0").unwrap();
        assert!(resp.text.contains("History"));
    }

    #[test]
    fn test_handle_callback_settings_model_switch() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
agent:
  model: "gpt-4o"
  streaming: true
"#;
        let _env = EnvVarGuard::set("KESTREL_HOME", dir.path());
        let config_path = dir.path().join("config.yaml");
        std::fs::write(&config_path, yaml).unwrap();

        let resp = handle_callback("settings:model:switch").unwrap();
        assert!(resp.text.contains("Model:"));
        assert!(resp.keyboard.is_some());
        assert!(!resp.text.contains("gpt-4o") || MODEL_CYCLE.len() == 1);
    }

    #[test]
    fn test_handle_callback_settings_streaming_toggle() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
agent:
  model: "gpt-4o"
  streaming: true
"#;
        let _env = EnvVarGuard::set("KESTREL_HOME", dir.path());
        let config_path = dir.path().join("config.yaml");
        std::fs::write(&config_path, yaml).unwrap();

        let resp = handle_callback("settings:streaming:toggle").unwrap();
        assert!(resp.text.contains("Streaming: off"));
        assert!(resp.keyboard.is_some());

        let resp2 = handle_callback("settings:streaming:toggle").unwrap();
        assert!(resp2.text.contains("Streaming: on"));
    }

    #[test]
    fn test_handle_callback_settings_unknown_action() {
        assert!(handle_callback("settings:unknown:foo").is_none());
    }

    #[test]
    fn test_model_cycle_constants() {
        assert!(!MODEL_CYCLE.is_empty());
        let mut seen = std::collections::HashSet::new();
        for m in MODEL_CYCLE {
            assert!(seen.insert(*m), "duplicate model in MODEL_CYCLE: {m}");
        }
    }

    #[test]
    fn test_rebuild_callback_data_with_payload() {
        use crate::platforms::telegram::{rebuild_callback_data, CallbackAction, CallbackContext};
        let ctx = CallbackContext {
            chat_id: "123".to_string(),
            message_id: "456".to_string(),
            sender_id: "789".to_string(),
            callback_query_id: "abc".to_string(),
            action: CallbackAction {
                prefix: "history".to_string(),
                action: "page".to_string(),
                payload: Some("2".to_string()),
            },
        };
        assert_eq!(rebuild_callback_data(&ctx), "history:page:2");
    }

    #[test]
    fn test_rebuild_callback_data_without_payload() {
        use crate::platforms::telegram::{rebuild_callback_data, CallbackAction, CallbackContext};
        let ctx = CallbackContext {
            chat_id: "123".to_string(),
            message_id: "456".to_string(),
            sender_id: "789".to_string(),
            callback_query_id: "abc".to_string(),
            action: CallbackAction {
                prefix: "settings".to_string(),
                action: "model".to_string(),
                payload: None,
            },
        };
        assert_eq!(rebuild_callback_data(&ctx), "settings:model");
    }
}
