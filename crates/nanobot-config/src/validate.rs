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
    AgentDefaults, ApiConfig, ChannelsConfig, Config, CronConfig, CustomProviderConfig,
    DiscordConfig, DreamConfig, HeartbeatConfig, McpServerConfig, ProvidersConfig, SecurityConfig,
    TelegramConfig, WebSocketConfig,
};
use std::collections::HashSet;
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
        Self {
            findings: Vec::new(),
        }
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
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .collect()
    }

    /// Only warning-level findings.
    pub fn warnings(&self) -> Vec<&ValidationFinding> {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Warning)
            .collect()
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
    validate_api(&config.api, &mut report);
    validate_cross_field(
        &config.providers,
        &config.custom_providers,
        &config.agent,
        &config.dream,
        &config.channels,
        &mut report,
    );

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
// Raw YAML env-var validation
// ---------------------------------------------------------------------------

/// Check raw config YAML for unresolved environment variable templates.
///
/// Detects `${VAR}` patterns (without a `:-default` fallback) and warns
/// if the referenced env var is not set. Returns a list of (path, message)
/// findings. This should be called on the raw YAML string **before**
/// [`crate::loader::expand_env_vars`] so unresolved vars are still visible.
///
/// Only emits warnings (not errors) because missing env vars may be
/// intentional (e.g., set at deploy time).
pub fn validate_raw_env_vars(raw_yaml: &str) -> Vec<ValidationFinding> {
    let mut findings = Vec::new();

    // Match ${VAR} without :-default
    let re = regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}")
        .expect("static regex is valid");
    for cap in re.captures_iter(raw_yaml) {
        let var_name = &cap[1];
        let has_default = cap.get(2).is_some();

        if !has_default && std::env::var(var_name).is_err() {
            // Find which line contains this variable for a useful path hint
            let full_match = cap.get(0).expect("capture group 0 always exists").as_str();
            for (line_num, line) in raw_yaml.lines().enumerate() {
                if line.contains(full_match) {
                    // Try to extract the YAML key from the line
                    let key_hint = line
                        .split(':')
                        .next()
                        .map(|k| k.trim().to_string())
                        .unwrap_or_default();
                    findings.push(ValidationFinding {
                        severity: Severity::Warning,
                        path: if key_hint.is_empty() {
                            format!("line {}", line_num + 1)
                        } else {
                            key_hint
                        },
                        message: format!(
                            "Env var '{}' is not set and has no default — will resolve to empty string",
                            var_name
                        ),
                    });
                    break;
                }
            }
        }
    }

    findings
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

