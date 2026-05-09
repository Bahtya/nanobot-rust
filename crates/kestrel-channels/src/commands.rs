//! Built-in slash command handlers.
//!
//! Provides channel-level command handling that runs *before* messages are
//! forwarded to the message bus.  This means commands like `/validate` and
//! `/status` work even when the LLM provider is misconfigured.

use kestrel_config::validate::ValidationFinding;
use kestrel_config::{load_config, validate, Config};
use kestrel_providers::{ModelCatalog, ModelInfo};
use kestrel_session::SessionManager;
use kestrel_skill::{Skill, SkillRegistry};
use std::fmt::Write;
use std::sync::{Arc, LazyLock};

use parking_lot::RwLock;
use tokio::sync::OnceCell;

use crate::platforms::telegram::{InlineKeyboardBuilder, InlineKeyboardMarkup};

/// Fallback model names for cycling when dynamic discovery is unavailable.
const MODEL_CYCLE: &[&str] = &[
    "gpt-4o",
    "claude-sonnet-4-6",
    "deepseek-chat",
    "deepseek-v4-flash",
];

static MODEL_CATALOG: OnceCell<ModelCatalog> = OnceCell::const_new();

static SKILL_REGISTRY: LazyLock<RwLock<Option<Arc<SkillRegistry>>>> =
    LazyLock::new(|| RwLock::new(None));

/// Configure the shared skill registry used by built-in `/skill` commands.
pub fn set_skill_registry(registry: Option<Arc<SkillRegistry>>) {
    *SKILL_REGISTRY.write() = registry;
}

fn configured_skill_registry() -> Option<Arc<SkillRegistry>> {
    SKILL_REGISTRY.read().clone()
}

/// Get or initialize the shared model catalog.
pub async fn get_model_catalog_static() -> &'static ModelCatalog {
    MODEL_CATALOG
        .get_or_init(|| async {
            let config = load_config(None).unwrap_or_default();
            kestrel_providers::build_catalog(&config)
        })
        .await
}

async fn get_model_catalog() -> &'static ModelCatalog {
    get_model_catalog_static().await
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
        "help"
            | "status"
            | "validate"
            | "skill"
            | "settings"
            | "history"
            | "reset"
            | "menu"
            | "models"
    )
}

