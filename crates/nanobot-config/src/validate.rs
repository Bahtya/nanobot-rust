//! Configuration validation — schema checks for config.yaml.
//!
//! Validates field types, required fields, value ranges, and cross-field
//! constraints. Returns a structured [`ValidationReport`] with warnings
//! and errors.
//!
//! # Sections validated
//!
//! - **Providers** — API keys, base URLs, required fields for Azure
//! - **Channels** — required fields per enabled channel (Telegram, Discord, etc.)
//! - **Agent** — model name, temperature range, max_tokens, max_iterations
//! - **Dream** — interval bounds
//! - **Heartbeat** — interval bounds
//! - **Cron** — tick interval bounds, state file
//! - **Security** — network CIDR format
//! - **MCP servers** — transport type, required fields
//! - **Cross-field** — model ↔ provider matching

use crate::schema::{
    AgentDefaults, ChannelsConfig, Config, CronConfig, CustomProviderConfig, DiscordConfig,
    DreamConfig, HeartbeatConfig, McpServerConfig, ProvidersConfig, SecurityConfig, TelegramConfig,
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
///
/// Checks all sections: providers, channels, agent, dream, heartbeat,
/// cron, security, MCP servers, and cross-field constraints.
pub fn validate(config: &Config) -> ValidationReport {
    let mut report = ValidationReport::new();

    validate_providers(&config.providers, &config.custom_providers, &mut report);
    validate_channels(&config.channels, &mut report);
    validate_agent(&config.agent, &mut report);
    validate_dream(&config.dream, &mut report);
    validate_heartbeat(&config.heartbeat, &mut report);
    validate_cron(&config.cron, &mut report);
    validate_security(&config.security, &mut report);
    validate_mcp_servers(&config.mcp_servers, &mut report);
    validate_cross_field(&config.providers, &config.custom_providers, &config.agent, &mut report);

    report
}

/// Fill default values for optional/missing fields in a [`Config`].
///
/// This mutates the config in place, setting sensible defaults for fields
/// that are empty or unset. Returns a list of fields that were filled.
pub fn fill_defaults(config: &mut Config) -> Vec<String> {
    let mut filled = Vec::new();

    // Agent section
    if config.agent.model.is_empty() {
        config.agent.model = "gpt-4o".to_string();
        filled.push("agent.model".to_string());
    }
    if config.agent.max_tokens == 0 {
        config.agent.max_tokens = 4096;
        filled.push("agent.max_tokens".to_string());
    }
    if config.agent.max_iterations == 0 {
        config.agent.max_iterations = 50;
        filled.push("agent.max_iterations".to_string());
    }
    if config.agent.tool_timeout == 0 {
        config.agent.tool_timeout = 120;
        filled.push("agent.tool_timeout".to_string());
    }

    // Dream section
    if config.dream.interval_secs == 0 {
        config.dream.interval_secs = 7200;
        filled.push("dream.interval_secs".to_string());
    }

    // Heartbeat section
    if config.heartbeat.interval_secs == 0 {
        config.heartbeat.interval_secs = 1800;
        filled.push("heartbeat.interval_secs".to_string());
    }

    // Cron section
    if config.cron.tick_secs == 0 {
        config.cron.tick_secs = 60;
        filled.push("cron.tick_secs".to_string());
    }

    // Config version
    if config._config_version.is_none() {
        config._config_version = Some(4);
        filled.push("_config_version".to_string());
    }

    filled
}

/// Validate and fill defaults in one step.
///
/// First fills defaults, then validates. Returns the report and the list
/// of fields that were filled.
pub fn validate_and_fill(config: &mut Config) -> (ValidationReport, Vec<String>) {
    let filled = fill_defaults(config);
    let report = validate(config);
    (report, filled)
}

// ---------------------------------------------------------------------------
// URL validation helper
// ---------------------------------------------------------------------------

fn validate_url(url: &str, path: &str, report: &mut ValidationReport) {
    if url.is_empty() {
        report.error(path, "URL cannot be empty");
        return;
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        report.error(path, "URL must start with http:// or https://");
    }
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
        if let Some(ref url) = p.base_url {
            validate_url(url, "providers.openai.base_url", report);
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
        if let Some(ref url) = p.base_url {
            validate_url(url, "providers.anthropic.base_url", report);
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

    // Moonshot
    if providers.moonshot.is_some() {
        has_provider = true;
    }
    // MiniMax
    if providers.minimax.is_some() {
        has_provider = true;
    }

    // Ollama — no key needed, but check base_url
    if let Some(ref p) = providers.ollama {
        has_provider = true;
        if let Some(ref url) = p.base_url {
            if url.is_empty() {
                report.warning("providers.ollama.base_url", "base_url is empty (default: http://localhost:11434)");
            }
        }
    }

    // Azure OpenAI
    if let Some(ref p) = providers.azure_openai {
        has_provider = true;
        if p.api_key.as_deref().is_none_or(|k| k.is_empty()) {
            report.warning("providers.azure_openai.api_key", "API key is empty or missing");
        }
        if p.endpoint.as_deref().is_none_or(|e| e.is_empty()) {
            report.error("providers.azure_openai.endpoint", "Azure endpoint is required");
        } else if let Some(ref ep) = p.endpoint {
            validate_url(ep, "providers.azure_openai.endpoint", report);
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
        } else {
            validate_url(&cp.base_url, &format!("{prefix}.base_url"), report);
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
            } else if let Some(ref url) = d.webhook {
                validate_url(url, "channels.dingtalk.webhook", report);
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
            } else if let Some(ref url) = m.webhook_url {
                validate_url(url, "channels.mochat.webhook_url", report);
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

    if !(0.0..=2.0).contains(&agent.temperature) {
        report.error(
            "agent.temperature",
            format!("Temperature must be between 0.0 and 2.0, got {}", agent.temperature),
        );
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

// ---------------------------------------------------------------------------
// Dream validation
// ---------------------------------------------------------------------------

fn validate_dream(dream: &DreamConfig, report: &mut ValidationReport) {
    if dream.enabled {
        if dream.interval_secs == 0 {
            report.error("dream.interval_secs", "Dream interval must be > 0 when dream is enabled");
        } else if dream.interval_secs < 60 {
            report.warning("dream.interval_secs", "Dream interval < 60s may be too frequent and waste tokens");
        }

        if let Some(ref model) = dream.model {
            if model.is_empty() {
                report.warning("dream.model", "Dream model is set but empty — will fall back to agent model");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Heartbeat validation
// ---------------------------------------------------------------------------

fn validate_heartbeat(heartbeat: &HeartbeatConfig, report: &mut ValidationReport) {
    if heartbeat.enabled {
        if heartbeat.interval_secs == 0 {
            report.error("heartbeat.interval_secs", "Heartbeat interval must be > 0 when heartbeat is enabled");
        } else if heartbeat.interval_secs < 10 {
            report.warning("heartbeat.interval_secs", "Heartbeat interval < 10s may cause excessive load");
        } else if heartbeat.interval_secs > 86400 {
            report.warning("heartbeat.interval_secs", "Heartbeat interval > 86400s (24h) means checks will be very infrequent");
        }
    }
}

// ---------------------------------------------------------------------------
// Cron validation
// ---------------------------------------------------------------------------

fn validate_cron(cron: &CronConfig, report: &mut ValidationReport) {
    if cron.enabled {
        if cron.tick_secs == 0 {
            report.error("cron.tick_secs", "Cron tick interval must be > 0 when cron is enabled");
        } else if cron.tick_secs < 5 {
            report.warning("cron.tick_secs", "Cron tick < 5s is very aggressive and may waste CPU");
        }

        if let Some(ref path) = cron.state_file {
            if path.is_empty() {
                report.warning("cron.state_file", "State file path is set but empty");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Security validation
// ---------------------------------------------------------------------------

fn validate_security(security: &SecurityConfig, report: &mut ValidationReport) {
    for (i, cidr) in security.ssrf_whitelist.iter().enumerate() {
        if cidr.is_empty() {
            report.warning(
                format!("security.ssrf_whitelist[{i}]"),
                "Empty CIDR entry",
            );
        } else if cidr.parse::<ipnet::IpNet>().is_err() {
            report.warning(
                format!("security.ssrf_whitelist[{i}]"),
                format!("'{}' is not a valid CIDR (expected e.g. '10.0.0.0/8')", cidr),
            );
        }
    }

    for (i, cidr) in security.blocked_networks.iter().enumerate() {
        if cidr.is_empty() {
            report.warning(
                format!("security.blocked_networks[{i}]"),
                "Empty CIDR entry",
            );
        } else if cidr.parse::<ipnet::IpNet>().is_err() {
            report.warning(
                format!("security.blocked_networks[{i}]"),
                format!("'{}' is not a valid CIDR (expected e.g. '192.168.0.0/16')", cidr),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// MCP server validation
// ---------------------------------------------------------------------------

fn validate_mcp_servers(servers: &std::collections::HashMap<String, McpServerConfig>, report: &mut ValidationReport) {
    for (name, srv) in servers {
        let prefix = format!("mcp_servers.{name}");

        match srv.transport.as_str() {
            "stdio" => {
                if srv.command.as_deref().is_none_or(|c| c.is_empty()) {
                    report.error(
                        format!("{prefix}.command"),
                        "Command is required for stdio transport",
                    );
                }
            }
            "sse" | "http" => {
                if srv.url.as_deref().is_none_or(|u| u.is_empty()) {
                    report.error(
                        format!("{prefix}.url"),
                        format!("URL is required for {} transport", srv.transport),
                    );
                } else if let Some(ref url) = srv.url {
                    validate_url(url, &format!("{prefix}.url"), report);
                }
            }
            other => {
                report.error(
                    format!("{prefix}.transport"),
                    format!("Unknown transport '{}'. Must be 'stdio', 'sse', or 'http'", other),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-field validation
// ---------------------------------------------------------------------------

/// Model keyword to provider name mapping (mirrors ProviderRegistry).
const MODEL_KEYWORD_MAP: &[(&str, &str)] = &[
    ("claude", "anthropic"),
    ("anthropic", "anthropic"),
    ("gpt", "openai"),
    ("o1", "openai"),
    ("o3", "openai"),
    ("o4", "openai"),
    ("chatgpt", "openai"),
    ("deepseek", "deepseek"),
    ("gemini", "gemini"),
    ("groq", "groq"),
    ("moonshot", "moonshot"),
    ("kimi", "moonshot"),
    ("minimax", "minimax"),
    ("llama", "ollama"),
    ("mistral", "ollama"),
    ("qwen", "ollama"),
    ("codestral", "ollama"),
];

fn validate_cross_field(
    providers: &ProvidersConfig,
    custom: &[CustomProviderConfig],
    agent: &AgentDefaults,
    report: &mut ValidationReport,
) {
    if agent.model.is_empty() {
        return; // Already caught by validate_agent
    }

    // Build list of configured provider names
    let mut configured: Vec<&str> = Vec::new();
    if providers.openai.is_some() { configured.push("openai"); }
    if providers.anthropic.is_some() { configured.push("anthropic"); }
    if providers.openrouter.is_some() { configured.push("openrouter"); }
    if providers.ollama.is_some() { configured.push("ollama"); }
    if providers.deepseek.is_some() { configured.push("deepseek"); }
    if providers.gemini.is_some() { configured.push("gemini"); }
    if providers.groq.is_some() { configured.push("groq"); }
    if providers.moonshot.is_some() { configured.push("moonshot"); }
    if providers.minimax.is_some() { configured.push("minimax"); }
    if providers.azure_openai.is_some() { configured.push("azure_openai"); }
    for cp in custom {
        configured.push(&cp.name);
    }

    if configured.is_empty() {
        return; // Already caught by validate_providers
    }

    // Check if the agent model matches any configured provider
    let model_lower = agent.model.to_lowercase();
    let mut matched = false;
    for (keyword, provider_name) in MODEL_KEYWORD_MAP {
        if model_lower.contains(keyword) && configured.contains(provider_name) {
            matched = true;
            break;
        }
    }

    // Custom providers match by model_patterns
    if !matched {
        for cp in custom {
            if cp.model_patterns.iter().any(|p| model_lower.contains(&p.to_lowercase())) {
                matched = true;
                break;
            }
        }
    }

    // If the model doesn't match any provider by keyword, check if there's
    // exactly one provider (it'll be the default fallback)
    if !matched && configured.len() == 1 {
        // Single provider will handle any model — OK, unless the model
        // keyword explicitly targets a different provider
        let mut explicit_mismatch = false;
        for (keyword, provider_name) in MODEL_KEYWORD_MAP {
            if model_lower.contains(keyword) {
                // The model keyword points to a specific provider that isn't configured
                if !configured.contains(provider_name) {
                    explicit_mismatch = true;
                    break;
                }
            }
        }
        if !explicit_mismatch {
            matched = true;
        }
    }

    if !matched {
        report.warning(
            "agent.model",
            format!(
                "Model '{}' may not match any configured provider. Configured: [{}]",
                agent.model,
                configured.join(", ")
            ),
        );
    }

    // Dream model cross-check
    if let Some(ref dream_model) = agent.workspace {
        // If there's a dream model specified elsewhere we'd check it here
        let _ = dream_model;
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::*;
    use std::collections::HashMap;

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
    // Dream tests
    // -------------------------------------------------------------------

    #[test]
    fn test_dream_enabled_zero_interval() {
        let mut config = make_valid_config();
        config.dream.enabled = true;
        config.dream.interval_secs = 0;
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report.errors().iter().any(|e| e.path == "dream.interval_secs"));
    }

    #[test]
    fn test_dream_enabled_very_short_interval() {
        let mut config = make_valid_config();
        config.dream.enabled = true;
        config.dream.interval_secs = 30;
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "dream.interval_secs"));
    }

    #[test]
    fn test_dream_disabled_zero_interval_ok() {
        let mut config = make_valid_config();
        config.dream.enabled = false;
        config.dream.interval_secs = 0;
        let report = validate(&config);
        assert!(report.errors().iter().all(|e| e.path != "dream.interval_secs"));
    }

    #[test]
    fn test_dream_empty_model() {
        let mut config = make_valid_config();
        config.dream.enabled = true;
        config.dream.model = Some(String::new());
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "dream.model"));
    }

    // -------------------------------------------------------------------
    // Heartbeat tests
    // -------------------------------------------------------------------

    #[test]
    fn test_heartbeat_enabled_zero_interval() {
        let mut config = make_valid_config();
        config.heartbeat.enabled = true;
        config.heartbeat.interval_secs = 0;
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report.errors().iter().any(|e| e.path == "heartbeat.interval_secs"));
    }

    #[test]
    fn test_heartbeat_enabled_very_short_interval() {
        let mut config = make_valid_config();
        config.heartbeat.enabled = true;
        config.heartbeat.interval_secs = 5;
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "heartbeat.interval_secs"));
    }

    #[test]
    fn test_heartbeat_enabled_very_long_interval() {
        let mut config = make_valid_config();
        config.heartbeat.enabled = true;
        config.heartbeat.interval_secs = 100_000;
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "heartbeat.interval_secs" && w.message.contains("24h")));
    }

    #[test]
    fn test_heartbeat_disabled_zero_interval_ok() {
        let mut config = make_valid_config();
        config.heartbeat.enabled = false;
        config.heartbeat.interval_secs = 0;
        let report = validate(&config);
        assert!(report.errors().iter().all(|e| e.path != "heartbeat.interval_secs"));
    }

    // -------------------------------------------------------------------
    // Cron tests
    // -------------------------------------------------------------------

    #[test]
    fn test_cron_enabled_zero_tick() {
        let mut config = make_valid_config();
        config.cron.enabled = true;
        config.cron.tick_secs = 0;
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report.errors().iter().any(|e| e.path == "cron.tick_secs"));
    }

    #[test]
    fn test_cron_enabled_very_short_tick() {
        let mut config = make_valid_config();
        config.cron.enabled = true;
        config.cron.tick_secs = 1;
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "cron.tick_secs"));
    }

    #[test]
    fn test_cron_disabled_zero_tick_ok() {
        let mut config = make_valid_config();
        config.cron.enabled = false;
        config.cron.tick_secs = 0;
        let report = validate(&config);
        assert!(report.errors().iter().all(|e| e.path != "cron.tick_secs"));
    }

    #[test]
    fn test_cron_empty_state_file() {
        let mut config = make_valid_config();
        config.cron.enabled = true;
        config.cron.state_file = Some(String::new());
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "cron.state_file"));
    }

    // -------------------------------------------------------------------
    // Security tests
    // -------------------------------------------------------------------

    #[test]
    fn test_security_valid_cidr() {
        let mut config = make_valid_config();
        config.security.ssrf_whitelist = vec!["10.0.0.0/8".to_string(), "172.16.0.0/12".to_string()];
        config.security.blocked_networks = vec!["192.168.0.0/16".to_string()];
        let report = validate(&config);
        assert!(report.warnings().iter().all(|w| !w.path.starts_with("security.")));
    }

    #[test]
    fn test_security_invalid_cidr() {
        let mut config = make_valid_config();
        config.security.ssrf_whitelist = vec!["not-a-cidr".to_string()];
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "security.ssrf_whitelist[0]" && w.message.contains("not a valid CIDR")));
    }

    #[test]
    fn test_security_empty_cidr() {
        let mut config = make_valid_config();
        config.security.blocked_networks = vec![String::new()];
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "security.blocked_networks[0]"));
    }

    #[test]
    fn test_security_valid_ipv6_cidr() {
        let mut config = make_valid_config();
        config.security.ssrf_whitelist = vec!["::1/128".to_string(), "fd00::/8".to_string()];
        let report = validate(&config);
        assert!(report.warnings().iter().all(|w| !w.path.starts_with("security.")));
    }

    // -------------------------------------------------------------------
    // MCP server tests
    // -------------------------------------------------------------------

    #[test]
    fn test_mcp_stdio_valid() {
        let mut config = make_valid_config();
        config.mcp_servers.insert("fs".to_string(), McpServerConfig {
            transport: "stdio".to_string(),
            command: Some("mcp-fs".to_string()),
            args: None,
            url: None,
            env: HashMap::new(),
        });
        let report = validate(&config);
        assert!(report.is_valid(), "Unexpected errors: {}", report);
    }

    #[test]
    fn test_mcp_stdio_missing_command() {
        let mut config = make_valid_config();
        config.mcp_servers.insert("fs".to_string(), McpServerConfig {
            transport: "stdio".to_string(),
            command: None,
            args: None,
            url: None,
            env: HashMap::new(),
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report.errors().iter().any(|e| e.path == "mcp_servers.fs.command"));
    }

    #[test]
    fn test_mcp_sse_valid() {
        let mut config = make_valid_config();
        config.mcp_servers.insert("remote".to_string(), McpServerConfig {
            transport: "sse".to_string(),
            command: None,
            args: None,
            url: Some("https://mcp.example.com/sse".to_string()),
            env: HashMap::new(),
        });
        let report = validate(&config);
        assert!(report.is_valid(), "Unexpected errors: {}", report);
    }

    #[test]
    fn test_mcp_http_missing_url() {
        let mut config = make_valid_config();
        config.mcp_servers.insert("remote".to_string(), McpServerConfig {
            transport: "http".to_string(),
            command: None,
            args: None,
            url: None,
            env: HashMap::new(),
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report.errors().iter().any(|e| e.path == "mcp_servers.remote.url"));
    }

    #[test]
    fn test_mcp_unknown_transport() {
        let mut config = make_valid_config();
        config.mcp_servers.insert("bad".to_string(), McpServerConfig {
            transport: "grpc".to_string(),
            command: None,
            args: None,
            url: None,
            env: HashMap::new(),
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report.errors().iter().any(|e| e.path == "mcp_servers.bad.transport" && e.message.contains("Unknown transport")));
    }

    // -------------------------------------------------------------------
    // Cross-field tests
    // -------------------------------------------------------------------

    #[test]
    fn test_cross_field_model_matches_provider() {
        let config = make_valid_config(); // model=gpt-4o, openai configured
        let report = validate(&config);
        assert!(report.warnings().iter().all(|w| w.path != "agent.model"));
    }

    #[test]
    fn test_cross_field_model_no_match() {
        let mut config = Config::default();
        config.providers.anthropic = Some(ProviderEntry {
            api_key: Some("sk-ant-valid".to_string()),
            base_url: None,
            model: None,
            no_proxy: None,
        });
        config.agent.model = "gpt-4o".to_string(); // gpt keywords → openai, but only anthropic configured
        let report = validate(&config);
        assert!(report.warnings().iter().any(|w| w.path == "agent.model" && w.message.contains("may not match")));
    }

    #[test]
    fn test_cross_field_single_provider_any_model() {
        let mut config = Config::default();
        config.providers.openai = Some(ProviderEntry {
            api_key: Some("sk-test".to_string()),
            base_url: None,
            model: None,
            no_proxy: None,
        });
        config.agent.model = "some-unknown-model".to_string();
        let report = validate(&config);
        // Single provider → no warning (will be used as default)
        assert!(report.warnings().iter().all(|w| w.path != "agent.model"));
    }

    // -------------------------------------------------------------------
    // fill_defaults tests
    // -------------------------------------------------------------------

    #[test]
    fn test_fill_defaults_empty_config() {
        let mut config = Config::default();
        // Clear some defaults
        config.agent.model = String::new();
        config.agent.max_tokens = 0;
        config.agent.max_iterations = 0;
        config.agent.tool_timeout = 0;
        config.dream.interval_secs = 0;
        config.heartbeat.interval_secs = 0;
        config.cron.tick_secs = 0;
        config._config_version = None;

        let filled = fill_defaults(&mut config);

        assert_eq!(config.agent.model, "gpt-4o");
        assert_eq!(config.agent.max_tokens, 4096);
        assert_eq!(config.agent.max_iterations, 50);
        assert_eq!(config.agent.tool_timeout, 120);
        assert_eq!(config.dream.interval_secs, 7200);
        assert_eq!(config.heartbeat.interval_secs, 1800);
        assert_eq!(config.cron.tick_secs, 60);
        assert_eq!(config._config_version, Some(4));

        assert!(filled.contains(&"agent.model".to_string()));
        assert!(filled.contains(&"agent.max_tokens".to_string()));
        assert!(filled.contains(&"agent.max_iterations".to_string()));
        assert!(filled.contains(&"agent.tool_timeout".to_string()));
        assert!(filled.contains(&"dream.interval_secs".to_string()));
        assert!(filled.contains(&"heartbeat.interval_secs".to_string()));
        assert!(filled.contains(&"cron.tick_secs".to_string()));
        assert!(filled.contains(&"_config_version".to_string()));
    }

    #[test]
    fn test_fill_defaults_already_set() {
        let mut config = Config::default();
        config.agent.model = "claude-3".to_string();
        config.agent.max_tokens = 8192;

        let filled = fill_defaults(&mut config);

        assert_eq!(config.agent.model, "claude-3"); // not overwritten
        assert_eq!(config.agent.max_tokens, 8192); // not overwritten
        assert!(!filled.contains(&"agent.model".to_string()));
        assert!(!filled.contains(&"agent.max_tokens".to_string()));
    }

    // -------------------------------------------------------------------
    // validate_and_fill tests
    // -------------------------------------------------------------------

    #[test]
    fn test_validate_and_fill_combines() {
        let mut config = Config::default();
        config.agent.model = String::new();
        config.agent.max_tokens = 0;
        config.providers.openai = Some(ProviderEntry {
            api_key: Some("sk-test".to_string()),
            base_url: None,
            model: None,
            no_proxy: None,
        });

        let (report, filled) = validate_and_fill(&mut config);

        // Defaults should be filled
        assert_eq!(config.agent.model, "gpt-4o");
        assert_eq!(config.agent.max_tokens, 4096);
        assert!(filled.contains(&"agent.model".to_string()));

        // After filling, should be valid
        assert!(report.is_valid(), "Unexpected errors: {}", report);
    }

    // -------------------------------------------------------------------
    // Provider None api_key
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

    // -------------------------------------------------------------------
    // DingTalk webhook URL validation
    // -------------------------------------------------------------------

    #[test]
    fn test_dingtalk_invalid_webhook_url() {
        let mut config = make_valid_config();
        config.channels.dingtalk = Some(DingtalkConfig {
            webhook: Some("not-a-url".to_string()),
            secret: None,
            enabled: true,
        });
        let report = validate(&config);
        assert!(report.errors().iter().any(|e| e.path == "channels.dingtalk.webhook"));
    }

    #[test]
    fn test_dingtalk_valid_webhook_url() {
        let mut config = make_valid_config();
        config.channels.dingtalk = Some(DingtalkConfig {
            webhook: Some("https://oapi.dingtalk.com/robot/send?access_token=abc".to_string()),
            secret: None,
            enabled: true,
        });
        let report = validate(&config);
        assert!(report.errors().iter().all(|e| e.path != "channels.dingtalk.webhook"));
    }

    // -------------------------------------------------------------------
    // Mochat webhook URL validation
    // -------------------------------------------------------------------

    #[test]
    fn test_mochat_missing_webhook() {
        let mut config = make_valid_config();
        config.channels.mochat = Some(MochatConfig {
            webhook_url: None,
            enabled: true,
        });
        let report = validate(&config);
        assert!(report.errors().iter().any(|e| e.path == "channels.mochat.webhook_url"));
    }

    // -------------------------------------------------------------------
    // Provider base_url validation
    // -------------------------------------------------------------------

    #[test]
    fn test_openai_invalid_base_url() {
        let mut config = make_valid_config();
        config.providers.openai = Some(ProviderEntry {
            api_key: Some("sk-test".to_string()),
            base_url: Some("ftp://bad.proto".to_string()),
            model: None,
            no_proxy: None,
        });
        let report = validate(&config);
        assert!(report.errors().iter().any(|e| e.path == "providers.openai.base_url"));
    }

    #[test]
    fn test_azure_openai_invalid_endpoint() {
        let mut config = make_valid_config();
        config.providers.azure_openai = Some(AzureOpenAIProviderEntry {
            api_key: Some("key".to_string()),
            endpoint: Some("not-a-url".to_string()),
            deployment: Some("my-deploy".to_string()),
            api_version: None,
        });
        let report = validate(&config);
        assert!(report.errors().iter().any(|e| e.path == "providers.azure_openai.endpoint"));
    }
}