/// Validate a URL and warn if it uses HTTP instead of HTTPS (for sensitive endpoints).
fn validate_url_require_https(url: &str, path: &str, report: &mut ValidationReport) {
    validate_url(url, path, report);
    if url.starts_with("http://") {
        report.warning(
            path,
            "URL uses HTTP instead of HTTPS — credentials may be exposed in transit",
        );
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
            report.warning(
                "providers.openrouter.api_key",
                "API key is empty or missing",
            );
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
                report.warning(
                    "providers.ollama.base_url",
                    "base_url is empty (default: http://localhost:11434)",
                );
            }
        }
    }

    // Azure OpenAI
    if let Some(ref p) = providers.azure_openai {
        has_provider = true;
        if p.api_key.as_deref().is_none_or(|k| k.is_empty()) {
            report.warning(
                "providers.azure_openai.api_key",
                "API key is empty or missing",
            );
        }
        if p.endpoint.as_deref().is_none_or(|e| e.is_empty()) {
            report.error(
                "providers.azure_openai.endpoint",
                "Azure endpoint is required",
            );
        } else if let Some(ref ep) = p.endpoint {
            validate_url(ep, "providers.azure_openai.endpoint", report);
        }
        if p.deployment.as_deref().is_none_or(|d| d.is_empty()) {
            report.error(
                "providers.azure_openai.deployment",
                "Deployment name is required",
            );
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
    let mut custom_names = HashSet::new();
    for (i, cp) in custom.iter().enumerate() {
        has_provider = true;
        let prefix = format!("custom_providers[{i}]");
        if cp.name.is_empty() {
            report.error(format!("{prefix}.name"), "Custom provider name is empty");
        } else if !custom_names.insert(cp.name.clone()) {
            report.error(
                format!("{prefix}.name"),
                format!("Duplicate custom provider name '{}'", cp.name),
            );
        }
        if cp.base_url.is_empty() {
            report.error(
                format!("{prefix}.base_url"),
                "Custom provider base_url is empty",
            );
        } else {
            validate_url(&cp.base_url, &format!("{prefix}.base_url"), report);
        }
        if cp.model_patterns.is_empty() {
            report.warning(
                format!("{prefix}.model_patterns"),
                "No model_patterns defined — this provider will only be used via explicit routing",
            );
        }
    }

    if !has_provider {
        report.error(
            "providers",
            "No LLM provider configured — at least one provider is required",
        );
    }
}

fn validate_api_key_prefix(
    key: &str,
    path: &str,
    expected_prefix: &str,
    report: &mut ValidationReport,
) {
    if !key.starts_with(expected_prefix) {
        report.warning(
            path,
            format!(
                "API key does not start with expected prefix '{}'",
                expected_prefix
            ),
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
                report.error(
                    "channels.slack.bot_token",
                    "Slack bot_token is required when enabled",
                );
            }
        }
    }

    // Matrix
    if let Some(ref m) = channels.matrix {
        if m.enabled {
            has_enabled_channel = true;
            if m.homeserver.as_deref().is_none_or(|h| h.is_empty()) {
                report.error(
                    "channels.matrix.homeserver",
                    "Matrix homeserver URL is required when enabled",
                );
            }
            if m.user_id.as_deref().is_none_or(|u| u.is_empty()) {
                report.error(
                    "channels.matrix.user_id",
                    "Matrix user_id is required when enabled",
                );
            }
            if m.access_token.is_none() && m.password.is_none() {
                report.error(
                    "channels.matrix",
                    "Either access_token or password is required for Matrix",
                );
            }
        }
    }

    // Email
    if let Some(ref e) = channels.email {
        if e.enabled {
            has_enabled_channel = true;
            if e.imap_host.as_deref().is_none_or(|h| h.is_empty()) {
                report.error(
                    "channels.email.imap_host",
                    "IMAP host is required when email is enabled",
                );
            }
            if e.smtp_host.as_deref().is_none_or(|h| h.is_empty()) {
                report.error(
                    "channels.email.smtp_host",
                    "SMTP host is required when email is enabled",
                );
            }
            if e.username.as_deref().is_none_or(|u| u.is_empty()) {
                report.error(
                    "channels.email.username",
                    "Username is required when email is enabled",
                );
            }
            if e.port == 0 {
                report.warning(
                    "channels.email.port",
                    "Email port is 0 — will use default (587 for SMTP)",
                );
            } else if !(1..=65535).contains(&e.port) {
                report.error(
                    "channels.email.port",
                    format!("Port must be between 1 and 65535, got {}", e.port),
                );
            } else if e.port != 25
                && e.port != 465
                && e.port != 587
                && e.port != 993
                && e.port != 995
            {
                report.warning(
                    "channels.email.port",
                    format!(
                        "Port {} is non-standard for email (common: 25, 465, 587, 993, 995)",
                        e.port
                    ),
                );
            }
        }
    }

    // DingTalk
    if let Some(ref d) = channels.dingtalk {
        if d.enabled {
            has_enabled_channel = true;
            if d.webhook.as_deref().is_none_or(|w| w.is_empty()) {
                report.error(
                    "channels.dingtalk.webhook",
                    "Webhook URL is required when DingTalk is enabled",
                );
            } else if let Some(ref url) = d.webhook {
                validate_url_require_https(url, "channels.dingtalk.webhook", report);
            }
        }
    }

    // Feishu
    if let Some(ref f) = channels.feishu {
        if f.enabled {
            has_enabled_channel = true;
            if f.app_id.as_deref().is_none_or(|a| a.is_empty()) {
                report.error(
                    "channels.feishu.app_id",
                    "App ID is required when Feishu is enabled",
                );
            }
            if f.app_secret.as_deref().is_none_or(|s| s.is_empty()) {
                report.error(
                    "channels.feishu.app_secret",
                    "App secret is required when Feishu is enabled",
                );
            }
        }
    }

    // WeCom
    if let Some(ref w) = channels.wecom {
        if w.enabled {
            has_enabled_channel = true;
            if w.corp_id.as_deref().is_none_or(|c| c.is_empty()) {
                report.error(
                    "channels.wecom.corp_id",
                    "Corp ID is required when WeCom is enabled",
                );
            }
            if w.secret.as_deref().is_none_or(|s| s.is_empty()) {
                report.error(
                    "channels.wecom.secret",
                    "Secret is required when WeCom is enabled",
                );
            }
        }
    }

    // Weixin
    if let Some(ref w) = channels.weixin {
        if w.enabled {
            has_enabled_channel = true;
            if w.app_id.as_deref().is_none_or(|a| a.is_empty()) {
                report.error(
                    "channels.weixin.app_id",
                    "App ID is required when WeChat is enabled",
                );
            }
        }
    }

    // QQ
    if let Some(ref q) = channels.qq {
        if q.enabled {
            has_enabled_channel = true;
            if q.app_id.as_deref().is_none_or(|a| a.is_empty()) {
                report.error(
                    "channels.qq.app_id",
                    "App ID is required when QQ is enabled",
                );
            }
        }
    }

    // Mochat
    if let Some(ref m) = channels.mochat {
        if m.enabled {
            has_enabled_channel = true;
            if m.webhook_url.as_deref().is_none_or(|u| u.is_empty()) {
                report.error(
                    "channels.mochat.webhook_url",
                    "Webhook URL is required when Mochat is enabled",
                );
            } else if let Some(ref url) = m.webhook_url {
                validate_url_require_https(url, "channels.mochat.webhook_url", report);
            }
        }
    }

    // Matrix homeserver — should use HTTPS
    if let Some(ref m) = channels.matrix {
        if m.enabled {
            if let Some(ref hs) = m.homeserver {
                if !hs.is_empty() {
                    validate_url_require_https(hs, "channels.matrix.homeserver", report);
                }
            }
        }
    }

    // WhatsApp
    if channels.whatsapp.as_ref().is_some_and(|w| w.enabled) {
        has_enabled_channel = true;
        report.warning(
            "channels.whatsapp",
            "WhatsApp channel is not yet implemented",
        );
    }

    // WebSocket
    if let Some(ref ws) = channels.websocket {
        if ws.enabled {
            has_enabled_channel = true;
            validate_websocket(ws, report);
        }
    }

    if !has_enabled_channel {
        report.warning(
            "channels",
            "No channel is enabled — gateway will run in local-only mode",
        );
    }
}

fn validate_telegram(tg: &TelegramConfig, report: &mut ValidationReport) {
    if tg.token.is_empty() {
        report.error(
            "channels.telegram.token",
            "Bot token is required when Telegram is enabled",
        );
    } else if !tg.token.contains(':') {
        report.warning(
            "channels.telegram.token",
            "Token format looks invalid — expected '123456:ABC-DEF'",
        );
    }
}

fn validate_discord(dc: &DiscordConfig, report: &mut ValidationReport) {
    if dc.token.is_empty() {
        report.error(
            "channels.discord.token",
            "Bot token is required when Discord is enabled",
        );
    } else if dc.token.len() < 20 {
        report.warning(
            "channels.discord.token",
            "Token looks too short — expected a longer bot token",
        );
    }
}

fn validate_websocket(ws: &WebSocketConfig, report: &mut ValidationReport) {
    // Validate listen_addr format — must contain a colon separating host and port
    if ws.listen_addr.is_empty() {
        report.error(
            "channels.websocket.listen_addr",
            "Listen address cannot be empty",
        );
    } else if !ws.listen_addr.contains(':') {
        report.error(
            "channels.websocket.listen_addr",
            format!(
                "Listen address '{}' must be in 'host:port' format",
                ws.listen_addr
            ),
        );
    } else {
        // Try to parse the port portion
        let port_str = ws.listen_addr.rsplit(':').next().unwrap_or("");
        match port_str.parse::<u16>() {
            Ok(port) => {
                if port == 0 {
                    report.warning(
                        "channels.websocket.listen_addr",
                        "Port 0 means the OS will assign a random port",
                    );
                }
            }
            Err(_) => {
                report.error(
                    "channels.websocket.listen_addr",
                    format!("Invalid port in listen address '{}'", ws.listen_addr),
                );
            }
        }
    }

    if ws.max_clients == 0 {
        report.error("channels.websocket.max_clients", "max_clients must be > 0");
    } else if ws.max_clients > 10000 {
        report.warning(
            "channels.websocket.max_clients",
            format!(
                "max_clients of {} is very high — may exhaust resources",
                ws.max_clients
            ),
        );
    }

    if ws.max_message_size == 0 {
        report.error(
            "channels.websocket.max_message_size",
            "max_message_size must be > 0",
        );
    }

    if ws.auth.required && ws.auth.token.as_deref().is_none_or(|t| t.is_empty()) {
        report.error(
            "channels.websocket.auth.token",
            "Auth token is required when authentication is enabled",
        );
    }
}