fn normalize_skill_command_key(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut last_was_sep = false;

    for ch in name.chars().flat_map(char::to_lowercase) {
        let mapped = match ch {
            'a'..='z' | '0'..='9' => Some(ch),
            '-' | '_' | ' ' => Some('_'),
            _ => None,
        };

        match mapped {
            Some('_') if !last_was_sep && !normalized.is_empty() => {
                normalized.push('_');
                last_was_sep = true;
            }
            Some('_') => {}
            Some(value) => {
                normalized.push(value);
                last_was_sep = false;
            }
            None => {}
        }
    }

    normalized.trim_matches('_').to_string()
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
    } else if matches_command(text, "models") {
        Some(CommandDispatch::Respond(
            handle_models_provider_list().await,
        ))
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
    let _ = writeln!(out, "/validate - Validate config.toml and show results");
    let _ = writeln!(out, "/settings - Toggle preferences (notifications, model)");
    let _ = writeln!(out, "/models   - Browse and select models from providers");
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
    let _ = writeln!(
        out,
        "Provider: {}",
        config.agent.provider.as_deref().unwrap_or("(auto)")
    );
    let _ = writeln!(out, "Model: {}", config.agent.model);
    let _ = writeln!(
        out,
        "Streaming: {}",
        if config.agent.streaming { "on" } else { "off" }
    );
    let _ = writeln!(out, "\nTap a button to change:");

    let keyboard = InlineKeyboardBuilder::new()
        .row_pair(
            "Models: pick",
            "models:providers",
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

/// Format provider and model for display, e.g. "opencode_go/glm-5.1".
fn fmt_provider_model(config: &Config) -> String {
    fmt_provider_model_raw(
        config.agent.provider.as_deref().unwrap_or(""),
        &config.agent.model,
    )
}

fn fmt_provider_model_raw(provider: &str, model: &str) -> String {
    if provider.is_empty() {
        model.to_string()
    } else {
        format!("{}/{}", provider, model)
    }
}

// ---------------------------------------------------------------------------
// /settings text-based implementation (for WebSocket and other non-keyboard channels)
// ---------------------------------------------------------------------------

/// Handle `/models` for text-only channels (WebSocket, etc.) that lack inline keyboards.
///
/// Two-level text-based interaction:
/// - `/models` — list available providers with model counts
/// - `/models <provider>` — list models for a specific provider
/// - `/models <provider>/<model_id>` — select a model
/// - `/models refresh` — invalidate model cache and show providers
pub async fn handle_ws_models(text: &str) -> String {
    let args = command_arguments(text);

    if args.eq_ignore_ascii_case("refresh") {
        let catalog = get_model_catalog().await;
        catalog.invalidate_cache().await;
        return ws_models_provider_list().await;
    }

    // Check if args looks like a provider/model selection (contains /).
    if let Some((provider, model_id)) = args.split_once('/') {
        if !provider.is_empty() && !model_id.is_empty() {
            return ws_models_select(provider, model_id);
        }
    }

    // If args matches a known provider name, show its models.
    if !args.is_empty() {
        let catalog = get_model_catalog().await;
        let models = catalog.list_all_models().await;
        let provider_models: Vec<_> = models
            .iter()
            .filter(|m| m.provider.eq_ignore_ascii_case(args))
            .collect();
        if !provider_models.is_empty() {
            return ws_models_detail(args, &provider_models).await;
        }
    }

    // Default: show provider list.
    ws_models_provider_list().await
}

async fn ws_models_provider_list() -> String {
    let config = match load_config(None) {
        Ok(c) => c,
        Err(e) => return format!("Failed to load config: {e}"),
    };

    let current = fmt_provider_model(&config);

    let catalog = get_model_catalog().await;
    let models = catalog.list_all_models().await;

    if models.is_empty() {
        return "No models discovered. Configure a provider (e.g. opencode_go) in config.toml."
            .to_string();
    }

    let mut provider_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for m in &models {
        *provider_counts.entry(m.provider.clone()).or_insert(0) += 1;
    }

    let mut out = String::new();
    let _ = writeln!(out, "Providers (current: {}):\n", current);
    for (provider, count) in &provider_counts {
        let _ = writeln!(out, "  {} ({} models)", provider, count);
    }
    let _ = writeln!(out, "\nUsage:");
    let _ = writeln!(out, "/models <provider> — list models");
    let _ = writeln!(out, "/models <provider>/<model> — select model");
    out
}

async fn ws_models_detail(provider: &str, models: &[&ModelInfo]) -> String {
    let config = match load_config(None) {
        Ok(c) => c,
        Err(e) => return format!("Failed to load config: {e}"),
    };
    let current = fmt_provider_model(&config);

    let mut out = String::new();
    let _ = writeln!(out, "[{}] models (current: {}):\n", provider, current);
    for m in models {
        let ctx = m
            .context_length
            .map(|c| format!(" ({}K ctx)", c / 1024))
            .unwrap_or_default();
        let _ = writeln!(out, "  {}{}", m.id, ctx);
    }
    let _ = writeln!(out, "\nUse /models {}/<model_id> to select.", provider);
    out
}

fn ws_models_select(provider: &str, model_id: &str) -> String {
    let mut config = match load_config(None) {
        Ok(c) => c,
        Err(e) => return format!("Failed to load config: {e}"),
    };

    let old_provider = config.agent.provider.clone().unwrap_or_default();
    let old_model = config.agent.model.clone();
    config.agent.provider = Some(provider.to_string());
    config.agent.model = model_id.to_string();

    if let Err(e) = save_config_to_default(&config) {
        return format!("Failed to save config: {e}");
    }

    format!(
        "Model changed: {} → {} ({})",
        fmt_provider_model_raw(&old_provider, &old_model),
        model_id,
        provider
    )
}

/// Handle `/settings` for text-only channels (WebSocket, etc.) that lack inline keyboards.
///
/// Subcommands:
/// - `/settings` — show current settings
/// - `/settings model` — show current model
/// - `/settings model next` — cycle to next model
/// - `/settings model <name>` — set model by name
/// - `/settings models` — list all available models from providers
/// - `/settings models refresh` — force refresh model list from APIs
/// - `/settings streaming` — toggle streaming
pub async fn handle_ws_settings(text: &str) -> String {
    let args = command_arguments(text);

    if args.is_empty() {
        let config = match load_config(None) {
            Ok(c) => c,
            Err(e) => return format!("Failed to load config: {e}"),
        };
        let mut out = String::new();
        let _ = writeln!(out, "Settings");
        let _ = writeln!(
            out,
            "Provider: {}",
            config.agent.provider.as_deref().unwrap_or("(auto)")
        );
        let _ = writeln!(out, "Model: {}", config.agent.model);
        let _ = writeln!(
            out,
            "Streaming: {}",
            if config.agent.streaming { "on" } else { "off" }
        );
        let _ = writeln!(out, "\nUsage:");
        let _ = writeln!(out, "/settings model — show current model");
        let _ = writeln!(out, "/settings model next — cycle to next model");
        let _ = writeln!(out, "/settings model <name> — set model by name");
        let _ = writeln!(out, "/settings models — list all available models");
        let _ = writeln!(out, "/settings models refresh — refresh model list");
        let _ = writeln!(out, "/settings streaming — toggle streaming");
        let _ = writeln!(out, "/settings gateway — show API gateway config");
        let _ = writeln!(out, "/settings timeout — show timeout settings");
        let _ = writeln!(out, "/settings timeout <key> <secs> — set a timeout");
        let _ = writeln!(out, "/settings retry — show retry policy");
        return out;
    }

    let mut parts = args.splitn(2, char::is_whitespace);
    let subcommand = parts.next().unwrap_or_default();

    match subcommand.to_ascii_lowercase().as_str() {
        "model" => {
            let rest = parts.next().unwrap_or("").trim();
            if rest.is_empty() {
                let config = match load_config(None) {
                    Ok(c) => c,
                    Err(e) => return format!("Failed to load config: {e}"),
                };
                return format!("Current: {}", fmt_provider_model(&config));
            }
            if rest.eq_ignore_ascii_case("next") {
                return ws_settings_model_switch().await;
            }
            ws_settings_model_set(rest)
        }
        "models" => {
            let rest = parts.next().unwrap_or("").trim();
            ws_settings_models_list(rest).await
        }
        "streaming" => ws_settings_streaming_toggle(),
        "gateway" => ws_settings_gateway(),
        "timeout" => {
            let rest = parts.next().unwrap_or("").trim();
            ws_settings_timeout(rest)
        }
        "retry" => ws_settings_retry(),
        _ => "Usage:\n/settings model [next|<name>]\n/settings models [refresh]\n/settings streaming\n/settings gateway\n/settings timeout [key secs]\n/settings retry".to_string(),
    }
}

async fn ws_settings_model_switch() -> String {
    let mut config = match load_config(None) {
        Ok(c) => c,
        Err(e) => return format!("Failed to load config: {e}"),
    };

    let current_provider = config.agent.provider.as_deref().unwrap_or("");
    let current_model = &config.agent.model;

    // Build cycle from discovered models, scoped to current provider.
    let catalog = get_model_catalog().await;
    let discovered = catalog.list_all_models().await;

    let candidates: Vec<ModelInfo> = if !current_provider.is_empty() {
        discovered
            .into_iter()
            .filter(|m| m.provider == current_provider)
            .collect()
    } else {
        discovered
    };

    // Fallback to MODEL_CYCLE if no discovered models.
    if candidates.is_empty() {
        let cycle: Vec<&str> = MODEL_CYCLE.to_vec();
        let idx = cycle
            .iter()
            .position(|m| m.eq_ignore_ascii_case(current_model))
            .map(|i| (i + 1) % cycle.len())
            .unwrap_or(0);
        config.agent.model = cycle[idx].to_string();
    } else {
        let idx = candidates
            .iter()
            .position(|m| m.id.eq_ignore_ascii_case(current_model))
            .map(|i| (i + 1) % candidates.len())
            .unwrap_or(0);
        let next = &candidates[idx];
        config.agent.provider = Some(next.provider.clone());
        config.agent.model = next.id.clone();
    }

    if let Err(e) = save_config_to_default(&config) {
        return format!("Failed to save config: {e}");
    }

    format!("Model switched to: {}", fmt_provider_model(&config))
}

async fn ws_settings_models_list(arg: &str) -> String {
    let catalog = get_model_catalog().await;

    if arg.eq_ignore_ascii_case("refresh") {
        catalog.invalidate_cache().await;
    }

    let models = catalog.list_all_models().await;
    if models.is_empty() {
        return "No models discovered. Configure a provider (e.g. opencode_go) in config.toml."
            .to_string();
    }

    let mut out = String::new();
    let _ = writeln!(out, "Available models ({}):\n", models.len());

    // Group by provider for readability.
    let mut current_provider = String::new();
    for m in &models {
        if m.provider != current_provider {
            current_provider = m.provider.clone();
            let _ = writeln!(out, "[{}]", current_provider);
        }
        let ctx = m
            .context_length
            .map(|c| format!(" ({}K ctx)", c / 1024))
            .unwrap_or_default();
        let _ = writeln!(out, "  {}{}", m.id, ctx);
    }

    let _ = writeln!(out, "\nUse /settings model <id> to select.");
    out
}

fn ws_settings_model_set(name: &str) -> String {
    // If name contains a /, treat it as provider/model and use the full selection flow.
    if let Some((provider, model_id)) = name.split_once('/') {
        if !provider.is_empty() && !model_id.is_empty() {
            return ws_models_select(provider, model_id);
        }
    }

    let mut config = match load_config(None) {
        Ok(c) => c,
        Err(e) => return format!("Failed to load config: {e}"),
    };
    let old = fmt_provider_model(&config);
    config.agent.model = name.to_string();

    if let Err(e) = save_config_to_default(&config) {
        return format!("Failed to save config: {e}");
    }

    format!("Model changed: {} → {}", old, fmt_provider_model(&config))
}

fn ws_settings_streaming_toggle() -> String {
    let mut config = match load_config(None) {
        Ok(c) => c,
        Err(e) => return format!("Failed to load config: {e}"),
    };

    config.agent.streaming = !config.agent.streaming;

    if let Err(e) = save_config_to_default(&config) {
        return format!("Failed to save config: {e}");
    }

    format!(
        "Streaming: {}",
        if config.agent.streaming { "on" } else { "off" }
    )
}

fn ws_settings_gateway() -> String {
    let config = match load_config(None) {
        Ok(c) => c,
        Err(e) => return format!("Failed to load config: {e}"),
    };

    let mut out = String::new();
    let _ = writeln!(out, "Gateway settings");
    let _ = writeln!(out, "API host: {}", config.api.host);
    let _ = writeln!(out, "API port: {}", config.api.port);
    let _ = writeln!(
        out,
        "CORS origins: {}",
        if config.api.allowed_origins.is_empty() {
            "(none)".to_string()
        } else {
            config.api.allowed_origins.join(", ")
        }
    );
    let _ = writeln!(out, "Max body size: {} bytes", config.api.max_body_size);
    let _ = writeln!(
        out,
        "WebSocket: {}",
        if let Some(ref ws) = config.channels.websocket {
            format!("{} (enabled: {})", ws.listen_addr, ws.enabled)
        } else {
            "not configured".to_string()
        }
    );
    out
}

/// Timeout field names that can be set via /settings timeout <key> <secs>.
const TIMEOUT_FIELDS: &[&str] = &[
    "tool_timeout",
    "connect_timeout",
    "first_byte_timeout",
    "idle_timeout",
    "message_timeout",
];

fn ws_settings_timeout(arg: &str) -> String {
    if arg.is_empty() {
        let config = match load_config(None) {
            Ok(c) => c,
            Err(e) => return format!("Failed to load config: {e}"),
        };
        let mut out = String::new();
        let _ = writeln!(out, "Timeout settings (seconds)");
        let _ = writeln!(out, "tool_timeout: {}", config.agent.tool_timeout);
        let _ = writeln!(out, "connect_timeout: {}", config.agent.connect_timeout);
        let _ = writeln!(
            out,
            "first_byte_timeout: {}",
            config.agent.first_byte_timeout
        );
        let _ = writeln!(out, "idle_timeout: {}", config.agent.idle_timeout);
        let _ = writeln!(out, "message_timeout: {}", config.agent.message_timeout);
        let _ = writeln!(out, "\nUsage: /settings timeout <key> <secs>");
        let _ = writeln!(out, "Keys: {}", TIMEOUT_FIELDS.join(", "));
        return out;
    }

    let mut parts = arg.splitn(2, char::is_whitespace);
    let key = parts.next().unwrap_or_default().to_ascii_lowercase();
    let value_str = parts.next().unwrap_or("").trim();

    if !TIMEOUT_FIELDS.contains(&key.as_str()) {
        return format!(
            "Unknown timeout key '{}'. Valid keys: {}",
            key,
            TIMEOUT_FIELDS.join(", ")
        );
    }

    let secs: u64 = match value_str.parse() {
        Ok(v) => v,
        Err(_) => {
            return format!(
                "Invalid value '{}'. Must be a number in seconds.",
                value_str
            )
        }
    };

    let mut config = match load_config(None) {
        Ok(c) => c,
        Err(e) => return format!("Failed to load config: {e}"),
    };

    let old = match key.as_str() {
        "tool_timeout" => {
            let old = config.agent.tool_timeout;
            config.agent.tool_timeout = secs;
            old
        }
        "connect_timeout" => {
            let old = config.agent.connect_timeout;
            config.agent.connect_timeout = secs;
            old
        }
        "first_byte_timeout" => {
            let old = config.agent.first_byte_timeout;
            config.agent.first_byte_timeout = secs;
            old
        }
        "idle_timeout" => {
            let old = config.agent.idle_timeout;
            config.agent.idle_timeout = secs;
            old
        }
        "message_timeout" => {
            let old = config.agent.message_timeout;
            config.agent.message_timeout = secs;
            old
        }
        _ => unreachable!(),
    };

    if let Err(e) = save_config_to_default(&config) {
        return format!("Failed to save config: {e}");
    }

    format!("{}: {}s → {}s", key, old, secs)
}

fn ws_settings_retry() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Retry policy (built-in defaults)");
    let _ = writeln!(out, "max_retries: 3");
    let _ = writeln!(out, "base_delay: 500ms");
    let _ = writeln!(out, "max_delay: 60s");
    let _ = writeln!(out, "jitter: true");
    let _ = writeln!(out, "retryable_codes: 429, 500, 502, 503");
    let _ = writeln!(out, "max_retries_503: 5");
    let _ = writeln!(out, "max_delay_503: 30s");
    let _ = writeln!(
        out,
        "\nRetry is configured per-provider with circuit breaking."
    );
    out
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
    settings.push(format!(
        "Provider: {}",
        config.agent.provider.as_deref().unwrap_or("(auto)")
    ));
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
    if config.providers.opencode_go.is_some() {
        providers.push("opencode_go");
    }
    if config.providers.glm_coding_plan.is_some() {
        providers.push("glm_coding_plan");
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
                 Create one at ~/.kestrel/config.toml or run `kestrel setup`."
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
    if config.providers.opencode_go.is_some() {
        providers.push("opencode_go");
    }
    if config.providers.glm_coding_plan.is_some() {
        providers.push("glm_coding_plan");
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
    let _ = writeln!(out, "Agent: {} ({})", fmt_provider_model(&config), name);

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
    if let Some(ref fs) = config.channels.feishu {
        let state = if fs.enabled {
            if std::env::var("FEISHU_APP_ID").is_ok() && std::env::var("FEISHU_APP_SECRET").is_ok()
            {
                "connected"
            } else {
                "configured (no credentials)"
            }
        } else {
            "disabled"
        };
        channels.push(format!("feishu: {}", state));
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
    if let Some(ref p) = config.providers.opencode_go {
        providers.push(format_key_status("opencode_go", p.api_key.as_deref()));
    }
    if let Some(ref p) = config.providers.glm_coding_plan {
        providers.push(format_key_status("glm_coding_plan", p.api_key.as_deref()));
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
// /models implementation (two-level provider → model selection)
// ---------------------------------------------------------------------------

/// Show the provider list as an inline keyboard.
///
/// Level 1: User picks a provider to see its models.
pub async fn handle_models_provider_list() -> CommandResponse {
    let config = match load_config(None) {
        Ok(c) => c,
        Err(e) => return CommandResponse::text(format!("Failed to load config: {e}")),
    };

    let current = fmt_provider_model(&config);

    let catalog = get_model_catalog().await;
    let models = catalog.list_all_models().await;

    if models.is_empty() {
        return CommandResponse::text(
            "No models discovered. Configure a provider (e.g. opencode_go) in config.toml.",
        );
    }

    // Group models by provider and count.
    let mut provider_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for m in &models {
        *provider_counts.entry(m.provider.clone()).or_insert(0) += 1;
    }

    let mut out = String::new();
    let _ = writeln!(out, "Select a provider:");
    let _ = writeln!(out, "Current: {}", current);

    let mut kb = InlineKeyboardBuilder::new();
    for (provider, count) in &provider_counts {
        let label = format!("{} ({} models)", provider, count);
        let data = format!("models:provider:{}", provider);
        kb = kb.button(&label, &data);
        kb = kb.new_row();
    }
    kb = kb.button("Refresh", "models:refresh");

    CommandResponse::with_keyboard(out, kb.build())
}

/// Models per page for the provider detail keyboard.
const MODELS_PER_PAGE: usize = 15;

/// Show models for a specific provider as an inline keyboard (paginated).
///
/// Level 2: User picks a model to set as active.
pub async fn handle_models_provider_detail(provider: &str, page: usize) -> CommandResponse {
    let config = match load_config(None) {
        Ok(c) => c,
        Err(e) => return CommandResponse::text(format!("Failed to load config: {e}")),
    };

    let current = fmt_provider_model(&config);

    let catalog = get_model_catalog().await;
    let models = catalog.list_provider_models(provider).await;

    if models.is_empty() {
        return CommandResponse::text(format!("No models found for provider: {}", provider));
    }

    let total = models.len();
    let total_pages = total.div_ceil(MODELS_PER_PAGE);
    let page = page.min(total_pages.saturating_sub(1));
    let start = page * MODELS_PER_PAGE;
    let end = (start + MODELS_PER_PAGE).min(total);
    let page_models = &models[start..end];

    let mut out = String::new();
    let _ = writeln!(out, "[{}] models ({} total):", provider, total);
    let _ = writeln!(out, "Current: {}", current);
    if total_pages > 1 {
        let _ = writeln!(out, "Page {} of {}", page + 1, total_pages);
    }

    let mut kb = InlineKeyboardBuilder::new();
    for m in page_models {
        let marker =
            if m.id == config.agent.model && config.agent.provider.as_deref() == Some(provider) {
                " *"
            } else {
                ""
            };
        let ctx = m
            .context_length
            .map(|c| format!(" ({}K)", c / 1024))
            .unwrap_or_default();
        let label = format!("{}{}{}", m.id, ctx, marker);
        let data = format!("models:select:{}/{}", m.provider, m.id);
        kb = kb.button(&label, &data);
        kb = kb.new_row();
    }

    // Navigation row: pagination | back
    if total_pages > 1 {
        let nav_row = InlineKeyboardBuilder::pagination_row(page, total_pages, |p| {
            format!("models:ppage:{}|{}", provider, p)
        });
        kb = kb.push_row(nav_row);
    }

    kb = kb.button("<< Back to providers", "models:providers");

    CommandResponse::with_keyboard(out, kb.build())
}

/// Select a model (provider/model_id format) and persist to config.
pub fn handle_models_select(qualified_id: &str) -> CommandResponse {
    // Split "provider/model_id" into separate fields.
    let (provider, model_id) = match qualified_id.split_once('/') {
        Some((p, m)) if !p.is_empty() && !m.is_empty() => (p.to_string(), m.to_string()),
        _ => {
            return CommandResponse::text(format!(
                "Invalid model format: '{}'. Expected provider/model_id",
                qualified_id
            ))
        }
    };

    let mut config = match load_config(None) {
        Ok(c) => c,
        Err(e) => return CommandResponse::text(format!("Failed to load config: {e}")),
    };

    let old = fmt_provider_model(&config);
    config.agent.provider = Some(provider.clone());
    config.agent.model = model_id.clone();

    if let Err(e) = save_config_to_default(&config) {
        return CommandResponse::text(format!("Failed to save config: {e}"));
    }

    let mut out = String::new();
    let _ = writeln!(out, "Model changed:");
    let _ = writeln!(out, "  {} -> {}/{}", old, provider, model_id);

    CommandResponse::text(out)
}

/// Handle a callback from the /models inline keyboard.
pub fn handle_models_callback(action: &str, payload: Option<&str>) -> Option<CommandResponse> {
    match action {
        "providers" => {
            // Synchronous wrapper — we need to block on async here.
            // Use tokio::task::block_in_place for a minimal synchronous path.
            // Actually, let's just show the provider list synchronously from cached data.
            // The async version is called from the router.
            None // Handled by the async router
        }
        "provider" => {
            // Handled by async router
            None
        }
        "select" => {
            let qualified_id = payload?;
            Some(handle_models_select(qualified_id))
        }
        "refresh" => {
            // Handled by async router
            None
        }
        _ => None,
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
            "show" => Some(handle_settings()),
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

    /// Helper: create a temp dir with a config.toml and set KESTREL_HOME.
    struct TestHome {
        _dir: tempfile::TempDir,
        _env: EnvVarGuard,
    }

    fn with_temp_config(toml_str: &str) -> TestHome {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let mut f = std::fs::File::create(&config_path).unwrap();
        f.write_all(toml_str.as_bytes()).unwrap();
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
        let toml_str = r#"
[providers.openai]
api_key = "sk-test123"

[channels.telegram]
token = "123456:ABC"
"#;
        let _dir = with_temp_config(toml_str);
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
        let toml_str = r#"
[agent]
model = ""
"#;
        let _dir = with_temp_config(toml_str);
        let result = handle_validate();
        assert!(result.contains("error(s)") || result.contains("[ERROR]"));
    }

    #[test]
    fn test_handle_validate_summary_shows_name() {
        let toml_str = r#"
name = "testbot"

[providers.openai]
api_key = "sk-test"
"#;
        let _dir = with_temp_config(toml_str);
        let result = handle_validate();
        assert!(result.contains("Name: testbot"));
    }

    #[test]
    fn test_handle_validate_summary_shows_unnamed() {
        let toml_str = r#"
[providers.openai]
api_key = "sk-test"
"#;
        let _dir = with_temp_config(toml_str);
        let result = handle_validate();
        assert!(result.contains("Name: unnamed"));
    }

    #[test]
    fn test_handle_validate_summary_shows_providers() {
        let toml_str = r#"
[providers.openai]
api_key = "sk-test"

[providers.anthropic]
api_key = "sk-ant-test"
"#;
        let _dir = with_temp_config(toml_str);
        let result = handle_validate();
        assert!(result.contains("openai"));
        assert!(result.contains("anthropic"));
    }

    #[test]
    fn test_handle_validate_summary_shows_channels() {
        let toml_str = r#"
[channels.telegram]
token = "123:ABC"

[channels.discord]
token = "discord-token"
"#;
        let _dir = with_temp_config(toml_str);
        let result = handle_validate();
        assert!(result.contains("telegram (enabled)"));
        assert!(result.contains("discord (enabled)"));
    }

    #[test]
    fn test_handle_validate_summary_channels_disabled() {
        let toml_str = r#"
[channels.telegram]
token = "123:ABC"
enabled = false
"#;
        let _dir = with_temp_config(toml_str);
        let result = handle_validate();
        assert!(result.contains("telegram (disabled)"));
    }

    #[test]
    fn test_handle_validate_no_providers() {
        let toml_str = r#"
# empty config
"#;
        let _dir = with_temp_config(toml_str);
        let result = handle_validate();
        assert!(result.contains("Providers: (none)") || result.contains("error"));
    }

    #[test]
    fn test_handle_validate_no_channels() {
        let toml_str = r#"
[providers.openai]
api_key = "sk-test"
"#;
        let _dir = with_temp_config(toml_str);
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
        let toml_str = r#"
[providers.openai]
api_key = ""
"#;
        let _dir = with_temp_config(toml_str);
        let result = handle_status();
        assert!(result.contains("openai: no key"));
    }

    #[test]
    fn test_handle_status_heartbeat_disabled() {
        let toml_str = r#"
[heartbeat]
enabled = false
"#;
        let _dir = with_temp_config(toml_str);
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
        let _dir = with_temp_config("[providers.openai]\napi_key = \"sk-test\"\n");
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
        let _dir = with_temp_config("[providers.openai]\napi_key = \"sk-test\"\n");
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
        let toml_str = r#"
[providers.openai]
api_key = "sk-test"
"#;
        let _dir = with_temp_config(toml_str);
        let r = handle_settings();
        assert!(r.text.contains("Settings"));
        assert!(r.text.contains("Model:"));
        assert!(r.keyboard.is_some());
    }

    #[tokio::test]
    async fn test_try_handle_command_settings() {
        let toml_str = r#"
[providers.openai]
api_key = "sk-test"
"#;
        let _dir = with_temp_config(toml_str);
        let r = expect_response(try_handle_command("/settings").await.unwrap());
        assert!(r.text.contains("Settings"));
        assert!(r.keyboard.is_some());
    }

    #[test]
    fn test_settings_keyboard_buttons() {
        let toml_str = r#"
[providers.openai]
api_key = "sk-test"
"#;
        let _dir = with_temp_config(toml_str);
        let r = handle_settings();
        let kb = r.keyboard.unwrap();
        // Should have the row with models picker + streaming toggle.
        assert!(!kb.inline_keyboard.is_empty());
        let row = &kb.inline_keyboard[0];
        assert_eq!(row.len(), 2);
        assert!(row[0].callback_data.as_ref().unwrap().contains("models"));
        assert!(row[1].callback_data.as_ref().unwrap().contains("streaming"));
    }

    // -- /settings tests (paginated view) -------------------------------------

    #[test]
    fn test_handle_settings_single_page() {
        let toml_str = r#"
[providers.openai]
api_key = "sk-test"
"#;
        let _dir = with_temp_config(toml_str);
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
        let toml_str = r#"
name = "mybot"

[providers.openai]
api_key = "sk-test"
"#;
        let _dir = with_temp_config(toml_str);
        let resp = handle_settings_callback(0);
        assert!(resp.text.contains("mybot"));
    }

    #[test]
    fn test_handle_settings_shows_providers() {
        let toml_str = r#"
[providers.openai]
api_key = "sk-test"

[providers.anthropic]
api_key = "sk-ant-test"
"#;
        let _dir = with_temp_config(toml_str);
        // Providers are on page 1 (index 5+ out of 7 settings with page size 5).
        let resp = handle_settings_callback(1);
        assert!(resp.text.contains("openai"));
        assert!(resp.text.contains("anthropic"));
    }

    #[test]
    fn test_handle_settings_no_providers() {
        let toml_str = "# empty\n";
        let _dir = with_temp_config(toml_str);
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

        assert!(rewritten.contains("The user explicitly invoked the /plan_release skill"));
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
            .any(|(command, _)| command == "release_plan"));
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
        let toml_str = r#"
[agent]
model = "gpt-4o"
streaming = true
"#;
        let _env = EnvVarGuard::set("KESTREL_HOME", dir.path());
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, toml_str).unwrap();

        let resp = handle_callback("settings:model:switch").unwrap();
        assert!(resp.text.contains("Model:"));
        assert!(resp.keyboard.is_some());
        assert!(!resp.text.contains("gpt-4o") || MODEL_CYCLE.len() == 1);
    }

    #[test]
    fn test_handle_callback_settings_streaming_toggle() {
        let dir = tempfile::tempdir().unwrap();
        let toml_str = r#"
[agent]
model = "gpt-4o"
streaming = true
"#;
        let _env = EnvVarGuard::set("KESTREL_HOME", dir.path());
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, toml_str).unwrap();

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

    // -- /settings text-based (WebSocket) tests ---------------------------------

    #[tokio::test]
    async fn test_ws_settings_shows_current() {
        let toml_str = r#"
[providers.openai]
api_key = "sk-test"
"#;
        let _dir = with_temp_config(toml_str);
        let result = handle_ws_settings("/settings").await;
        assert!(result.contains("Settings"));
        assert!(result.contains("Model:"));
        assert!(result.contains("Streaming:"));
        assert!(result.contains("/settings model"));
        assert!(result.contains("/settings streaming"));
        assert!(result.contains("/settings models"));
    }

    #[tokio::test]
    async fn test_ws_settings_model_show_current() {
        let toml_str = r#"
[agent]
model = "gpt-4o"
"#;
        let _dir = with_temp_config(toml_str);
        let result = handle_ws_settings("/settings model").await;
        assert!(result.contains("gpt-4o"));
    }

    #[tokio::test]
    async fn test_ws_settings_model_next_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let toml_str = r#"
[agent]
model = "gpt-4o"
"#;
        let _env = EnvVarGuard::set("KESTREL_HOME", dir.path());
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, toml_str).unwrap();

        let result = handle_ws_settings("/settings model next").await;
        assert!(result.contains("Model switched to:"));
        assert!(!result.contains("gpt-4o") || MODEL_CYCLE.len() == 1);
    }

    #[tokio::test]
    async fn test_ws_settings_model_set_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let toml_str = r#"
[agent]
model = "gpt-4o"
"#;
        let _env = EnvVarGuard::set("KESTREL_HOME", dir.path());
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, toml_str).unwrap();

        let result = handle_ws_settings("/settings model my-custom-model").await;
        assert!(result.contains("gpt-4o"));
        assert!(result.contains("my-custom-model"));
    }

    #[tokio::test]
    async fn test_ws_settings_streaming_toggle() {
        let dir = tempfile::tempdir().unwrap();
        let toml_str = r#"
[agent]
model = "gpt-4o"
streaming = true
"#;
        let _env = EnvVarGuard::set("KESTREL_HOME", dir.path());
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, toml_str).unwrap();

        let result = handle_ws_settings("/settings streaming").await;
        assert!(result.contains("Streaming: off"));

        let result2 = handle_ws_settings("/settings streaming").await;
        assert!(result2.contains("Streaming: on"));
    }

    #[tokio::test]
    async fn test_ws_settings_unknown_subcommand() {
        let result = handle_ws_settings("/settings foobar").await;
        assert!(result.contains("Usage"));
    }

    // -- /settings gateway tests -----------------------------------------------

    #[test]
    fn test_ws_settings_gateway_shows_config() {
        let toml_str = r#"
[api]
host = "0.0.0.0"
port = 9090

[channels.websocket]
enabled = true
listen_addr = "0.0.0.0:9091"
"#;
        let _dir = with_temp_config(toml_str);
        let result = ws_settings_gateway();
        assert!(result.contains("Gateway settings"));
        assert!(result.contains("API host: 0.0.0.0"));
        assert!(result.contains("API port: 9090"));
        assert!(result.contains("0.0.0.0:9091"));
    }

    // -- /settings timeout tests -----------------------------------------------

    #[test]
    fn test_ws_settings_timeout_shows_all() {
        let _dir = with_empty_home();
        let result = ws_settings_timeout("");
        assert!(result.contains("Timeout settings"));
        assert!(result.contains("tool_timeout:"));
        assert!(result.contains("connect_timeout:"));
        assert!(result.contains("first_byte_timeout:"));
        assert!(result.contains("idle_timeout:"));
        assert!(result.contains("message_timeout:"));
    }

    #[test]
    fn test_ws_settings_timeout_set_valid() {
        let dir = tempfile::tempdir().unwrap();
        let toml_str = r#"
[agent]
model = "gpt-4o"
tool_timeout = 60
"#;
        let _env = EnvVarGuard::set("KESTREL_HOME", dir.path());
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, toml_str).unwrap();

        let result = ws_settings_timeout("tool_timeout 300");
        assert!(result.contains("tool_timeout: 60s → 300s"));
    }

    #[test]
    fn test_ws_settings_timeout_invalid_key() {
        let result = ws_settings_timeout("bogus 99");
        assert!(result.contains("Unknown timeout key"));
    }

    #[test]
    fn test_ws_settings_timeout_invalid_value() {
        let result = ws_settings_timeout("tool_timeout abc");
        assert!(result.contains("Invalid value"));
    }

    // -- /settings retry tests -------------------------------------------------

    #[test]
    fn test_ws_settings_retry_shows_defaults() {
        let result = ws_settings_retry();
        assert!(result.contains("Retry policy"));
        assert!(result.contains("max_retries: 3"));
        assert!(result.contains("jitter: true"));
    }

    #[tokio::test]
    async fn test_ws_settings_models_empty() {
        let _dir = with_empty_home();
        let result = handle_ws_settings("/settings models").await;
        assert!(result.contains("No models discovered"));
    }

    // -- /models tests (Telegram two-level selection) -------------------------

    #[tokio::test]
    async fn test_handle_models_provider_list_no_models() {
        let _dir = with_empty_home();
        let resp = handle_models_provider_list().await;
        assert!(resp.text.contains("No models discovered"));
        assert!(resp.keyboard.is_none());
    }

    #[test]
    fn test_handle_models_select_sets_model() {
        let dir = tempfile::tempdir().unwrap();
        let toml_str = r#"
[agent]
model = "gpt-4o"
"#;
        let _env = EnvVarGuard::set("KESTREL_HOME", dir.path());
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, toml_str).unwrap();

        let resp = handle_models_select("opencode_go/kimi-k2.6");
        assert!(resp.text.contains("Model changed"));
        assert!(resp.text.contains("gpt-4o"));
        assert!(resp.text.contains("opencode_go"));
        assert!(resp.text.contains("kimi-k2.6"));
        assert!(resp.keyboard.is_none());
    }

    #[test]
    fn test_handle_models_select_invalid_config() {
        // No KESTREL_HOME set — should handle gracefully.
        let resp = handle_models_select("test/model");
        assert!(resp.text.contains("Failed to") || resp.text.contains("Model changed"));
    }

    #[test]
    fn test_handle_models_callback_select() {
        let dir = tempfile::tempdir().unwrap();
        let toml_str = r#"
[agent]
model = "gpt-4o"
"#;
        let _env = EnvVarGuard::set("KESTREL_HOME", dir.path());
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, toml_str).unwrap();

        let resp = handle_models_callback("select", Some("openai/gpt-4o-mini")).unwrap();
        assert!(resp.text.contains("Model changed"));
    }

    #[test]
    fn test_handle_models_callback_unknown_action() {
        assert!(handle_models_callback("unknown", None).is_none());
    }

    #[test]
    fn test_handle_models_callback_select_no_payload() {
        assert!(handle_models_callback("select", None).is_none());
    }

    #[tokio::test]
    async fn test_try_handle_command_models() {
        let _dir = with_empty_home();
        let result = try_handle_command("/models").await;
        assert!(result.is_some());
        let resp = expect_response(result.unwrap());
        assert!(resp.text.contains("No models discovered"));
    }
}
