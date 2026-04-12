//! Configuration validation — schema checks for config.yaml.
//!
//! Validates field types, required fields, value ranges, and cross-field
//! constraints. Returns a structured [`ValidationReport`] with warnings
//! and errors.

use crate::schema::{
    AgentDefaults, ChannelsConfig, Config, CustomProviderConfig, DiscordConfig,
    ProvidersConfig, TelegramConfig,
};
use std::fmt;

// ---------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------

/// Severity of a validation finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A problem that should be fixed but won't prevent startup.
    Warning,
    /// A critical problem that will prevent correct operation.
    Error,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Warning => write!(f, "WARNING"),
            Severity::Error => write!(f, "ERROR"),
        }
    }
}

/// A single validation finding.
#[derive(Debug, Clone)]
pub struct ValidationFinding {
    /// Severity level.
    pub severity: Severity,
    /// Dot-separated field path (e.g. `"providers.openai.api_key"`).
    pub path: String,
    /// Human-readable message.
    pub message: String,
}

impl fmt::Display for ValidationFinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}: {}", self.severity, self.path, self.message)
    }
}

/// Aggregated validation report.
#[derive(Debug, Clone, Default)]
pub struct ValidationReport {
    findings: Vec<ValidationFinding>,
}

impl ValidationReport {
    fn new() -> Self {
        Self { findings: Vec::new() }
    }

    fn error(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.findings.push(ValidationFinding {
            severity: Severity::Error,
            path: path.into(),
            message: message.into(),
        });
    }

    fn warning(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.findings.push(ValidationFinding {
            severity: Severity::Warning,
            path: path.into(),
            message: message.into(),
        });
    }

    /// All findings (errors and warnings).
    pub fn findings(&self) -> &[ValidationFinding] {
        &self.findings
    }

    /// Only error-level findings.
    pub fn errors(&self) -> Vec<&ValidationFinding> {
        self.findings.iter().filter(|f| f.severity == Severity::Error).collect()
    }

    /// Only warning-level findings.
    pub fn warnings(&self) -> Vec<&ValidationFinding> {
        self.findings.iter().filter(|f| f.severity == Severity::Warning).collect()
    }

    /// Whether there are no errors.
    pub fn is_valid(&self) -> bool {
        self.findings.iter().all(|f| f.severity != Severity::Error)
    }

    /// Total number of findings.
    pub fn len(&self) -> usize {
        self.findings.len()
    }

    /// Whether the report is empty.
    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }
}