// ---------------------------------------------------------------------------
// Agent validation
// ---------------------------------------------------------------------------

fn validate_agent(agent: &AgentDefaults, report: &mut ValidationReport) {
    if agent.model.is_empty() {
        report.error("agent.model", "Model name cannot be empty");
    } else if agent.model.contains(char::is_whitespace) {
        report.error(
            "agent.model",
            format!("Model name '{}' contains whitespace", agent.model),
        );
    }

    if !(0.0..=2.0).contains(&agent.temperature) {
        report.error(
            "agent.temperature",
            format!(
                "Temperature must be between 0.0 and 2.0, got {}",
                agent.temperature
            ),
        );
    }

    if agent.max_tokens == 0 {
        report.error("agent.max_tokens", "max_tokens must be > 0");
    } else if agent.max_tokens > 128_000 {
        report.warning(
            "agent.max_tokens",
            "max_tokens > 128000 may not be supported by all models",
        );
    }

    if agent.max_iterations == 0 {
        report.error("agent.max_iterations", "max_iterations must be > 0");
    } else if agent.max_iterations > 500 {
        report.warning(
            "agent.max_iterations",
            format!("max_iterations of {} is very high — may cause excessive token usage or infinite loops", agent.max_iterations),
        );
    }

    if agent.tool_timeout == 0 {
        report.warning(
            "agent.tool_timeout",
            "tool_timeout of 0 means tools will timeout immediately",
        );
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
            report.error(
                "dream.interval_secs",
                "Dream interval must be > 0 when dream is enabled",
            );
        } else if dream.interval_secs < 60 {
            report.warning(
                "dream.interval_secs",
                "Dream interval < 60s may be too frequent and waste tokens",
            );
        }

        if let Some(ref model) = dream.model {
            if model.is_empty() {
                report.warning(
                    "dream.model",
                    "Dream model is set but empty — will fall back to agent model",
                );
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
            report.error(
                "heartbeat.interval_secs",
                "Heartbeat interval must be > 0 when heartbeat is enabled",
            );
        } else if heartbeat.interval_secs < 10 {
            report.warning(
                "heartbeat.interval_secs",
                "Heartbeat interval < 10s may cause excessive load",
            );
        } else if heartbeat.interval_secs > 86400 {
            report.warning(
                "heartbeat.interval_secs",
                "Heartbeat interval > 86400s (24h) means checks will be very infrequent",
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Cron validation
// ---------------------------------------------------------------------------

fn validate_cron(cron: &CronConfig, report: &mut ValidationReport) {
    if cron.enabled {
        if cron.tick_secs == 0 {
            report.error(
                "cron.tick_secs",
                "Cron tick interval must be > 0 when cron is enabled",
            );
        } else if cron.tick_secs < 5 {
            report.warning(
                "cron.tick_secs",
                "Cron tick < 5s is very aggressive and may waste CPU",
            );
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
            report.warning(format!("security.ssrf_whitelist[{i}]"), "Empty CIDR entry");
        } else if cidr.parse::<ipnet::IpNet>().is_err() {
            report.warning(
                format!("security.ssrf_whitelist[{i}]"),
                format!(
                    "'{}' is not a valid CIDR (expected e.g. '10.0.0.0/8')",
                    cidr
                ),
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
                format!(
                    "'{}' is not a valid CIDR (expected e.g. '192.168.0.0/16')",
                    cidr
                ),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// MCP server validation
// ---------------------------------------------------------------------------

fn validate_mcp_servers(
    servers: &std::collections::HashMap<String, McpServerConfig>,
    report: &mut ValidationReport,
) {
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
                    format!(
                        "Unknown transport '{}'. Must be 'stdio', 'sse', or 'http'",
                        other
                    ),
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

// ---------------------------------------------------------------------------
// API server validation
// ---------------------------------------------------------------------------

fn validate_api(api: &ApiConfig, report: &mut ValidationReport) {
    if api.max_body_size == 0 {
        report.error("api.max_body_size", "Max body size must be > 0");
    } else if api.max_body_size < 1024 {
        report.warning(
            "api.max_body_size",
            "Max body size < 1KB may reject legitimate requests",
        );
    }
    if api.allowed_origins.is_empty() {
        report.warning(
            "api.allowed_origins",
            "No CORS origins configured — API may reject browser requests",
        );
    } else {
        for (i, origin) in api.allowed_origins.iter().enumerate() {
            if origin.is_empty() {
                report.warning(format!("api.allowed_origins[{i}]"), "Empty origin entry");
            } else if origin != "*"
                && !origin.starts_with("http://")
                && !origin.starts_with("https://")
            {
                report.warning(
                    format!("api.allowed_origins[{i}]"),
                    format!("Origin '{}' should be '*' or start with http(s)://", origin),
                );
            }
        }
    }
}

fn validate_cross_field(
    providers: &ProvidersConfig,
    custom: &[CustomProviderConfig],
    agent: &AgentDefaults,
    dream: &DreamConfig,
    channels: &ChannelsConfig,
    report: &mut ValidationReport,
) {
    if agent.model.is_empty() {
        return; // Already caught by validate_agent
    }

    // Build list of configured provider names
    let mut configured: Vec<&str> = Vec::new();
    if providers.openai.is_some() {
        configured.push("openai");
    }
    if providers.anthropic.is_some() {
        configured.push("anthropic");
    }
    if providers.openrouter.is_some() {
        configured.push("openrouter");
    }
    if providers.ollama.is_some() {
        configured.push("ollama");
    }
    if providers.deepseek.is_some() {
        configured.push("deepseek");
    }
    if providers.gemini.is_some() {
        configured.push("gemini");
    }
    if providers.groq.is_some() {
        configured.push("groq");
    }
    if providers.moonshot.is_some() {
        configured.push("moonshot");
    }
    if providers.minimax.is_some() {
        configured.push("minimax");
    }
    if providers.azure_openai.is_some() {
        configured.push("azure_openai");
    }
    for cp in custom {
        configured.push(&cp.name);
    }

    if configured.is_empty() {
        return; // Already caught by validate_providers
    }

    // Cross-check: if any channel is enabled, verify at least one provider is configured.
    // (This produces a more specific warning than the generic "no provider" error.)
    let has_enabled_channel = [
        channels.telegram.as_ref().is_some_and(|t| t.enabled),
        channels.discord.as_ref().is_some_and(|d| d.enabled),
        channels.slack.as_ref().is_some_and(|s| s.enabled),
        channels.matrix.as_ref().is_some_and(|m| m.enabled),
        channels.email.as_ref().is_some_and(|e| e.enabled),
        channels.dingtalk.as_ref().is_some_and(|d| d.enabled),
        channels.feishu.as_ref().is_some_and(|f| f.enabled),
        channels.wecom.as_ref().is_some_and(|w| w.enabled),
        channels.weixin.as_ref().is_some_and(|w| w.enabled),
        channels.qq.as_ref().is_some_and(|q| q.enabled),
        channels.mochat.as_ref().is_some_and(|m| m.enabled),
        channels.whatsapp.as_ref().is_some_and(|w| w.enabled),
        channels.websocket.as_ref().is_some_and(|w| w.enabled),
    ]
    .iter()
    .any(|&v| v);

    if has_enabled_channel && configured.is_empty() {
        report.warning(
            "channels",
            "Channels are enabled but no LLM provider is configured — the gateway will not be able to respond",
        );
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
            if cp
                .model_patterns
                .iter()
                .any(|p| model_lower.contains(&p.to_lowercase()))
            {
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

    // Dream model cross-check — verify the dream model also has a matching provider
    if dream.enabled {
        if let Some(ref dream_model) = dream.model {
            if !dream_model.is_empty() {
                let dm_lower = dream_model.to_lowercase();
                let mut dream_matched = false;
                for (keyword, provider_name) in MODEL_KEYWORD_MAP {
                    if dm_lower.contains(keyword) && configured.contains(provider_name) {
                        dream_matched = true;
                        break;
                    }
                }
                if !dream_matched {
                    for cp in custom {
                        if cp
                            .model_patterns
                            .iter()
                            .any(|p| dm_lower.contains(&p.to_lowercase()))
                        {
                            dream_matched = true;
                            break;
                        }
                    }
                }
                if !dream_matched && configured.len() == 1 {
                    // Single provider fallback — only OK if no explicit keyword mismatch
                    let mut explicit_mismatch = false;
                    for (keyword, provider_name) in MODEL_KEYWORD_MAP {
                        if dm_lower.contains(keyword) && !configured.contains(provider_name) {
                            explicit_mismatch = true;
                            break;
                        }
                    }
                    if !explicit_mismatch {
                        dream_matched = true;
                    }
                }
                if !dream_matched {
                    report.warning(
                        "dream.model",
                        format!(
                            "Dream model '{}' may not match any configured provider",
                            dream_model
                        ),
                    );
                }
            }
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
        assert_eq!(
            format!("{}", finding),
            "[ERROR] agent.model: cannot be empty"
        );
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
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "providers.openai.api_key"));
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
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "providers.openai.api_key" && w.message.contains("sk-")));
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
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "providers.anthropic.api_key" && w.message.contains("sk-ant-")));
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
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "providers.ollama.base_url"));
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
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "channels.telegram.token"));
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
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "channels.telegram.token"));
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
        assert!(report
            .errors()
            .iter()
            .all(|e| e.path != "channels.telegram.token"));
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
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "channels.discord.token"));
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
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "channels.discord.token"));
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
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "channels.slack.bot_token"));
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
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "agent.temperature"));
    }

    #[test]
    fn test_agent_temperature_too_high() {
        let mut config = make_valid_config();
        config.agent.temperature = 3.0;
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "agent.temperature"));
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
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "agent.max_tokens"));
    }

    #[test]
    fn test_agent_max_iterations_zero() {
        let mut config = make_valid_config();
        config.agent.max_iterations = 0;
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "agent.max_iterations"));
    }

    #[test]
    fn test_agent_tool_timeout_zero() {
        let mut config = make_valid_config();
        config.agent.tool_timeout = 0;
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "agent.tool_timeout"));
    }

    #[test]
    fn test_agent_empty_workspace() {
        let mut config = make_valid_config();
        config.agent.workspace = Some(String::new());
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "agent.workspace"));
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
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "dream.interval_secs"));
    }

    #[test]
    fn test_dream_enabled_very_short_interval() {
        let mut config = make_valid_config();
        config.dream.enabled = true;
        config.dream.interval_secs = 30;
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "dream.interval_secs"));
    }

    #[test]
    fn test_dream_disabled_zero_interval_ok() {
        let mut config = make_valid_config();
        config.dream.enabled = false;
        config.dream.interval_secs = 0;
        let report = validate(&config);
        assert!(report
            .errors()
            .iter()
            .all(|e| e.path != "dream.interval_secs"));
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
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "heartbeat.interval_secs"));
    }

    #[test]
    fn test_heartbeat_enabled_very_short_interval() {
        let mut config = make_valid_config();
        config.heartbeat.enabled = true;
        config.heartbeat.interval_secs = 5;
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "heartbeat.interval_secs"));
    }

    #[test]
    fn test_heartbeat_enabled_very_long_interval() {
        let mut config = make_valid_config();
        config.heartbeat.enabled = true;
        config.heartbeat.interval_secs = 100_000;
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "heartbeat.interval_secs" && w.message.contains("24h")));
    }

    #[test]
    fn test_heartbeat_disabled_zero_interval_ok() {
        let mut config = make_valid_config();
        config.heartbeat.enabled = false;
        config.heartbeat.interval_secs = 0;
        let report = validate(&config);
        assert!(report
            .errors()
            .iter()
            .all(|e| e.path != "heartbeat.interval_secs"));
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
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "cron.state_file"));
    }

    // -------------------------------------------------------------------
    // Security tests
    // -------------------------------------------------------------------

    #[test]
    fn test_security_valid_cidr() {
        let mut config = make_valid_config();
        config.security.ssrf_whitelist =
            vec!["10.0.0.0/8".to_string(), "172.16.0.0/12".to_string()];
        config.security.blocked_networks = vec!["192.168.0.0/16".to_string()];
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .all(|w| !w.path.starts_with("security.")));
    }

    #[test]
    fn test_security_invalid_cidr() {
        let mut config = make_valid_config();
        config.security.ssrf_whitelist = vec!["not-a-cidr".to_string()];
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "security.ssrf_whitelist[0]"
                && w.message.contains("not a valid CIDR")));
    }

    #[test]
    fn test_security_empty_cidr() {
        let mut config = make_valid_config();
        config.security.blocked_networks = vec![String::new()];
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "security.blocked_networks[0]"));
    }

    #[test]
    fn test_security_valid_ipv6_cidr() {
        let mut config = make_valid_config();
        config.security.ssrf_whitelist = vec!["::1/128".to_string(), "fd00::/8".to_string()];
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .all(|w| !w.path.starts_with("security.")));
    }

    // -------------------------------------------------------------------
    // MCP server tests
    // -------------------------------------------------------------------

    #[test]
    fn test_mcp_stdio_valid() {
        let mut config = make_valid_config();
        config.mcp_servers.insert(
            "fs".to_string(),
            McpServerConfig {
                transport: "stdio".to_string(),
                command: Some("mcp-fs".to_string()),
                args: None,
                url: None,
                env: HashMap::new(),
            },
        );
        let report = validate(&config);
        assert!(report.is_valid(), "Unexpected errors: {}", report);
    }

    #[test]
    fn test_mcp_stdio_missing_command() {
        let mut config = make_valid_config();
        config.mcp_servers.insert(
            "fs".to_string(),
            McpServerConfig {
                transport: "stdio".to_string(),
                command: None,
                args: None,
                url: None,
                env: HashMap::new(),
            },
        );
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "mcp_servers.fs.command"));
    }

    #[test]
    fn test_mcp_sse_valid() {
        let mut config = make_valid_config();
        config.mcp_servers.insert(
            "remote".to_string(),
            McpServerConfig {
                transport: "sse".to_string(),
                command: None,
                args: None,
                url: Some("https://mcp.example.com/sse".to_string()),
                env: HashMap::new(),
            },
        );
        let report = validate(&config);
        assert!(report.is_valid(), "Unexpected errors: {}", report);
    }

    #[test]
    fn test_mcp_http_missing_url() {
        let mut config = make_valid_config();
        config.mcp_servers.insert(
            "remote".to_string(),
            McpServerConfig {
                transport: "http".to_string(),
                command: None,
                args: None,
                url: None,
                env: HashMap::new(),
            },
        );
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "mcp_servers.remote.url"));
    }

    #[test]
    fn test_mcp_unknown_transport() {
        let mut config = make_valid_config();
        config.mcp_servers.insert(
            "bad".to_string(),
            McpServerConfig {
                transport: "grpc".to_string(),
                command: None,
                args: None,
                url: None,
                env: HashMap::new(),
            },
        );
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "mcp_servers.bad.transport"
                && e.message.contains("Unknown transport")));
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
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "agent.model" && w.message.contains("may not match")));
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
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "providers.openai.api_key"));
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
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "channels.dingtalk.webhook"));
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
        assert!(report
            .errors()
            .iter()
            .all(|e| e.path != "channels.dingtalk.webhook"));
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
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "channels.mochat.webhook_url"));
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
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "providers.openai.base_url"));
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
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "providers.azure_openai.endpoint"));
    }

    // -------------------------------------------------------------------
    // New: Duplicate custom provider names
    // -------------------------------------------------------------------

    #[test]
    fn test_custom_provider_duplicate_names() {
        let mut config = make_valid_config();
        config.custom_providers.push(CustomProviderConfig {
            name: "dup".to_string(),
            base_url: "https://a.com/v1".to_string(),
            api_key: Some("key".to_string()),
            model_patterns: vec!["a".to_string()],
            no_proxy: None,
        });
        config.custom_providers.push(CustomProviderConfig {
            name: "dup".to_string(),
            base_url: "https://b.com/v1".to_string(),
            api_key: Some("key".to_string()),
            model_patterns: vec!["b".to_string()],
            no_proxy: None,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "custom_providers[1].name" && e.message.contains("Duplicate")));
    }

    // -------------------------------------------------------------------
    // New: Custom provider no model_patterns warning
    // -------------------------------------------------------------------

    #[test]
    fn test_custom_provider_no_model_patterns_warning() {
        let mut config = make_valid_config();
        config.custom_providers.push(CustomProviderConfig {
            name: "bare".to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            api_key: Some("key".to_string()),
            model_patterns: vec![],
            no_proxy: None,
        });
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "custom_providers[0].model_patterns"));
    }

    // -------------------------------------------------------------------
    // New: Email port validation
    // -------------------------------------------------------------------

    #[test]
    fn test_email_port_zero_warns() {
        let mut config = make_valid_config();
        config.channels.email = Some(EmailConfig {
            imap_host: Some("imap.example.com".to_string()),
            smtp_host: Some("smtp.example.com".to_string()),
            username: Some("user@example.com".to_string()),
            password: None,
            port: 0,
            enabled: true,
        });
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "channels.email.port" && w.message.contains("0")));
    }

    #[test]
    fn test_email_port_nonstandard_warns() {
        let mut config = make_valid_config();
        config.channels.email = Some(EmailConfig {
            imap_host: Some("imap.example.com".to_string()),
            smtp_host: Some("smtp.example.com".to_string()),
            username: Some("user@example.com".to_string()),
            password: None,
            port: 8080,
            enabled: true,
        });
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "channels.email.port" && w.message.contains("non-standard")));
    }

    #[test]
    fn test_email_port_standard_ok() {
        let mut config = make_valid_config();
        config.channels.email = Some(EmailConfig {
            imap_host: Some("imap.example.com".to_string()),
            smtp_host: Some("smtp.example.com".to_string()),
            username: Some("user@example.com".to_string()),
            password: None,
            port: 587,
            enabled: true,
        });
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .all(|w| w.path != "channels.email.port"));
    }

    // -------------------------------------------------------------------
    // New: URL HTTPS enforcement for webhooks
    // -------------------------------------------------------------------

    #[test]
    fn test_dingtalk_http_webhook_warns() {
        let mut config = make_valid_config();
        config.channels.dingtalk = Some(DingtalkConfig {
            webhook: Some("http://oapi.dingtalk.com/robot/send?access_token=abc".to_string()),
            secret: None,
            enabled: true,
        });
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "channels.dingtalk.webhook"
                && w.message.contains("HTTP instead of HTTPS")));
    }

    #[test]
    fn test_mochat_http_webhook_warns() {
        let mut config = make_valid_config();
        config.channels.mochat = Some(MochatConfig {
            webhook_url: Some("http://hook.example.com/webhook".to_string()),
            enabled: true,
        });
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "channels.mochat.webhook_url"
                && w.message.contains("HTTP instead of HTTPS")));
    }

    #[test]
    fn test_matrix_homeserver_http_warns() {
        let mut config = make_valid_config();
        config.channels.matrix = Some(MatrixConfig {
            homeserver: Some("http://matrix.example.com".to_string()),
            user_id: Some("@bot:example.com".to_string()),
            password: Some("pass".to_string()),
            access_token: None,
            enabled: true,
        });
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "channels.matrix.homeserver"
                && w.message.contains("HTTP instead of HTTPS")));
    }

    #[test]
    fn test_matrix_homeserver_https_ok() {
        let mut config = make_valid_config();
        config.channels.matrix = Some(MatrixConfig {
            homeserver: Some("https://matrix.example.com".to_string()),
            user_id: Some("@bot:example.com".to_string()),
            password: Some("pass".to_string()),
            access_token: None,
            enabled: true,
        });
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .all(|w| w.path != "channels.matrix.homeserver"));
    }

    // -------------------------------------------------------------------
    // New: Agent model whitespace
    // -------------------------------------------------------------------

    #[test]
    fn test_agent_model_whitespace_error() {
        let mut config = make_valid_config();
        config.agent.model = "gpt 4o".to_string();
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "agent.model" && e.message.contains("whitespace")));
    }

    // -------------------------------------------------------------------
    // New: Agent max_iterations upper bound
    // -------------------------------------------------------------------

    #[test]
    fn test_agent_max_iterations_very_large() {
        let mut config = make_valid_config();
        config.agent.max_iterations = 1000;
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "agent.max_iterations" && w.message.contains("very high")));
    }

    // -------------------------------------------------------------------
    // New: API allowed_origins format
    // -------------------------------------------------------------------

    #[test]
    fn test_api_allowed_origins_bad_format() {
        let mut config = make_valid_config();
        config.api.allowed_origins = vec!["example.com".to_string()];
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "api.allowed_origins[0]" && w.message.contains("should be")));
    }

    #[test]
    fn test_api_allowed_origins_wildcard_ok() {
        let mut config = make_valid_config();
        config.api.allowed_origins = vec!["*".to_string()];
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .all(|w| !w.path.starts_with("api.allowed_origins")));
    }

    #[test]
    fn test_api_allowed_origins_valid_url_ok() {
        let mut config = make_valid_config();
        config.api.allowed_origins = vec!["https://example.com".to_string()];
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .all(|w| !w.path.starts_with("api.allowed_origins")));
    }

    #[test]
    fn test_api_max_body_size_tiny_warns() {
        let mut config = make_valid_config();
        config.api.max_body_size = 512;
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "api.max_body_size" && w.message.contains("1KB")));
    }

    // -------------------------------------------------------------------
    // New: Dream model cross-check
    // -------------------------------------------------------------------

    #[test]
    fn test_dream_model_cross_check_mismatch() {
        let mut config = make_valid_config();
        config.dream.enabled = true;
        config.dream.model = Some("claude-3".to_string()); // only openai configured
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "dream.model" && w.message.contains("may not match")));
    }

    #[test]
    fn test_dream_model_cross_check_match() {
        let mut config = make_valid_config();
        config.dream.enabled = true;
        config.dream.model = Some("gpt-4o-mini".to_string()); // openai configured
        let report = validate(&config);
        assert!(report.warnings().iter().all(|w| w.path != "dream.model"));
    }

    #[test]
    fn test_dream_model_cross_check_disabled() {
        let mut config = make_valid_config();
        config.dream.enabled = false;
        config.dream.model = Some("claude-3".to_string()); // mismatch but dream disabled
        let report = validate(&config);
        assert!(report.warnings().iter().all(|w| w.path != "dream.model"));
    }

    // -------------------------------------------------------------------
    // validate_raw_env_vars tests
    // -------------------------------------------------------------------

    #[test]
    fn test_raw_env_vars_unresolved_var() {
        // Ensure THIS_VAR_DOES_NOT_EXIST is not in the environment
        std::env::remove_var("THIS_VAR_DOES_NOT_EXIST_XYZ");
        let yaml = r#"
providers:
  openai:
    api_key: ${THIS_VAR_DOES_NOT_EXIST_XYZ}
"#;
        let findings = validate_raw_env_vars(yaml);
        assert!(!findings.is_empty());
        assert!(findings
            .iter()
            .any(|f| f.message.contains("THIS_VAR_DOES_NOT_EXIST_XYZ")));
        assert_eq!(findings[0].severity, Severity::Warning);
    }

    #[test]
    fn test_raw_env_vars_with_default_ok() {
        // Var with a default should NOT produce a warning
        std::env::remove_var("PROBABLY_MISSING_VAR_ABC");
        let yaml = r#"
providers:
  openai:
    api_key: ${PROBABLY_MISSING_VAR_ABC:-fallback-key}
"#;
        let findings = validate_raw_env_vars(yaml);
        assert!(
            findings.is_empty(),
            "Expected no findings for var with default, got: {:?}",
            findings
        );
    }

    #[test]
    fn test_raw_env_vars_set_in_env_ok() {
        // Var that IS set should NOT produce a warning
        std::env::set_var("NANOBOT_TEST_SET_VAR", "hello");
        let yaml = r#"
providers:
  openai:
    api_key: ${NANOBOT_TEST_SET_VAR}
"#;
        let findings = validate_raw_env_vars(yaml);
        std::env::remove_var("NANOBOT_TEST_SET_VAR");
        assert!(
            findings.is_empty(),
            "Expected no findings for set var, got: {:?}",
            findings
        );
    }

    #[test]
    fn test_raw_env_vars_no_vars() {
        let yaml = r#"
agent:
  model: gpt-4o
  temperature: 0.7
"#;
        let findings = validate_raw_env_vars(yaml);
        assert!(findings.is_empty());
    }

    #[test]
    fn test_raw_env_vars_multiple_unresolved() {
        std::env::remove_var("MISSING_AAA");
        std::env::remove_var("MISSING_BBB");
        let yaml = r#"
providers:
  openai:
    api_key: ${MISSING_AAA}
channels:
  telegram:
    token: ${MISSING_BBB}
"#;
        let findings = validate_raw_env_vars(yaml);
        assert_eq!(findings.len(), 2);
        let names: Vec<&str> = findings
            .iter()
            .map(|f| {
                // Extract var name from message
                f.message.split('\'').nth(1).unwrap_or("")
            })
            .collect();
        assert!(names.contains(&"MISSING_AAA"));
        assert!(names.contains(&"MISSING_BBB"));
    }

    #[test]
    fn test_raw_env_vars_path_is_yaml_key() {
        std::env::remove_var("MISSING_KEY_FOR_PATH_TEST");
        let yaml = "api_key: ${MISSING_KEY_FOR_PATH_TEST}\n";
        let findings = validate_raw_env_vars(yaml);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].path, "api_key");
    }

    #[test]
    fn test_raw_env_vars_empty_default_no_warn() {
        // ${VAR:-} has an empty default, should NOT warn
        std::env::remove_var("MISSING_WITH_EMPTY_DEFAULT");
        let yaml = "key: ${MISSING_WITH_EMPTY_DEFAULT:-}\n";
        let findings = validate_raw_env_vars(yaml);
        assert!(
            findings.is_empty(),
            "Expected no warning for var with empty default, got: {:?}",
            findings
        );
    }

    // -------------------------------------------------------------------
    // Channel-provider cross-field tests
    // -------------------------------------------------------------------

    #[test]
    fn test_channels_enabled_no_provider_warns() {
        // Channels enabled but no provider → cross-field warning
        let mut config = Config::default();
        // No providers configured at all
        config.channels.telegram = Some(TelegramConfig {
            token: "123456:ABC-DEF".to_string(),
            enabled: true,
            allowed_users: vec![],
            admin_users: vec![],
            streaming: false,
        });
        let report = validate(&config);
        // Should have errors about no provider + the provider error
        assert!(!report.is_valid());
    }

    #[test]
    fn test_channels_enabled_with_provider_ok() {
        // Channels enabled WITH a provider → no channel-provider warning
        let config = make_valid_config(); // has openai provider + telegram enabled
        let report = validate(&config);
        assert!(report.is_valid(), "Unexpected errors: {}", report);
        assert!(report
            .warnings()
            .iter()
            .all(|w| !w.message.contains("no LLM provider")));
    }

    // -------------------------------------------------------------------
    // Additional invalid config scenario tests
    // -------------------------------------------------------------------

    #[test]
    fn test_invalid_temperature_string_model() {
        // Agent model with only whitespace characters
        let mut config = make_valid_config();
        config.agent.model = "   ".to_string();
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report.errors().iter().any(|e| e.path == "agent.model"));
    }

    #[test]
    fn test_agent_max_iterations_boundary() {
        let mut config = make_valid_config();
        config.agent.max_iterations = 500;
        let report = validate(&config);
        assert!(report.is_valid());
        assert!(report
            .warnings()
            .iter()
            .all(|w| w.path != "agent.max_iterations"));
    }

    #[test]
    fn test_agent_max_iterations_just_above_boundary() {
        let mut config = make_valid_config();
        config.agent.max_iterations = 501;
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "agent.max_iterations" && w.message.contains("very high")));
    }

    #[test]
    fn test_heartbeat_boundary_valid() {
        let mut config = make_valid_config();
        config.heartbeat.enabled = true;
        config.heartbeat.interval_secs = 10;
        assert!(validate(&config).is_valid());
        config.heartbeat.interval_secs = 86400;
        assert!(validate(&config).is_valid());
    }

    #[test]
    fn test_cron_boundary_valid() {
        let mut config = make_valid_config();
        config.cron.enabled = true;
        config.cron.tick_secs = 5;
        assert!(validate(&config).is_valid());
    }

    #[test]
    fn test_dream_boundary_valid() {
        let mut config = make_valid_config();
        config.dream.enabled = true;
        config.dream.interval_secs = 60;
        assert!(validate(&config).is_valid());
    }

    #[test]
    fn test_dream_interval_below_60_warns() {
        let mut config = make_valid_config();
        config.dream.enabled = true;
        config.dream.interval_secs = 59;
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "dream.interval_secs"));
    }

    #[test]
    fn test_api_empty_origins_warns() {
        let mut config = make_valid_config();
        config.api.allowed_origins = vec![];
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "api.allowed_origins"));
    }

    #[test]
    fn test_api_zero_body_size_errors() {
        let mut config = make_valid_config();
        config.api.max_body_size = 0;
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "api.max_body_size"));
    }

    #[test]
    fn test_mcp_empty_command_error() {
        let mut config = make_valid_config();
        config.mcp_servers.insert(
            "bad".to_string(),
            McpServerConfig {
                transport: "stdio".to_string(),
                command: Some(String::new()),
                args: None,
                url: None,
                env: HashMap::new(),
            },
        );
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "mcp_servers.bad.command"));
    }

    #[test]
    fn test_mcp_sse_empty_url_error() {
        let mut config = make_valid_config();
        config.mcp_servers.insert(
            "bad".to_string(),
            McpServerConfig {
                transport: "sse".to_string(),
                command: None,
                args: None,
                url: Some(String::new()),
                env: HashMap::new(),
            },
        );
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "mcp_servers.bad.url"));
    }

    #[test]
    fn test_mcp_sse_invalid_url_error() {
        let mut config = make_valid_config();
        config.mcp_servers.insert(
            "bad".to_string(),
            McpServerConfig {
                transport: "http".to_string(),
                command: None,
                args: None,
                url: Some("not-a-url".to_string()),
                env: HashMap::new(),
            },
        );
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "mcp_servers.bad.url"));
    }

    #[test]
    fn test_feishu_missing_app_id() {
        let mut config = make_valid_config();
        config.channels.feishu = Some(FeishuConfig {
            app_id: None,
            app_secret: Some("secret".to_string()),
            enabled: true,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "channels.feishu.app_id"));
    }

    #[test]
    fn test_feishu_missing_app_secret() {
        let mut config = make_valid_config();
        config.channels.feishu = Some(FeishuConfig {
            app_id: Some("id".to_string()),
            app_secret: None,
            enabled: true,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "channels.feishu.app_secret"));
    }

    #[test]
    fn test_wecom_missing_fields() {
        let mut config = make_valid_config();
        config.channels.wecom = Some(WecomConfig {
            corp_id: None,
            agent_id: None,
            secret: None,
            token: None,
            encoding_aes_key: None,
            enabled: true,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        let errs: Vec<&str> = report.errors().iter().map(|e| e.path.as_str()).collect();
        assert!(errs.contains(&"channels.wecom.corp_id"));
        assert!(errs.contains(&"channels.wecom.secret"));
    }

    #[test]
    fn test_weixin_missing_app_id() {
        let mut config = make_valid_config();
        config.channels.weixin = Some(WeixinConfig {
            app_id: None,
            app_secret: None,
            token: None,
            encoding_aes_key: None,
            enabled: true,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "channels.weixin.app_id"));
    }

    #[test]
    fn test_qq_missing_app_id() {
        let mut config = make_valid_config();
        config.channels.qq = Some(QQConfig {
            app_id: None,
            app_secret: None,
            token: None,
            enabled: true,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "channels.qq.app_id"));
    }

    #[test]
    fn test_email_port_boundary_valid() {
        let mut config = make_valid_config();
        config.channels.email = Some(EmailConfig {
            imap_host: Some("imap.example.com".to_string()),
            smtp_host: Some("smtp.example.com".to_string()),
            username: Some("user".to_string()),
            password: None,
            port: 1,
            enabled: true,
        });
        let report = validate(&config);
        // port=1 is in range [1, 65535] but non-standard
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "channels.email.port" && w.message.contains("non-standard")));
    }

    #[test]
    fn test_security_blocked_networks_invalid() {
        let mut config = make_valid_config();
        config.security.blocked_networks = vec!["not-valid".to_string()];
        let report = validate(&config);
        assert!(report
            .warnings()
            .iter()
            .any(|w| w.path == "security.blocked_networks[0]"
                && w.message.contains("not a valid CIDR")));
    }

    // -------------------------------------------------------------------
    // WebSocket validation tests
    // -------------------------------------------------------------------

    #[test]
    fn test_websocket_valid_config() {
        let mut config = make_valid_config();
        config.channels.websocket = Some(WebSocketConfig {
            enabled: true,
            listen_addr: "127.0.0.1:8090".to_string(),
            auth: crate::schema::WsAuthConfig {
                required: false,
                token: None,
            },
            max_clients: 100,
            max_message_size: 1048576,
        });
        let report = validate(&config);
        assert!(report.is_valid(), "Unexpected errors: {}", report);
    }

    #[test]
    fn test_websocket_disabled_skips_validation() {
        let mut config = make_valid_config();
        config.channels.websocket = Some(WebSocketConfig {
            enabled: false,
            listen_addr: String::new(), // invalid but disabled
            auth: crate::schema::WsAuthConfig::default(),
            max_clients: 0,
            max_message_size: 0,
        });
        let report = validate(&config);
        assert!(report
            .errors()
            .iter()
            .all(|e| !e.path.starts_with("channels.websocket.")));
    }

    #[test]
    fn test_websocket_empty_listen_addr() {
        let mut config = make_valid_config();
        config.channels.websocket = Some(WebSocketConfig {
            enabled: true,
            listen_addr: String::new(),
            auth: crate::schema::WsAuthConfig::default(),
            max_clients: 100,
            max_message_size: 1024,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "channels.websocket.listen_addr"));
    }

    #[test]
    fn test_websocket_no_colon_in_addr() {
        let mut config = make_valid_config();
        config.channels.websocket = Some(WebSocketConfig {
            enabled: true,
            listen_addr: "no-port-here".to_string(),
            auth: crate::schema::WsAuthConfig::default(),
            max_clients: 100,
            max_message_size: 1024,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(
            report
                .errors()
                .iter()
                .any(|e| e.path == "channels.websocket.listen_addr"
                    && e.message.contains("host:port"))
        );
    }

    #[test]
    fn test_websocket_invalid_port() {
        let mut config = make_valid_config();
        config.channels.websocket = Some(WebSocketConfig {
            enabled: true,
            listen_addr: "127.0.0.1:abc".to_string(),
            auth: crate::schema::WsAuthConfig::default(),
            max_clients: 100,
            max_message_size: 1024,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "channels.websocket.listen_addr"
                && e.message.contains("Invalid port")));
    }

    #[test]
    fn test_websocket_port_zero_warns() {
        let mut config = make_valid_config();
        config.channels.websocket = Some(WebSocketConfig {
            enabled: true,
            listen_addr: "127.0.0.1:0".to_string(),
            auth: crate::schema::WsAuthConfig::default(),
            max_clients: 100,
            max_message_size: 1024,
        });
        let report = validate(&config);
        assert!(report.is_valid()); // just a warning
        assert!(report.warnings().iter().any(
            |w| w.path == "channels.websocket.listen_addr" && w.message.contains("random port")
        ));
    }

    #[test]
    fn test_websocket_zero_max_clients() {
        let mut config = make_valid_config();
        config.channels.websocket = Some(WebSocketConfig {
            enabled: true,
            listen_addr: "127.0.0.1:8090".to_string(),
            auth: crate::schema::WsAuthConfig::default(),
            max_clients: 0,
            max_message_size: 1024,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "channels.websocket.max_clients"));
    }

    #[test]
    fn test_websocket_high_max_clients_warns() {
        let mut config = make_valid_config();
        config.channels.websocket = Some(WebSocketConfig {
            enabled: true,
            listen_addr: "127.0.0.1:8090".to_string(),
            auth: crate::schema::WsAuthConfig::default(),
            max_clients: 20000,
            max_message_size: 1024,
        });
        let report = validate(&config);
        assert!(report.is_valid()); // just a warning
        assert!(
            report
                .warnings()
                .iter()
                .any(|w| w.path == "channels.websocket.max_clients"
                    && w.message.contains("very high"))
        );
    }

    #[test]
    fn test_websocket_zero_max_message_size() {
        let mut config = make_valid_config();
        config.channels.websocket = Some(WebSocketConfig {
            enabled: true,
            listen_addr: "127.0.0.1:8090".to_string(),
            auth: crate::schema::WsAuthConfig::default(),
            max_clients: 100,
            max_message_size: 0,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "channels.websocket.max_message_size"));
    }

    #[test]
    fn test_websocket_auth_required_no_token() {
        let mut config = make_valid_config();
        config.channels.websocket = Some(WebSocketConfig {
            enabled: true,
            listen_addr: "127.0.0.1:8090".to_string(),
            auth: crate::schema::WsAuthConfig {
                required: true,
                token: None,
            },
            max_clients: 100,
            max_message_size: 1024,
        });
        let report = validate(&config);
        assert!(!report.is_valid());
        assert!(report
            .errors()
            .iter()
            .any(|e| e.path == "channels.websocket.auth.token"));
    }

    #[test]
    fn test_websocket_auth_required_with_token() {
        let mut config = make_valid_config();
        config.channels.websocket = Some(WebSocketConfig {
            enabled: true,
            listen_addr: "127.0.0.1:8090".to_string(),
            auth: crate::schema::WsAuthConfig {
                required: true,
                token: Some("my-secret".to_string()),
            },
            max_clients: 100,
            max_message_size: 1024,
        });
        let report = validate(&config);
        assert!(report.is_valid(), "Unexpected errors: {}", report);
    }

    #[test]
    fn test_websocket_counts_as_enabled_channel() {
        let mut config = make_valid_config();
        config.channels.telegram = None; // disable telegram
        config.channels.discord = None; // disable discord
        config.channels.websocket = Some(WebSocketConfig {
            enabled: true,
            listen_addr: "127.0.0.1:8090".to_string(),
            auth: crate::schema::WsAuthConfig::default(),
            max_clients: 100,
            max_message_size: 1024,
        });
        let report = validate(&config);
        // Should NOT warn about "No channel is enabled"
        assert!(report.warnings().iter().all(|w| w.path != "channels"));
    }
}
