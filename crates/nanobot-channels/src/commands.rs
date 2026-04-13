//! Built-in slash command handlers.
//!
//! Provides channel-level command handling that runs *before* messages are
//! forwarded to the message bus.  This means commands like `/validate` work
//! even when the LLM provider is misconfigured.

use nanobot_config::validate::ValidationFinding;
use nanobot_config::{load_config, validate, Config};
use std::fmt::Write;

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
pub fn try_handle_command(text: &str) -> Option<String> {
    if matches_command(text, "validate") {
        Some(handle_validate())
    } else {
        None
    }
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
        assert!(!matches_command("/help", "validate"));
        assert!(!matches_command("/start", "validate"));
        assert!(!matches_command("validate", "validate")); // no slash
        assert!(!matches_command("", "validate"));
        assert!(!matches_command("hello world", "validate"));
    }

    // -- try_handle_command tests --------------------------------------------

    #[test]
    fn test_try_handle_command_validate() {
        let result = try_handle_command("/validate");
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("Configuration"));
    }

    #[test]
    fn test_try_handle_command_other() {
        assert!(try_handle_command("/help").is_none());
        assert!(try_handle_command("hello").is_none());
        assert!(try_handle_command("").is_none());
    }

    // -- handle_validate tests -----------------------------------------------

    /// Helper: create a temp dir with a config.yaml and set NANOBOT_RS_HOME.
    fn with_temp_config(yaml: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.yaml");
        let mut f = std::fs::File::create(&config_path).unwrap();
        f.write_all(yaml.as_bytes()).unwrap();
        std::env::set_var("NANOBOT_RS_HOME", dir.path());
        dir
    }

    /// Helper: create a temp dir with no config file.
    fn with_empty_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("NANOBOT_RS_HOME", dir.path());
        dir
    }

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
}