impl fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for finding in &self.findings {
            writeln!(f, "  {}", finding)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Validate a [`Config`] and return a report.
pub fn validate(config: &Config) -> ValidationReport {
    let mut report = ValidationReport::new();

    validate_providers(&config.providers, &config.custom_providers, &mut report);
    validate_channels(&config.channels, &mut report);
    validate_agent(&config.agent, &mut report);

    report
}

// ---------------------------------------------------------------------------
// Provider validation
// ---------------------------------------------------------------------------

fn validate_providers(
    providers: &ProvidersConfig,
    custom: &[CustomProviderConfig],
    report: &mut ValidationReport,
) {
    let mut has_provider = false;

    // OpenAI
    if let Some(ref p) = providers.openai {
        has_provider = true;
        if p.api_key.as_deref().is_none_or(|k| k.is_empty()) {
            report.warning("providers.openai.api_key", "API key is empty or missing");
        } else if let Some(ref key) = p.api_key {
            validate_api_key_prefix(key, "providers.openai.api_key", "sk-", report);
        }
    }

    // Anthropic
    if let Some(ref p) = providers.anthropic {
        has_provider = true;
        if p.api_key.as_deref().is_none_or(|k| k.is_empty()) {
            report.warning("providers.anthropic.api_key", "API key is empty or missing");
        } else if let Some(ref key) = p.api_key {
            validate_api_key_prefix(key, "providers.anthropic.api_key", "sk-ant-", report);
        }
    }

    // OpenRouter
    if let Some(ref p) = providers.openrouter {
        has_provider = true;
        if p.api_key.as_deref().is_none_or(|k| k.is_empty()) {
            report.warning("providers.openrouter.api_key", "API key is empty or missing");
        }
    }

    // DeepSeek
    if let Some(ref p) = providers.deepseek {
        has_provider = true;
        if p.api_key.as_deref().is_none_or(|k| k.is_empty()) {
            report.warning("providers.deepseek.api_key", "API key is empty or missing");
        }
    }

    // Groq
    if let Some(ref p) = providers.groq {
        has_provider = true;
        if p.api_key.as_deref().is_none_or(|k| k.is_empty()) {
            report.warning("providers.groq.api_key", "API key is empty or missing");
        }
    }

    // Gemini
    if let Some(ref p) = providers.gemini {
        has_provider = true;
        if p.api_key.as_deref().is_none_or(|k| k.is_empty()) {
            report.warning("providers.gemini.api_key", "API key is empty or missing");
        }
    }

    // Ollama — no key needed, but check base_url
    if let Some(ref p) = providers.ollama {
        has_provider = true;
        if p.base_url.as_deref().is_none_or(|u| u.is_empty()) {
            report.warning("providers.ollama.base_url", "base_url is empty (default: http://localhost:11434)");
        }
    }

    // Moonshot, MiniMax, etc.
    if providers.moonshot.is_some() {
        has_provider = true;
    }
    if providers.minimax.is_some() {
        has_provider = true;
    }

    // Azure OpenAI
    if let Some(ref p) = providers.azure_openai {
        has_provider = true;
        if p.api_key.as_deref().is_none_or(|k| k.is_empty()) {
            report.warning("providers.azure_openai.api_key", "API key is empty or missing");
        }
        if p.endpoint.as_deref().is_none_or(|e| e.is_empty()) {
            report.error("providers.azure_openai.endpoint", "Azure endpoint is required");
        }
        if p.deployment.as_deref().is_none_or(|d| d.is_empty()) {
            report.error("providers.azure_openai.deployment", "Deployment name is required");
        }
    }

    // GitHub Copilot
    if providers.github_copilot.is_some() {
        has_provider = true;
    }
    // OpenAI Codex
    if providers.openai_codex.is_some() {
        has_provider = true;
    }

    // Custom providers
    for (i, cp) in custom.iter().enumerate() {
        has_provider = true;
        let prefix = format!("custom_providers[{i}]");
        if cp.name.is_empty() {
            report.error(format!("{prefix}.name"), "Custom provider name is empty");
        }
        if cp.base_url.is_empty() {
            report.error(format!("{prefix}.base_url"), "Custom provider base_url is empty");
        } else if !cp.base_url.starts_with("http://") && !cp.base_url.starts_with("https://") {
            report.error(format!("{prefix}.base_url"), "base_url must start with http:// or https://");
        }
    }

    if !has_provider {
        report.error("providers", "No LLM provider configured — at least one provider is required");
    }
}

fn validate_api_key_prefix(key: &str, path: &str, expected_prefix: &str, report: &mut ValidationReport) {
    if !key.starts_with(expected_prefix) {
        report.warning(
            path,
            format!("API key does not start with expected prefix '{}'", expected_prefix),
        );
    }
}

// ---------------------------------------------------------------------------
// Channel validation
// ---------------------------------------------------------------------------

fn validate_channels(channels: &ChannelsConfig, report: &mut ValidationReport) {
    let mut has_enabled_channel = false;

    if let Some(ref tg) = channels.telegram {
        if tg.enabled {
            has_enabled_channel = true;
            validate_telegram(tg, report);
        }
    }

    if let Some(ref dc) = channels.discord {
        if dc.enabled {
            has_enabled_channel = true;
            validate_discord(dc, report);
        }
    }

    // Slack
    if let Some(ref s) = channels.slack {
        if s.enabled {
            has_enabled_channel = true;
            if s.bot_token.as_deref().is_none_or(|t| t.is_empty()) {
                report.error("channels.slack.bot_token", "Slack bot_token is required when enabled");
            }
        }
    }

    // Matrix
    if let Some(ref m) = channels.matrix {
        if m.enabled {
            has_enabled_channel = true;
            if m.homeserver.as_deref().is_none_or(|h| h.is_empty()) {
                report.error("channels.matrix.homeserver", "Matrix homeserver URL is required when enabled");
            }
            if m.user_id.as_deref().is_none_or(|u| u.is_empty()) {
                report.error("channels.matrix.user_id", "Matrix user_id is required when enabled");
            }
            if m.access_token.is_none() && m.password.is_none() {
                report.error("channels.matrix", "Either access_token or password is required for Matrix");
            }
        }
    }

    // Email
    if let Some(ref e) = channels.email {
        if e.enabled {
            has_enabled_channel = true;
            if e.imap_host.as_deref().is_none_or(|h| h.is_empty()) {
                report.error("channels.email.imap_host", "IMAP host is required when email is enabled");
            }
            if e.smtp_host.as_deref().is_none_or(|h| h.is_empty()) {
                report.error("channels.email.smtp_host", "SMTP host is required when email is enabled");
            }
            if e.username.as_deref().is_none_or(|u| u.is_empty()) {
                report.error("channels.email.username", "Username is required when email is enabled");
            }
        }
    }

    // DingTalk
    if let Some(ref d) = channels.dingtalk {
        if d.enabled {
            has_enabled_channel = true;
            if d.webhook.as_deref().is_none_or(|w| w.is_empty()) {
                report.error("channels.dingtalk.webhook", "Webhook URL is required when DingTalk is enabled");
            }
        }
    }

    // Feishu
    if let Some(ref f) = channels.feishu {
        if f.enabled {
            has_enabled_channel = true;
            if f.app_id.as_deref().is_none_or(|a| a.is_empty()) {
                report.error("channels.feishu.app_id", "App ID is required when Feishu is enabled");
            }
            if f.app_secret.as_deref().is_none_or(|s| s.is_empty()) {
                report.error("channels.feishu.app_secret", "App secret is required when Feishu is enabled");
            }
        }
    }

    // WeCom
    if let Some(ref w) = channels.wecom {
        if w.enabled {
            has_enabled_channel = true;
            if w.corp_id.as_deref().is_none_or(|c| c.is_empty()) {
                report.error("channels.wecom.corp_id", "Corp ID is required when WeCom is enabled");
            }
            if w.secret.as_deref().is_none_or(|s| s.is_empty()) {
                report.error("channels.wecom.secret", "Secret is required when WeCom is enabled");
            }
        }
    }

    // Weixin
    if let Some(ref w) = channels.weixin {
        if w.enabled {
            has_enabled_channel = true;
            if w.app_id.as_deref().is_none_or(|a| a.is_empty()) {
                report.error("channels.weixin.app_id", "App ID is required when WeChat is enabled");
            }
        }
    }

    // QQ
    if let Some(ref q) = channels.qq {
        if q.enabled {
            has_enabled_channel = true;
            if q.app_id.as_deref().is_none_or(|a| a.is_empty()) {
                report.error("channels.qq.app_id", "App ID is required when QQ is enabled");
            }
        }
    }

    // Mochat
    if let Some(ref m) = channels.mochat {
        if m.enabled {
            has_enabled_channel = true;
            if m.webhook_url.as_deref().is_none_or(|u| u.is_empty()) {
                report.error("channels.mochat.webhook_url", "Webhook URL is required when Mochat is enabled");
            }
        }
    }

    // WhatsApp
    if channels.whatsapp.as_ref().is_some_and(|w| w.enabled) {
        has_enabled_channel = true;
        report.warning("channels.whatsapp", "WhatsApp channel is not yet implemented");
    }

    if !has_enabled_channel {
        report.warning("channels", "No channel is enabled — gateway will run in local-only mode");
    }
}

fn validate_telegram(tg: &TelegramConfig, report: &mut ValidationReport) {
    if tg.token.is_empty() {
        report.error("channels.telegram.token", "Bot token is required when Telegram is enabled");
    } else if !tg.token.contains(':') {
        report.warning("channels.telegram.token", "Token format looks invalid — expected '123456:ABC-DEF'");
    }
}

fn validate_discord(dc: &DiscordConfig, report: &mut ValidationReport) {
    if dc.token.is_empty() {
        report.error("channels.discord.token", "Bot token is required when Discord is enabled");
    } else if dc.token.len() < 20 {
        report.warning("channels.discord.token", "Token looks too short — expected a longer bot token");
    }
}

// ---------------------------------------------------------------------------
// Agent validation
// ---------------------------------------------------------------------------

fn validate_agent(agent: &AgentDefaults, report: &mut ValidationReport) {
    if agent.model.is_empty() {
        report.error("agent.model", "Model name cannot be empty");
    }

    if agent.temperature < 0.0 {
        report.error("agent.temperature", "Temperature must be >= 0.0");
    } else if agent.temperature > 2.0 {
        report.error("agent.temperature", "Temperature must be <= 2.0");
    }

    if agent.max_tokens == 0 {
        report.error("agent.max_tokens", "max_tokens must be > 0");
    } else if agent.max_tokens > 128_000 {
        report.warning("agent.max_tokens", "max_tokens > 128000 may not be supported by all models");
    }

    if agent.max_iterations == 0 {
        report.error("agent.max_iterations", "max_iterations must be > 0");
    }

    if agent.tool_timeout == 0 {
        report.warning("agent.tool_timeout", "tool_timeout of 0 means tools will timeout immediately");
    }

    if let Some(ref workspace) = agent.workspace {
        if workspace.is_empty() {
            report.warning("agent.workspace", "Workspace path is set but empty");
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::*;

    fn make_valid_config() -> Config {
        let mut config = Config::default();
        config.providers.openai = Some(ProviderEntry {
            api_key: Some("sk-test-key-12345".to_string()),
            base_url: None,
            model: Some("gpt-4o".to_string()),
            no_proxy: None,
        });
        config.channels.telegram = Some(TelegramConfig {
            token: "123456:ABC-DEF".to_string(),
            enabled: true,
            allowed_users: vec![],
            admin_users: vec![],
            streaming: false,
        });
        config.agent.model = "gpt-4o".to_string();
        config.agent.temperature = 0.7;
        config.agent.max_tokens = 4096;
        config.agent.max_iterations = 50;
        config
    }

    // -------------------------------------------------------------------
    // Report type tests
    // -------------------------------------------------------------------

    #[test]
    fn test_report_empty() {
        let report = ValidationReport::default();
        assert!(report.is_empty());
        assert!(report.is_valid());
        assert_eq!(report.len(), 0);
    }

    #[test]
    fn test_report_with_error() {
        let mut report = ValidationReport::new();
        report.error("test.path", "something broke");
        assert!(!report.is_valid());
        assert_eq!(report.errors().len(), 1);
        assert_eq!(report.warnings().len(), 0);
        assert_eq!(report.len(), 1);
    }

    #[test]
    fn test_report_with_warning() {
        let mut report = ValidationReport::new();
        report.warning("test.path", "be careful");
        assert!(report.is_valid()); // warnings don't invalidate
        assert_eq!(report.errors().len(), 0);
        assert_eq!(report.warnings().len(), 1);
    }

    #[test]
    fn test_severity_display() {
        assert_eq!(format!("{}", Severity::Error), "ERROR");
        assert_eq!(format!("{}", Severity::Warning), "WARNING");
    }

    #[test]
    fn test_finding_display() {
        let finding = ValidationFinding {
            severity: Severity::Error,
            path: "agent.model".to_string(),
            message: "cannot be empty".to_string(),
        };
        assert_eq!(format!("{}", finding), "[ERROR] agent.model: cannot be empty");
    }

    #[test]
    fn test_report_display() {
        let mut report = ValidationReport::new();
        report.error("a", "err");
        report.warning("b", "warn");
        let s = format!("{}", report);
        assert!(s.contains("[ERROR] a: err"));
        assert!(s.contains("[WARNING] b: warn"));
    }

    // -------------------------------------------------------------------
    // Full config validation tests
    // -------------------------------------------------------------------

    #[test]
    fn test_valid_config() {
        let config = make_valid_config();
        let report = validate(&config);
        assert!(report.is_valid(), "Expected valid config, got: {}", report);
    }

    // -------------------------------------------------------------------
    // Provider tests
    // -------------------------------------------------------------------

    #[test]
    fn test_no_provider_configured() {
        let config = Config::default();
        let report = validate(&config);
        assert!(!report.is_valid());
        let errs: Vec<&str> = report.errors().iter().map(|e| e.path.as_str()).collect();
        assert!(errs.contains(&"providers"), "Missing provider error");
    }

    #[test]
    fn test_openai_empty_key() {
        let mut config = make_valid_config();
        config.providers.openai = Some(ProviderEntry {
            api_key: Some(String::new()),
            base_url: None,
            model: None,
            no_proxy: None,
        });
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "providers.openai.api_key"));
    }

    #[test]
    fn test_openai_bad_prefix() {
        let mut config = make_valid_config();
        config.providers.openai = Some(ProviderEntry {
            api_key: Some("bad-key".to_string()),
            base_url: None,
            model: None,
            no_proxy: None,
        });
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "providers.openai.api_key" && w.message.contains("sk-")));
    }

    #[test]
    fn test_anthropic_bad_prefix() {
        let mut config = make_valid_config();
        config.providers.anthropic = Some(ProviderEntry {
            api_key: Some("sk-wrong-prefix".to_string()),
            base_url: None,
            model: None,
            no_proxy: None,
        });
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "providers.anthropic.api_key" && w.message.contains("sk-ant-")));
    }

    #[test]
    fn test_anthropic_valid_key() {
        let mut config = make_valid_config();
        config.providers.anthropic = Some(ProviderEntry {
            api_key: Some("sk-ant-api03-valid-key".to_string()),
            base_url: None,
            model: None,
            no_proxy: None,
        });
        let report = validate(&config);
        assert!(report.is_valid());
    }

    #[test]
    fn test_azure_openai_missing_fields() {
        let mut config = make_valid_config();
        config.providers.azure_openai = Some(AzureOpenAIProviderEntry {
            api_key: Some(String::new()),
            endpoint: None,
            deployment: None,
            api_version: None,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        let errs: Vec<&str> = report.errors().iter().map(|e| e.path.as_str()).collect();
        assert!(errs.contains(&"providers.azure_openai.endpoint"));
        assert!(errs.contains(&"providers.azure_openai.deployment"));
    }

    #[test]
    fn test_custom_provider_empty_name() {
        let mut config = make_valid_config();
        config.custom_providers.push(CustomProviderConfig {
            name: String::new(),
            base_url: "ftp://bad.proto".to_string(),
            api_key: None,
            model_patterns: vec![],
            no_proxy: None,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        let errs: Vec<&str> = report.errors().iter().map(|e| e.path.as_str()).collect();
        assert!(errs.contains(&"custom_providers[0].name"));
        assert!(errs.contains(&"custom_providers[0].base_url"));
    }

    #[test]
    fn test_custom_provider_valid() {
        let mut config = make_valid_config();
        config.custom_providers.push(CustomProviderConfig {
            name: "my_provider".to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            api_key: Some("key".to_string()),
            model_patterns: vec!["my-model".to_string()],
            no_proxy: None,
        });
        let report = validate(&config);
        assert!(report.is_valid());
    }

    #[test]
    fn test_ollama_no_base_url() {
        let mut config = make_valid_config();
        config.providers.ollama = Some(ProviderEntry {
            api_key: None,
            base_url: Some(String::new()),
            model: Some("llama3".to_string()),
            no_proxy: None,
        });
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "providers.ollama.base_url"));
    }

    // -------------------------------------------------------------------
    // Channel tests
    // -------------------------------------------------------------------

    #[test]
    fn test_no_channels_warning() {
        let mut config = make_valid_config();
        config.channels = ChannelsConfig::default();
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "channels"));
    }

    #[test]
    fn test_telegram_empty_token() {
        let mut config = make_valid_config();
        config.channels.telegram = Some(TelegramConfig {
            token: String::new(),
            enabled: true,
            allowed_users: vec![],
            admin_users: vec![],
            streaming: false,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report.errors().iter().any(|e| e.path == "channels.telegram.token"));
    }

    #[test]
    fn test_telegram_bad_token_format() {
        let mut config = make_valid_config();
        config.channels.telegram = Some(TelegramConfig {
            token: "no-colon-here".to_string(),
            enabled: true,
            allowed_users: vec![],
            admin_users: vec![],
            streaming: false,
        });
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "channels.telegram.token"));
    }

    #[test]
    fn test_telegram_disabled() {
        let mut config = make_valid_config();
        config.channels.telegram = Some(TelegramConfig {
            token: String::new(), // empty token but disabled — should be fine
            enabled: false,
            allowed_users: vec![],
            admin_users: vec![],
            streaming: false,
        });
        let report = validate(&config);
        // Should not error on token since disabled
        assert!(report.errors().iter().all(|e| e.path != "channels.telegram.token"));
    }

    #[test]
    fn test_discord_empty_token() {
        let mut config = make_valid_config();
        config.channels.discord = Some(DiscordConfig {
            token: String::new(),
            allowed_guilds: vec![],
            enabled: true,
            streaming: false,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report.errors().iter().any(|e| e.path == "channels.discord.token"));
    }

    #[test]
    fn test_discord_short_token() {
        let mut config = make_valid_config();
        config.channels.discord = Some(DiscordConfig {
            token: "short".to_string(),
            allowed_guilds: vec![],
            enabled: true,
            streaming: false,
        });
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "channels.discord.token"));
    }

    #[test]
    fn test_slack_missing_token() {
        let mut config = make_valid_config();
        config.channels.slack = Some(SlackConfig {
            bot_token: None,
            app_token: None,
            signing_secret: None,
            enabled: true,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report.errors().iter().any(|e| e.path == "channels.slack.bot_token"));
    }

    #[test]
    fn test_matrix_missing_fields() {
        let mut config = make_valid_config();
        config.channels.matrix = Some(MatrixConfig {
            homeserver: None,
            user_id: None,
            password: None,
            access_token: None,
            enabled: true,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        let errs: Vec<&str> = report.errors().iter().map(|e| e.path.as_str()).collect();
        assert!(errs.contains(&"channels.matrix.homeserver"));
        assert!(errs.contains(&"channels.matrix.user_id"));
    }

    #[test]
    fn test_email_missing_fields() {
        let mut config = make_valid_config();
        config.channels.email = Some(EmailConfig {
            imap_host: None,
            smtp_host: None,
            username: None,
            password: None,
            port: 0,
            enabled: true,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        let errs: Vec<&str> = report.errors().iter().map(|e| e.path.as_str()).collect();
        assert!(errs.contains(&"channels.email.imap_host"));
        assert!(errs.contains(&"channels.email.smtp_host"));
        assert!(errs.contains(&"channels.email.username"));
    }

    // -------------------------------------------------------------------
    // Agent tests
    // -------------------------------------------------------------------

    #[test]
    fn test_agent_empty_model() {
        let mut config = make_valid_config();
        config.agent.model = String::new();
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report.errors().iter().any(|e| e.path == "agent.model"));
    }

    #[test]
    fn test_agent_temperature_negative() {
        let mut config = make_valid_config();
        config.agent.temperature = -0.5;
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report.errors().iter().any(|e| e.path == "agent.temperature"));
    }

    #[test]
    fn test_agent_temperature_too_high() {
        let mut config = make_valid_config();
        config.agent.temperature = 3.0;
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report.errors().iter().any(|e| e.path == "agent.temperature"));
    }

    #[test]
    fn test_agent_temperature_boundary_valid() {
        let mut config = make_valid_config();
        config.agent.temperature = 0.0;
        assert!(validate(&config).is_valid());
        config.agent.temperature = 2.0;
        assert!(validate(&config).is_valid());
    }

    #[test]
    fn test_agent_max_tokens_zero() {
        let mut config = make_valid_config();
        config.agent.max_tokens = 0;
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report.errors().iter().any(|e| e.path == "agent.max_tokens"));
    }

    #[test]
    fn test_agent_max_tokens_very_large() {
        let mut config = make_valid_config();
        config.agent.max_tokens = 200_000;
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "agent.max_tokens"));
    }

    #[test]
    fn test_agent_max_iterations_zero() {
        let mut config = make_valid_config();
        config.agent.max_iterations = 0;
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report.errors().iter().any(|e| e.path == "agent.max_iterations"));
    }

    #[test]
    fn test_agent_tool_timeout_zero() {
        let mut config = make_valid_config();
        config.agent.tool_timeout = 0;
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "agent.tool_timeout"));
    }

    #[test]
    fn test_agent_empty_workspace() {
        let mut config = make_valid_config();
        config.agent.workspace = Some(String::new());
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "agent.workspace"));
    }

    // -------------------------------------------------------------------
    // Edge case: provider with None api_key
    // -------------------------------------------------------------------

    #[test]
    fn test_provider_none_api_key() {
        let mut config = make_valid_config();
        config.providers.openai = Some(ProviderEntry {
            api_key: None,
            base_url: None,
            model: None,
            no_proxy: None,
        });
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "providers.openai.api_key"));
    }

    // -------------------------------------------------------------------
    // Multiple providers
    // -------------------------------------------------------------------

    #[test]
    fn test_multiple_providers() {
        let mut config = make_valid_config();
        config.providers.anthropic = Some(ProviderEntry {
            api_key: Some("sk-ant-valid".to_string()),
            base_url: None,
            model: None,
            no_proxy: None,
        });
        let report = validate(&config);
        assert!(report.is_valid());
    }

    // -------------------------------------------------------------------
    // All channels disabled
    // -------------------------------------------------------------------

    #[test]
    fn test_all_channels_disabled() {
        let mut config = make_valid_config();
        config.channels = ChannelsConfig::default();
        let report = validate(&config);
        // Should warn (not error) — gateway can run local-only
        assert!(report.warnings().iter().any(|w| w.path == "channels"));
        assert!(report.is_valid());
    }
}
