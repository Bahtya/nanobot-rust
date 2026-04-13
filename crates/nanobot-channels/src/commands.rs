//! Built-in slash command handlers.
//!
//! Provides channel-level command handling that runs *before* messages are
//! forwarded to the message bus.  This means commands like `/validate` and
//! `/status` work even when the LLM provider is misconfigured.

use nanobot_config::validate::ValidationFinding;
use nanobot_config::{load_config, validate, Config};
use nanobot_session::SessionManager;
use std::fmt::Write;

use crate::platforms::telegram::{InlineKeyboardBuilder, InlineKeyboardMarkup};

/// Model names available for cycling via /settings.
const MODEL_CYCLE: &[&str] = &["gpt-4o", "claude-sonnet-4-6", "deepseek-chat"];

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

/// Try to handle a built-in command.
///
/// If `text` matches a known built-in command, returns `Some(response)`.
/// Otherwise returns `None`, signalling the caller to forward the message
/// through the normal bus path.
pub fn try_handle_command(text: &str) -> Option<CommandResponse> {
    if matches_command(text, "help") {
        Some(CommandResponse::text(handle_help()))
    } else if matches_command(text, "status") {
        Some(CommandResponse::text(handle_status()))
    } else if matches_command(text, "validate") {
        Some(CommandResponse::text(handle_validate()))
    } else if matches_command(text, "settings") {
        Some(handle_settings())
    } else if matches_command(text, "history") {
        Some(handle_history_page(0))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// /help implementation
// ---------------------------------------------------------------------------

/// Return a formatted help text listing all available commands.
fn handle_help() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Available commands:\n");
    let _ = writeln!(out, "/help     - Show this help message");
    let _ = writeln!(out, "/status   - Show bot status, channels, and config summary");
    let _ = writeln!(out, "/validate - Validate config.yaml and show results");
    let _ = writeln!(out, "/settings - Toggle preferences (notifications, model)");
    let _ = writeln!(out, "/history  - Browse recent conversation history");
    let _ = writeln!(out, "/reset    - Reset conversation context for this chat");
    out
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
                 Create one at ~/.nanobot-rs/config.yaml or run `nanobot-rs setup`."
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
    let home = match nanobot_config::paths::get_nanobot_home() {
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
// /settings implementation
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

// ---------------------------------------------------------------------------
// /history implementation
// ---------------------------------------------------------------------------

/// Messages shown per page.
const HISTORY_PAGE_SIZE: usize = 5;

/// Render a page of recent conversation history.
///
/// `page` is zero-indexed.  Returns a `CommandResponse` with a pagination
/// keyboard when there are multiple pages.
///
/// If `data_dir` is `None`, uses `~/.nanobot-rs/data` (derived from config).
pub fn handle_history_page(page: usize) -> CommandResponse {
    let home = match nanobot_config::paths::get_nanobot_home() {
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
    let mut all_entries: Vec<(String, nanobot_session::SessionEntry)> = Vec::new();
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
            nanobot_core::MessageRole::User => "You",
            nanobot_core::MessageRole::Assistant => "Bot",
            nanobot_core::MessageRole::System => "Sys",
            nanobot_core::MessageRole::Tool => "Tool",
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
    let path = nanobot_config::paths::get_config_path().map_err(|e| e.to_string())?;
    nanobot_config::loader::save_config(config, &path).map_err(|e| e.to_string())
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

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
        assert!(!matches_command("/start", "validate"));
        assert!(!matches_command("validate", "validate"));
        assert!(!matches_command("", "validate"));
        assert!(!matches_command("hello world", "validate"));
    }

    // -- try_handle_command tests --------------------------------------------

    #[test]
    fn test_try_handle_command_validate() {
        let result = try_handle_command("/validate").unwrap();
        assert!(result.text.contains("Configuration"));
        assert!(result.keyboard.is_none());
    }

    #[test]
    fn test_try_handle_command_other() {
        assert!(try_handle_command("/unknown_cmd").is_none());
        assert!(try_handle_command("hello").is_none());
        assert!(try_handle_command("").is_none());
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

    fn with_temp_config(yaml: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        let mut f = std::fs::File::create(&config_path).unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        std::env::set_var("NANOBOT_RS_HOME", dir.path());
        dir
    }

    fn with_empty_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("NANOBOT_RS_HOME", dir.path());
        dir
    }

    // -- handle_validate tests -----------------------------------------------

    #[test]
    fn test_handle_validate_default_config() {
        let _dir = with_empty_home();
        let result = handle_validate();
        assert!(result.contains("Configuration"));
        assert!(result.contains("Agent:"));
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
        assert!(result.contains("openai"));
    }

    #[test]
    fn test_handle_validate_invalid_config() {
        let yaml = r#"
agent:
  model: ""
"#;
        let _dir = with_temp_config(yaml);
        let result = handle_validate();
        assert!(result.contains("error(s)") || result.contains("[ERROR]"));
    }

    // -- /help tests ---------------------------------------------------------

    #[test]
    fn test_handle_help_lists_commands() {
        let result = handle_help();
        assert!(result.contains("/help"));
        assert!(result.contains("/status"));
        assert!(result.contains("/validate"));
        assert!(result.contains("/settings"));
        assert!(result.contains("/history"));
    }

    #[test]
    fn test_try_handle_command_help() {
        let r = try_handle_command("/help").unwrap();
        assert!(r.text.contains("/help"));
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

    // -- /settings tests -----------------------------------------------------

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

    #[test]
    fn test_try_handle_command_settings() {
        let yaml = r#"
providers:
  openai:
    api_key: "sk-test"
"#;
        let _dir = with_temp_config(yaml);
        let r = try_handle_command("/settings").unwrap();
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

    // -- /history tests ------------------------------------------------------

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

    // -- utility tests -------------------------------------------------------

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
        assert!(result.len() <= 14); // 11 + "..."
    }

    #[test]
    fn test_truncate_str_multibyte() {
        // Don't panic on multi-byte UTF-8.
        let result = truncate_str("日本語テストです", 6);
        // Should not panic, result is some valid string.
        assert!(!result.is_empty());
    }
}
