//! Python kestrel config migration to kestrel YAML format.
//!
//! Reads Python JSON/YAML configs, converts to kestrel [`Config`],
//! validates the result, and reports mapped/unmapped/ignored fields.
//!
//! ## Usage
//!
//! ```ignore
//! use kestrel_config::python_migrate::{migrate_from_python, MigrationOptions};
//! use std::path::Path;
//!
//! let opts = MigrationOptions {
//!     dry_run: true,
//!     ..Default::default()
//! };
//! let result = migrate_from_python(Path::new("~/.kestrel"), &opts).unwrap();
//! println!("{}", result.summary());
//! ```

use crate::loader::save_config;
use crate::python_schema::*;
use crate::schema::*;
use crate::validate::{validate, ValidationReport};
use anyhow::{Context, Result};
use std::fmt;
use std::path::Path;

// ---------------------------------------------------------------------------
// Migration options
// ---------------------------------------------------------------------------

/// Options controlling migration behaviour.
#[derive(Debug, Clone)]
pub struct MigrationOptions {
    /// If true, do not write the output file — just return the result.
    pub dry_run: bool,
    /// Explicit path to the Python config file (JSON or YAML).
    /// If `None`, auto-detects `config.json` then `config.yaml` in `python_home`.
    pub input_file: Option<std::path::PathBuf>,
    /// Output path for the kestrel config.yaml.
    /// If `None`, writes to `<KESTREL_HOME>/config.yaml`.
    pub output_file: Option<std::path::PathBuf>,
    /// Whether to fill defaults before validating.
    pub fill_defaults: bool,
}

impl Default for MigrationOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            input_file: None,
            output_file: None,
            fill_defaults: true,
        }
    }
}

impl MigrationOptions {
    /// Create options with dry-run enabled.
    pub fn dry_run() -> Self {
        Self {
            dry_run: true,
            ..Default::default()
        }
    }

    /// Create options with a specific output path.
    pub fn with_output(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            output_file: Some(path.into()),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Migration result
// ---------------------------------------------------------------------------

/// Aggregated result of a Python config migration.
#[derive(Debug)]
pub struct MigrationResult {
    /// The converted kestrel config.
    pub config: Config,
    /// Report of mapped, unmapped, and noted fields.
    pub report: MigrationReport,
    /// Post-migration validation results.
    pub validation: ValidationReport,
}

impl MigrationResult {
    /// Return a human-readable summary of the migration.
    pub fn summary(&self) -> String {
        let mut s = String::new();

        // Field mapping summary
        s.push_str(&format!("Migration Summary\n{}\n", "-".repeat(40)));
        s.push_str(&format!(
            "  Mapped fields:   {}\n",
            self.report.mapped.len()
        ));
        s.push_str(&format!(
            "  Unmapped fields: {}\n",
            self.report.unmapped.len()
        ));
        s.push_str(&format!("  Notes:           {}\n", self.report.notes.len()));

        // Validation summary
        let errors = self.validation.errors().len();
        let warnings = self.validation.warnings().len();
        if errors > 0 || warnings > 0 {
            s.push_str(&format!("\nValidation\n{}\n", "-".repeat(40)));
            s.push_str(&format!("  Errors:   {}\n", errors));
            s.push_str(&format!("  Warnings: {}\n", warnings));
        }

        if self.validation.is_valid() {
            s.push_str("\n  Status: VALID\n");
        } else {
            s.push_str("\n  Status: NEEDS ATTENTION\n");
        }

        s
    }
}

// ---------------------------------------------------------------------------
// Migration report
// ---------------------------------------------------------------------------

/// Migration-specific report with categorized findings.
#[derive(Debug, Clone, Default)]
pub struct MigrationReport {
    /// Fields that were successfully mapped (Python → RS).
    pub mapped: Vec<String>,
    /// Python fields with no kestrel equivalent.
    pub unmapped: Vec<String>,
    /// Fields with semantic differences or notes.
    pub notes: Vec<String>,
}

impl MigrationReport {
    fn add_mapped(&mut self, path: &str) {
        self.mapped.push(path.to_string());
    }

    fn add_unmapped(&mut self, path: &str, reason: &str) {
        self.unmapped.push(format!("{}: {}", path, reason));
    }

    fn add_note(&mut self, path: &str, note: &str) {
        self.notes.push(format!("{}: {}", path, note));
    }

    /// Number of successfully mapped fields.
    pub fn mapped_count(&self) -> usize {
        self.mapped.len()
    }

    /// Number of unmapped Python fields.
    pub fn unmapped_count(&self) -> usize {
        self.unmapped.len()
    }

    /// Number of informational notes.
    pub fn notes_count(&self) -> usize {
        self.notes.len()
    }
}

impl fmt::Display for MigrationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.mapped.is_empty() {
            writeln!(f, "Mapped fields ({}):", self.mapped.len())?;
            for field in &self.mapped {
                writeln!(f, "  [OK] {}", field)?;
            }
        }

        if !self.unmapped.is_empty() {
            writeln!(f, "\nUnmapped fields ({}):", self.unmapped.len())?;
            for field in &self.unmapped {
                writeln!(f, "  [SKIP] {}", field)?;
            }
        }

        if !self.notes.is_empty() {
            writeln!(f, "\nNotes ({}):", self.notes.len())?;
            for note in &self.notes {
                writeln!(f, "  [NOTE] {}", note)?;
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Known channels for per-channel discovery
// ---------------------------------------------------------------------------

/// Known channel names for per-channel config discovery.
const KNOWN_CHANNELS: &[&str] = &[
    "telegram", "discord", "slack", "matrix", "whatsapp", "email", "dingtalk", "feishu", "wecom",
    "weixin", "qq",
];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Migrate Python kestrel config directory to kestrel Config.
///
/// `python_home` is the Python kestrel config directory (e.g., `~/.kestrel`).
/// Reads `config.json` (or `config.yaml`) from that directory, plus any
/// per-channel configs at sibling directories like `~/.kestrel-telegram/config.json`.
///
/// Validates the result and optionally writes to disk (unless dry-run).
pub fn migrate_from_python(
    python_home: &Path,
    options: &MigrationOptions,
) -> Result<MigrationResult> {
    // 1. Read main config (JSON or YAML)
    let main_config = if let Some(ref input) = options.input_file {
        read_python_config(input)?
    } else {
        let json_path = python_home.join("config.json");
        let yaml_path = python_home.join("config.yaml");
        if json_path.exists() {
            read_python_config(&json_path)?
        } else if yaml_path.exists() {
            read_python_config(&yaml_path)?
        } else {
            anyhow::bail!(
                "No config.json or config.yaml found in {}",
                python_home.display()
            );
        }
    };

    // 2. Probe for per-channel config files
    let parent = python_home.parent().unwrap_or(python_home);
    let home_name = python_home
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut per_channel_configs: Vec<(String, PythonConfig)> = Vec::new();
    for ch in KNOWN_CHANNELS {
        let ch_dir_name = format!("{}-{}", home_name, ch);
        let ch_dir = parent.join(&ch_dir_name);

        // Try JSON first, then YAML
        for filename in &["config.json", "config.yaml"] {
            let ch_path = ch_dir.join(filename);
            if ch_path.exists() {
                tracing::info!("Found per-channel config: {}", ch_path.display());
                match read_python_config(&ch_path) {
                    Ok(cfg) => {
                        per_channel_configs.push((ch.to_string(), cfg));
                        break; // Found config for this channel, move to next
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to read per-channel config {}: {}",
                            ch_path.display(),
                            e
                        );
                    }
                }
            }
        }
    }

    // 3. Convert Python config → RS config
    let mut report = MigrationReport::default();
    let mut config = convert_python_config(&main_config, &per_channel_configs, &mut report);

    // 4. Fill defaults (optional but recommended)
    if options.fill_defaults {
        crate::validate::fill_defaults(&mut config);
        report.add_note("_config_version", "set to 4 (kestrel current)");
    }

    // 5. Validate the migrated config
    let validation = validate(&config);

    // 6. Write to disk (unless dry-run)
    if !options.dry_run {
        let output_path = match &options.output_file {
            Some(p) => p.clone(),
            None => {
                let config_path = crate::paths::get_config_path()?;
                if let Some(parent) = config_path.parent() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("Failed to create config dir: {}", parent.display())
                    })?;
                }
                config_path
            }
        };
        save_config(&config, &output_path)?;
        tracing::info!("Migrated config written to {}", output_path.display());
    }

    Ok(MigrationResult {
        config,
        report,
        validation,
    })
}

/// Migrate from raw JSON/YAML string (no filesystem access).
///
/// Useful for testing or when the config content is already in memory.
/// Does not probe for per-channel configs or write to disk.
pub fn migrate_from_str(content: &str) -> Result<MigrationResult> {
    let py: PythonConfig = if content.trim_start().starts_with('{') {
        serde_json::from_str(content).with_context(|| "Failed to parse as JSON")?
    } else {
        serde_yaml::from_str(content).with_context(|| "Failed to parse as YAML")?
    };

    let mut report = MigrationReport::default();
    let mut config = convert_python_config(&py, &[], &mut report);

    if report.fill_defaults_needed(&config) {
        crate::validate::fill_defaults(&mut config);
    }

    let validation = validate(&config);

    Ok(MigrationResult {
        config,
        report,
        validation,
    })
}

impl MigrationReport {
    /// Check if fill_defaults would change anything (internal helper).
    fn fill_defaults_needed(&self, config: &Config) -> bool {
        config.agent.model.is_empty()
            || config.agent.max_tokens == 0
            || config.agent.max_iterations == 0
            || config.agent.tool_timeout == 0
    }
}

// ---------------------------------------------------------------------------
// Config reading
// ---------------------------------------------------------------------------

/// Read and parse a Python config file (JSON or YAML, detected by extension).
fn read_python_config(path: &Path) -> Result<PythonConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read Python config: {}", path.display()))?;

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "json" => serde_json::from_str(&raw)
            .with_context(|| format!("Failed to parse Python config JSON: {}", path.display())),
        "yaml" | "yml" => serde_yaml::from_str(&raw)
            .with_context(|| format!("Failed to parse Python config YAML: {}", path.display())),
        _ => {
            // Auto-detect: try JSON first, then YAML
            if raw.trim_start().starts_with('{') {
                serde_json::from_str(&raw).with_context(|| {
                    format!("Failed to parse Python config JSON: {}", path.display())
                })
            } else {
                serde_yaml::from_str(&raw).with_context(|| {
                    format!("Failed to parse Python config YAML: {}", path.display())
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Conversion: Python config → RS config
// ---------------------------------------------------------------------------

/// Convert a Python config (plus per-channel overrides) to kestrel Config.
fn convert_python_config(
    main: &PythonConfig,
    per_channel: &[(String, PythonConfig)],
    report: &mut MigrationReport,
) -> Config {
    let mut config = Config::default();

    convert_providers(&main.providers, &mut config, report);
    convert_channels(&main.channels, per_channel, &mut config, report);
    convert_agents(&main.agents, &mut config, report);
    convert_heartbeat(&main.heartbeat, &mut config, report);
    convert_security(&main.security, &mut config, report);
    convert_dream(&main.dream, &mut config, report);

    config
}

// ─── Provider conversion ────────────────────────────────────

fn convert_provider_entry(py: &PythonProviderEntry) -> ProviderEntry {
    ProviderEntry {
        api_key: py.api_key.clone(),
        base_url: py.api_base.clone(), // apiBase → base_url
        model: py.model.clone(),
        no_proxy: None,
    }
}

fn convert_providers(py: &PythonProviders, config: &mut Config, report: &mut MigrationReport) {
    if let Some(ref p) = py.openrouter {
        config.providers.openrouter = Some(convert_provider_entry(p));
        report.add_mapped("providers.openrouter (apiKey → api_key, apiBase → base_url)");
    }

    if let Some(ref p) = py.openai {
        config.providers.openai = Some(convert_provider_entry(p));
        report.add_mapped("providers.openai (apiKey → api_key, apiBase → base_url)");
    }

    if let Some(ref p) = py.anthropic {
        config.providers.anthropic = Some(convert_provider_entry(p));
        report.add_mapped("providers.anthropic (apiKey → api_key, apiBase → base_url)");
    }

    if let Some(ref p) = py.deepseek {
        config.providers.deepseek = Some(convert_provider_entry(p));
        report.add_mapped("providers.deepseek");
    }

    if let Some(ref p) = py.groq {
        config.providers.groq = Some(convert_provider_entry(p));
        report.add_mapped("providers.groq");
    }

    if let Some(ref p) = py.gemini {
        config.providers.gemini = Some(convert_provider_entry(p));
        report.add_mapped("providers.gemini");
    }

    if let Some(ref p) = py.ollama {
        config.providers.ollama = Some(convert_provider_entry(p));
        report.add_mapped("providers.ollama (apiBase → base_url)");
    }

    if let Some(ref p) = py.azure_openai {
        config.providers.azure_openai = Some(AzureOpenAIProviderEntry {
            api_key: p.api_key.clone(),
            endpoint: p.endpoint.clone(),
            deployment: p.deployment.clone(),
            api_version: p.api_version.clone(),
        });
        report.add_mapped("providers.azure_openai (direct field mapping)");
    }

    if let Some(ref p) = py.custom {
        if let Some(ref base_url) = p.api_base {
            config.custom_providers.push(CustomProviderConfig {
                name: "custom".to_string(),
                base_url: base_url.clone(),
                api_key: p.api_key.clone(),
                model_patterns: vec![],
                no_proxy: None,
            });
            report.add_mapped("providers.custom → custom_providers[0] (apiBase → base_url)");
        } else {
            report.add_note(
                "providers.custom",
                "No apiBase set, skipping custom provider",
            );
        }
    }
}

// ─── Channel conversion ─────────────────────────────────────

fn convert_channels(
    py: &PythonChannels,
    per_channel: &[(String, PythonConfig)],
    config: &mut Config,
    report: &mut MigrationReport,
) {
    // Telegram
    let mut telegram_py = py.telegram.clone();
    for (name, ch_cfg) in per_channel {
        if name == "telegram" {
            telegram_py = Some(merge_telegram(
                telegram_py.as_ref(),
                &ch_cfg.channels.telegram,
            ));
        }
    }
    if let Some(ref tg) = telegram_py {
        config.channels.telegram = Some(TelegramConfig {
            token: tg.token.clone().unwrap_or_default(),
            allowed_users: tg.allow_from.clone().unwrap_or_default(),
            admin_users: vec![],
            enabled: tg.enabled.unwrap_or(true),
            streaming: false,
        });
        report.add_mapped("channels.telegram (allowFrom → allowed_users, Vec<String>)");
    }

    // Discord
    let mut discord_py = py.discord.clone();
    for (name, ch_cfg) in per_channel {
        if name == "discord" {
            discord_py = Some(merge_discord(discord_py.as_ref(), &ch_cfg.channels.discord));
        }
    }
    if let Some(ref dc) = discord_py {
        config.channels.discord = Some(DiscordConfig {
            token: dc.token.clone().unwrap_or_default(),
            allowed_guilds: dc.allow_from.clone().unwrap_or_default(),
            enabled: dc.enabled.unwrap_or(true),
            streaming: dc.streaming.unwrap_or(false),
        });
        report.add_mapped("channels.discord (allowFrom → allowed_guilds, Vec<String>)");
        if dc.group_policy.is_some() {
            report.add_unmapped(
                "channels.discord.groupPolicy",
                "no kestrel equivalent (group behavior configured differently)",
            );
        }
    }

    // Slack
    let mut slack_py = py.slack.clone();
    for (name, ch_cfg) in per_channel {
        if name == "slack" {
            slack_py = Some(merge_slack(slack_py.as_ref(), &ch_cfg.channels.slack));
        }
    }
    if let Some(ref sl) = slack_py {
        config.channels.slack = Some(SlackConfig {
            bot_token: sl.bot_token.clone(),
            app_token: sl.app_token.clone(),
            signing_secret: None,
            enabled: sl.enabled.unwrap_or(true),
        });
        report.add_mapped("channels.slack (botToken → bot_token, appToken → app_token)");
        if sl.allow_from.is_some() {
            report.add_unmapped(
                "channels.slack.allowFrom",
                "no kestrel equivalent (user allowlisting not yet supported)",
            );
        }
    }

    // Matrix
    if let Some(ref m) = py.matrix {
        config.channels.matrix = Some(MatrixConfig {
            homeserver: m.homeserver.clone(),
            user_id: m.user_id.clone(),
            password: m.password.clone(),
            access_token: m.access_token.clone(),
            enabled: m.enabled.unwrap_or(true),
        });
        report.add_mapped("channels.matrix (direct field mapping)");
    }

    // Email
    if let Some(ref e) = py.email {
        config.channels.email = Some(EmailConfig {
            imap_host: e.imap_host.clone(),
            smtp_host: e.smtp_host.clone(),
            username: e.username.clone(),
            password: e.password.clone(),
            port: e.port.unwrap_or(0),
            enabled: e.enabled.unwrap_or(true),
        });
        report.add_mapped("channels.email (imapHost → imap_host, smtpHost → smtp_host)");
    }

    // DingTalk
    if let Some(ref d) = py.dingtalk {
        config.channels.dingtalk = Some(DingtalkConfig {
            webhook: d.webhook.clone(),
            secret: d.secret.clone(),
            enabled: d.enabled.unwrap_or(true),
        });
        report.add_mapped("channels.dingtalk");
    }

    // Feishu
    if let Some(ref f) = py.feishu {
        config.channels.feishu = Some(FeishuConfig {
            app_id: f.app_id.clone(),
            app_secret: f.app_secret.clone(),
            enabled: f.enabled.unwrap_or(true),
        });
        report.add_mapped("channels.feishu (appId → app_id, appSecret → app_secret)");
    }

    // WeCom
    if let Some(ref w) = py.wecom {
        config.channels.wecom = Some(WecomConfig {
            corp_id: w.corp_id.clone(),
            agent_id: w.agent_id.clone(),
            secret: w.secret.clone(),
            token: None,
            encoding_aes_key: None,
            enabled: w.enabled.unwrap_or(true),
        });
        report.add_mapped("channels.wecom (corpId → corp_id, agentId → agent_id)");
    }

    // WeChat
    if let Some(ref w) = py.weixin {
        config.channels.weixin = Some(WeixinConfig {
            app_id: w.app_id.clone(),
            app_secret: w.app_secret.clone(),
            token: None,
            encoding_aes_key: None,
            enabled: w.enabled.unwrap_or(true),
        });
        report.add_mapped("channels.weixin (appId → app_id, appSecret → app_secret)");
    }

    // QQ
    if let Some(ref q) = py.qq {
        config.channels.qq = Some(QQConfig {
            app_id: q.app_id.clone(),
            app_secret: q.app_secret.clone(),
            token: None,
            enabled: q.enabled.unwrap_or(true),
        });
        report.add_mapped("channels.qq (appId → app_id, appSecret → app_secret)");
    }
}

// ─── Merge helpers for per-channel configs ──────────────────

fn merge_telegram(
    base: Option<&PythonTelegramConfig>,
    override_cfg: &Option<PythonTelegramConfig>,
) -> PythonTelegramConfig {
    let base = base.cloned().unwrap_or_default();
    match override_cfg {
        Some(ov) => PythonTelegramConfig {
            enabled: ov.enabled.or(base.enabled),
            token: ov.token.clone().or(base.token),
            allow_from: ov.allow_from.clone().or(base.allow_from),
        },
        None => base,
    }
}

fn merge_discord(
    base: Option<&PythonDiscordConfig>,
    override_cfg: &Option<PythonDiscordConfig>,
) -> PythonDiscordConfig {
    let base = base.cloned().unwrap_or_default();
    match override_cfg {
        Some(ov) => PythonDiscordConfig {
            enabled: ov.enabled.or(base.enabled),
            token: ov.token.clone().or(base.token),
            allow_from: ov.allow_from.clone().or(base.allow_from),
            group_policy: ov.group_policy.clone().or(base.group_policy),
            streaming: ov.streaming.or(base.streaming),
        },
        None => base,
    }
}

fn merge_slack(
    base: Option<&PythonSlackConfig>,
    override_cfg: &Option<PythonSlackConfig>,
) -> PythonSlackConfig {
    let base = base.cloned().unwrap_or_default();
    match override_cfg {
        Some(ov) => PythonSlackConfig {
            enabled: ov.enabled.or(base.enabled),
            bot_token: ov.bot_token.clone().or(base.bot_token),
            app_token: ov.app_token.clone().or(base.app_token),
            allow_from: ov.allow_from.clone().or(base.allow_from),
        },
        None => base,
    }
}

// ─── Agent conversion ───────────────────────────────────────

fn convert_agents(py: &PythonAgents, config: &mut Config, report: &mut MigrationReport) {
    if let Some(ref defaults) = py.defaults {
        if let Some(ref model) = defaults.model {
            config.agent.model = model.clone();
        }
        if let Some(temp) = defaults.temperature {
            config.agent.temperature = temp;
        }
        if let Some(tokens) = defaults.max_tokens {
            config.agent.max_tokens = tokens;
        }
        if let Some(iters) = defaults.max_iterations {
            config.agent.max_iterations = iters;
        }
        if let Some(streaming) = defaults.streaming {
            config.agent.streaming = streaming;
        }
        if let Some(timeout) = defaults.tool_timeout {
            config.agent.tool_timeout = timeout;
        }
        report.add_mapped(
            "agents.defaults → agent (model, temperature, max_tokens, max_iterations, streaming, tool_timeout)",
        );

        if defaults.provider.is_some() {
            report.add_note(
                "agents.defaults.provider",
                "kestrel selects providers by model name convention, not explicit provider field",
            );
        }
    }
}

// ─── Heartbeat conversion ───────────────────────────────────

fn convert_heartbeat(py: &PythonHeartbeat, config: &mut Config, report: &mut MigrationReport) {
    if py.enabled.is_some() || py.interval_secs.is_some() {
        if let Some(enabled) = py.enabled {
            config.heartbeat.enabled = enabled;
        }
        if let Some(secs) = py.interval_secs {
            config.heartbeat.interval_secs = secs;
        }
        report.add_mapped("heartbeat (enabled, interval_secs → interval_secs)");
    }
}

// ─── Security conversion ────────────────────────────────────

fn convert_security(py: &PythonSecurity, config: &mut Config, report: &mut MigrationReport) {
    if py.block_private_ips.is_some() || py.ssrf_whitelist.is_some() {
        if let Some(block) = py.block_private_ips {
            config.security.block_private_ips = block;
        }
        if let Some(ref whitelist) = py.ssrf_whitelist {
            config.security.ssrf_whitelist = whitelist.clone();
        }
        report.add_mapped("security (block_private_ips, ssrf_whitelist → Vec<String>)");
    }
}

// ─── Dream conversion ───────────────────────────────────────

fn convert_dream(py: &PythonDream, config: &mut Config, report: &mut MigrationReport) {
    if py.enabled.is_some() || py.interval_secs.is_some() || py.model.is_some() {
        if let Some(enabled) = py.enabled {
            config.dream.enabled = enabled;
        }
        if let Some(secs) = py.interval_secs {
            config.dream.interval_secs = secs;
        }
        if let Some(ref model) = py.model {
            config.dream.model = Some(model.clone());
        }
        report.add_mapped("dream (enabled, interval_secs, model)");
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::python_schema::PythonConfig;
    use std::fs;

    // -------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------

    /// Build a fully-populated Python config JSON for testing.
    fn make_full_python_json() -> &'static str {
        r#"{
            "_config_version": 4,
            "providers": {
                "openrouter": {"apiKey": "sk-or-v1-test"},
                "openai": {"apiKey": "sk-openai-test", "model": "gpt-4o"},
                "anthropic": {"apiKey": "sk-ant-test"},
                "deepseek": {"apiKey": "sk-ds-test"},
                "groq": {"apiKey": "gsk_test"},
                "gemini": {"apiKey": "AIza_test"},
                "ollama": {"apiBase": "http://localhost:11434"},
                "azure_openai": {
                    "apiKey": "azure-key",
                    "endpoint": "https://my-resource.openai.azure.com",
                    "deployment": "gpt-4",
                    "api_version": "2024-02-15-preview"
                },
                "custom": {"apiKey": "custom-key", "apiBase": "https://custom.api/v1"}
            },
            "channels": {
                "telegram": {"enabled": true, "token": "123456:ABC-DEF", "allowFrom": ["111", "222"]},
                "discord": {"enabled": true, "token": "discord-token-here", "allowFrom": ["guild1"], "groupPolicy": "mention", "streaming": true},
                "slack": {"enabled": true, "botToken": "xoxb-123", "appToken": "xapp-456", "allowFrom": ["U123"]}
            },
            "agents": {
                "defaults": {
                    "model": "anthropic/claude-opus-4-5",
                    "provider": "openrouter",
                    "temperature": 0.5,
                    "max_tokens": 2048,
                    "max_iterations": 30,
                    "streaming": false,
                    "tool_timeout": 60
                }
            },
            "heartbeat": {"enabled": true, "interval_secs": 900},
            "security": {"block_private_ips": true, "ssrf_whitelist": ["10.0.0.0/8", "172.16.0.0/12"]},
            "dream": {"enabled": false, "interval_secs": 3600, "model": "gpt-4o-mini"}
        }"#
    }

    fn make_full_python_config() -> PythonConfig {
        serde_json::from_str(make_full_python_json()).unwrap()
    }

    // -------------------------------------------------------------------
    // MigrationReport tests
    // -------------------------------------------------------------------

    #[test]
    fn test_migration_report_display() {
        let mut report = MigrationReport::default();
        report.add_mapped("providers.openai");
        report.add_mapped("channels.telegram");
        report.add_unmapped("channels.discord.groupPolicy", "no equivalent");
        report.add_note("agents.defaults.provider", "auto-select by model name");

        let s = format!("{}", report);
        assert!(s.contains("Mapped fields (2)"));
        assert!(s.contains("[OK] providers.openai"));
        assert!(s.contains("[OK] channels.telegram"));
        assert!(s.contains("Unmapped fields (1)"));
        assert!(s.contains("[SKIP] channels.discord.groupPolicy"));
        assert!(s.contains("Notes (1)"));
        assert!(s.contains("[NOTE] agents.defaults.provider"));
    }

    #[test]
    fn test_migration_report_counts() {
        let mut report = MigrationReport::default();
        assert_eq!(report.mapped_count(), 0);
        assert_eq!(report.unmapped_count(), 0);
        assert_eq!(report.notes_count(), 0);

        report.add_mapped("a");
        report.add_mapped("b");
        report.add_unmapped("c", "reason");
        report.add_note("d", "note");

        assert_eq!(report.mapped_count(), 2);
        assert_eq!(report.unmapped_count(), 1);
        assert_eq!(report.notes_count(), 1);
    }

    // -------------------------------------------------------------------
    // MigrationOptions tests
    // -------------------------------------------------------------------

    #[test]
    fn test_migration_options_default() {
        let opts = MigrationOptions::default();
        assert!(!opts.dry_run);
        assert!(opts.input_file.is_none());
        assert!(opts.output_file.is_none());
        assert!(opts.fill_defaults);
    }

    #[test]
    fn test_migration_options_dry_run() {
        let opts = MigrationOptions::dry_run();
        assert!(opts.dry_run);
    }

    #[test]
    fn test_migration_options_with_output() {
        let opts = MigrationOptions::with_output("/tmp/test-config.yaml");
        assert_eq!(
            opts.output_file.unwrap().to_str(),
            Some("/tmp/test-config.yaml")
        );
    }

    // -------------------------------------------------------------------
    // Provider conversion tests
    // -------------------------------------------------------------------

    #[test]
    fn test_migrate_providers() {
        let py = make_full_python_config();
        let mut report = MigrationReport::default();
        let mut config = Config::default();
        convert_providers(&py.providers, &mut config, &mut report);

        assert!(config.providers.openrouter.is_some());
        assert_eq!(
            config.providers.openrouter.as_ref().unwrap().api_key,
            Some("sk-or-v1-test".to_string())
        );

        assert!(config.providers.openai.is_some());
        assert_eq!(
            config.providers.openai.as_ref().unwrap().api_key,
            Some("sk-openai-test".to_string())
        );
        assert_eq!(
            config.providers.openai.as_ref().unwrap().model,
            Some("gpt-4o".to_string())
        );

        assert!(config.providers.anthropic.is_some());
        assert!(config.providers.deepseek.is_some());
        assert!(config.providers.groq.is_some());
        assert!(config.providers.gemini.is_some());

        // Ollama: apiBase → base_url
        assert!(config.providers.ollama.is_some());
        assert_eq!(
            config.providers.ollama.as_ref().unwrap().base_url,
            Some("http://localhost:11434".to_string())
        );

        // Azure OpenAI
        let azure = config.providers.azure_openai.as_ref().unwrap();
        assert_eq!(azure.api_key, Some("azure-key".to_string()));
        assert_eq!(azure.deployment, Some("gpt-4".to_string()));

        // Custom → custom_providers[0]
        assert_eq!(config.custom_providers.len(), 1);
        assert_eq!(config.custom_providers[0].base_url, "https://custom.api/v1");
    }

    // -------------------------------------------------------------------
    // Channel conversion tests
    // -------------------------------------------------------------------

    #[test]
    fn test_migrate_telegram_channel() {
        let py = make_full_python_config();
        let mut report = MigrationReport::default();
        let mut config = Config::default();
        convert_channels(&py.channels, &[], &mut config, &mut report);

        let tg = config.channels.telegram.as_ref().unwrap();
        assert_eq!(tg.token, "123456:ABC-DEF");
        assert_eq!(tg.allowed_users, vec!["111", "222"]);
        assert!(tg.enabled);
        assert!(tg.admin_users.is_empty());
    }

    #[test]
    fn test_migrate_discord_with_group_policy_warning() {
        let py = make_full_python_config();
        let mut report = MigrationReport::default();
        let mut config = Config::default();
        convert_channels(&py.channels, &[], &mut config, &mut report);

        let dc = config.channels.discord.as_ref().unwrap();
        assert_eq!(dc.token, "discord-token-here");
        assert_eq!(dc.allowed_guilds, vec!["guild1"]);
        assert!(dc.streaming);

        assert!(report
            .unmapped
            .iter()
            .any(|u| u.contains("channels.discord.groupPolicy")));
    }

    #[test]
    fn test_migrate_slack_channel() {
        let py = make_full_python_config();
        let mut report = MigrationReport::default();
        let mut config = Config::default();
        convert_channels(&py.channels, &[], &mut config, &mut report);

        let sl = config.channels.slack.as_ref().unwrap();
        assert_eq!(sl.bot_token, Some("xoxb-123".to_string()));
        assert_eq!(sl.app_token, Some("xapp-456".to_string()));

        assert!(report
            .unmapped
            .iter()
            .any(|u| u.contains("channels.slack.allowFrom")));
    }

    #[test]
    fn test_migrate_all_channels() {
        let json = r#"{
            "channels": {
                "telegram": {"token": "tg-token"},
                "discord": {"token": "dc-token"},
                "slack": {"botToken": "xoxb-1"},
                "matrix": {"homeserver": "https://matrix.org", "userId": "@bot:matrix.org", "accessToken": "token"},
                "email": {"imapHost": "imap.example.com", "smtpHost": "smtp.example.com", "username": "user@test.com", "password": "pass"},
                "dingtalk": {"webhook": "https://dt.webhook", "secret": "dt-secret"},
                "feishu": {"appId": "fs-app", "appSecret": "fs-secret"},
                "wecom": {"corpId": "wc-corp", "agentId": "wc-agent", "secret": "wc-secret"},
                "weixin": {"appId": "wx-app", "appSecret": "wx-secret"},
                "qq": {"appId": "qq-app", "appSecret": "qq-secret"}
            }
        }"#;
        let py: PythonConfig = serde_json::from_str(json).unwrap();
        let mut report = MigrationReport::default();
        let mut config = Config::default();
        convert_channels(&py.channels, &[], &mut config, &mut report);

        assert!(config.channels.telegram.is_some());
        assert!(config.channels.discord.is_some());
        assert!(config.channels.slack.is_some());
        assert!(config.channels.matrix.is_some());
        assert!(config.channels.email.is_some());
        assert!(config.channels.dingtalk.is_some());
        assert!(config.channels.feishu.is_some());
        assert!(config.channels.wecom.is_some());
        assert!(config.channels.weixin.is_some());
        assert!(config.channels.qq.is_some());

        assert_eq!(
            config.channels.matrix.as_ref().unwrap().homeserver,
            Some("https://matrix.org".to_string())
        );
        assert_eq!(
            config.channels.dingtalk.as_ref().unwrap().webhook,
            Some("https://dt.webhook".to_string())
        );
        assert_eq!(
            config.channels.feishu.as_ref().unwrap().app_id,
            Some("fs-app".to_string())
        );
    }

    // -------------------------------------------------------------------
    // Agent, heartbeat, security, dream conversion tests
    // -------------------------------------------------------------------

    #[test]
    fn test_migrate_agents_defaults() {
        let py = make_full_python_config();
        let mut report = MigrationReport::default();
        let mut config = Config::default();
        convert_agents(&py.agents, &mut config, &mut report);

        assert_eq!(config.agent.model, "anthropic/claude-opus-4-5");
        assert!((config.agent.temperature - 0.5).abs() < f32::EPSILON);
        assert_eq!(config.agent.max_tokens, 2048);
        assert_eq!(config.agent.max_iterations, 30);
        assert!(!config.agent.streaming);
        assert_eq!(config.agent.tool_timeout, 60);

        assert!(report
            .notes
            .iter()
            .any(|n| n.contains("agents.defaults.provider")));
    }

    #[test]
    fn test_migrate_heartbeat() {
        let py = make_full_python_config();
        let mut report = MigrationReport::default();
        let mut config = Config::default();
        convert_heartbeat(&py.heartbeat, &mut config, &mut report);

        assert!(config.heartbeat.enabled);
        assert_eq!(config.heartbeat.interval_secs, 900);
    }

    #[test]
    fn test_migrate_security() {
        let py = make_full_python_config();
        let mut report = MigrationReport::default();
        let mut config = Config::default();
        convert_security(&py.security, &mut config, &mut report);

        assert!(config.security.block_private_ips);
        assert_eq!(
            config.security.ssrf_whitelist,
            vec!["10.0.0.0/8".to_string(), "172.16.0.0/12".to_string()]
        );
    }

    #[test]
    fn test_migrate_dream() {
        let py = make_full_python_config();
        let mut report = MigrationReport::default();
        let mut config = Config::default();
        convert_dream(&py.dream, &mut config, &mut report);

        assert!(!config.dream.enabled);
        assert_eq!(config.dream.interval_secs, 3600);
        assert_eq!(config.dream.model, Some("gpt-4o-mini".to_string()));
    }

    // -------------------------------------------------------------------
    // Edge case tests
    // -------------------------------------------------------------------

    #[test]
    fn test_migrate_custom_provider_no_base() {
        let json = r#"{"providers": {"custom": {"apiKey": "key-only"}}}"#;
        let py: PythonConfig = serde_json::from_str(json).unwrap();
        let mut report = MigrationReport::default();
        let mut config = Config::default();
        convert_providers(&py.providers, &mut config, &mut report);

        assert!(config.custom_providers.is_empty());
        assert!(report.notes.iter().any(|n| n.contains("No apiBase set")));
    }

    #[test]
    fn test_migrate_empty_config() {
        let json = "{}";
        let py: PythonConfig = serde_json::from_str(json).unwrap();
        let mut report = MigrationReport::default();
        let config = convert_python_config(&py, &[], &mut report);

        assert_eq!(config.agent.model, "gpt-4o");
        assert!(config.providers.openai.is_none());
        assert!(report.mapped.is_empty());
        assert!(report.unmapped.is_empty());
    }

    #[test]
    fn test_migrate_no_sections() {
        let json = r#"{"providers": {"openai": {"apiKey": "sk-test"}}}"#;
        let py: PythonConfig = serde_json::from_str(json).unwrap();
        let mut report = MigrationReport::default();
        let config = convert_python_config(&py, &[], &mut report);

        assert!(config.providers.openai.is_some());
        assert!(config.channels.telegram.is_none());
        assert!(!config.heartbeat.enabled);
    }

    #[test]
    fn test_migrate_azure_openai() {
        let json = r#"{
            "providers": {
                "azure_openai": {
                    "apiKey": "azure-key-123",
                    "endpoint": "https://my-aoai.openai.azure.com",
                    "deployment": "gpt-4-turbo",
                    "api_version": "2024-06-01"
                }
            }
        }"#;
        let py: PythonConfig = serde_json::from_str(json).unwrap();
        let mut report = MigrationReport::default();
        let mut config = Config::default();
        convert_providers(&py.providers, &mut config, &mut report);

        let azure = config.providers.azure_openai.as_ref().unwrap();
        assert_eq!(azure.api_key, Some("azure-key-123".to_string()));
        assert_eq!(azure.deployment, Some("gpt-4-turbo".to_string()));
    }

    // -------------------------------------------------------------------
    // YAML output roundtrip
    // -------------------------------------------------------------------

    #[test]
    fn test_migrate_yaml_output_valid() {
        let py = make_full_python_config();
        let mut report = MigrationReport::default();
        let config = convert_python_config(&py, &[], &mut report);

        let yaml = serde_yaml::to_string(&config).unwrap();
        let parsed: Config = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(parsed.agent.model, "anthropic/claude-opus-4-5");
        assert!(parsed.providers.openai.is_some());
        assert!(parsed.channels.telegram.is_some());
        assert_eq!(
            parsed.channels.telegram.as_ref().unwrap().token,
            "123456:ABC-DEF"
        );
    }

    // -------------------------------------------------------------------
    // Per-channel merge
    // -------------------------------------------------------------------

    #[test]
    fn test_migrate_per_channel_merge() {
        let main: PythonConfig = serde_json::from_str("{}").unwrap();
        let per_telegram: PythonConfig = serde_json::from_str(
            r#"{"channels": {"telegram": {"enabled": true, "token": "from-per-channel:XYZ"}}}"#,
        )
        .unwrap();
        let per_channel = vec![("telegram".to_string(), per_telegram)];

        let mut report = MigrationReport::default();
        let config = convert_python_config(&main, &per_channel, &mut report);

        let tg = config.channels.telegram.as_ref().unwrap();
        assert_eq!(tg.token, "from-per-channel:XYZ");
        assert!(tg.enabled);
    }

    // -------------------------------------------------------------------
    // migrate_from_str — JSON input
    // -------------------------------------------------------------------

    #[test]
    fn test_migrate_from_str_json() {
        let result = migrate_from_str(make_full_python_json()).unwrap();

        assert_eq!(result.config.agent.model, "anthropic/claude-opus-4-5");
        assert!(result.config.providers.openai.is_some());
        assert!(result.config.channels.telegram.is_some());
        assert!(!result.report.mapped.is_empty());
    }

    // -------------------------------------------------------------------
    // migrate_from_str — YAML input
    // -------------------------------------------------------------------

    #[test]
    fn test_migrate_from_str_yaml() {
        // Python kestrel can also output YAML configs
        let yaml = r#"
providers:
  openai:
    apiKey: sk-yaml-test
    model: gpt-4o
channels:
  telegram:
    enabled: true
    token: "999888:YAML"
agents:
  defaults:
    model: gpt-4o
    temperature: 0.8
    max_tokens: 8192
heartbeat:
  enabled: true
  interval_secs: 600
"#;
        let result = migrate_from_str(yaml).unwrap();

        assert_eq!(result.config.agent.model, "gpt-4o");
        assert!(result.config.providers.openai.is_some());
        assert_eq!(
            result.config.providers.openai.as_ref().unwrap().api_key,
            Some("sk-yaml-test".to_string())
        );
        assert!(result.config.channels.telegram.is_some());
        assert_eq!(
            result.config.channels.telegram.as_ref().unwrap().token,
            "999888:YAML"
        );
        assert!(result.config.heartbeat.enabled);
        assert_eq!(result.config.heartbeat.interval_secs, 600);
    }

    // -------------------------------------------------------------------
    // migrate_from_str — empty input
    // -------------------------------------------------------------------

    #[test]
    fn test_migrate_from_str_empty() {
        let result = migrate_from_str("{}").unwrap();
        assert!(result.report.mapped.is_empty());
        assert!(!result.validation.is_valid()); // no provider configured
    }

    // -------------------------------------------------------------------
    // Post-migration validation integration
    // -------------------------------------------------------------------

    #[test]
    fn test_validation_after_full_migration() {
        let result = migrate_from_str(make_full_python_json()).unwrap();

        // Full config should validate OK (has providers, channels, agent)
        assert!(
            result.validation.is_valid(),
            "Validation errors: {:?}",
            result.validation.errors()
        );
    }

    #[test]
    fn test_validation_catches_missing_provider() {
        let json = r#"{
            "channels": {
                "telegram": {"enabled": true, "token": "123:ABC"}
            }
        }"#;
        let result = migrate_from_str(json).unwrap();

        // No provider → validation should catch this
        assert!(!result.validation.is_valid());
        assert!(result
            .validation
            .errors()
            .iter()
            .any(|e| e.path == "providers"));
    }

    #[test]
    fn test_validation_catches_bad_temperature() {
        let json = r#"{
            "providers": {"openai": {"apiKey": "sk-test"}},
            "agents": {
                "defaults": {
                    "temperature": 5.0
                }
            }
        }"#;
        let result = migrate_from_str(json).unwrap();

        assert!(!result.validation.is_valid());
        assert!(result
            .validation
            .errors()
            .iter()
            .any(|e| e.path == "agent.temperature"));
    }

    // -------------------------------------------------------------------
    // MigrationResult::summary
    // -------------------------------------------------------------------

    #[test]
    fn test_migration_result_summary() {
        let result = migrate_from_str(make_full_python_json()).unwrap();
        let summary = result.summary();

        assert!(summary.contains("Mapped fields"));
        assert!(summary.contains("Unmapped fields"));
        assert!(summary.contains("Notes"));
        assert!(summary.contains("VALID"));
    }

    #[test]
    fn test_migration_result_summary_with_errors() {
        let result = migrate_from_str("{}").unwrap();
        let summary = result.summary();

        assert!(summary.contains("Errors:"));
        assert!(summary.contains("NEEDS ATTENTION"));
    }

    // -------------------------------------------------------------------
    // Filesystem migration: migrate_from_python with temp dirs
    // -------------------------------------------------------------------

    #[test]
    fn test_migrate_from_python_json_file() {
        let tmp = tempfile::tempdir().unwrap();
        let py_home = tmp.path().join("kestrel");
        fs::create_dir_all(&py_home).unwrap();

        fs::write(py_home.join("config.json"), make_full_python_json()).unwrap();

        let opts = MigrationOptions {
            dry_run: true,
            input_file: Some(py_home.join("config.json")),
            ..Default::default()
        };

        let result = migrate_from_python(&py_home, &opts).unwrap();
        assert_eq!(result.config.agent.model, "anthropic/claude-opus-4-5");
        assert!(result.config.providers.openai.is_some());
    }

    #[test]
    fn test_migrate_from_python_yaml_file() {
        let tmp = tempfile::tempdir().unwrap();
        let py_home = tmp.path().join("kestrel");
        fs::create_dir_all(&py_home).unwrap();

        let yaml_content = r#"
providers:
  openai:
    apiKey: sk-yaml-file-test
channels:
  telegram:
    enabled: true
    token: "111222:YAML-FILE"
"#;
        fs::write(py_home.join("config.yaml"), yaml_content).unwrap();

        let opts = MigrationOptions {
            dry_run: true,
            ..Default::default()
        };

        let result = migrate_from_python(&py_home, &opts).unwrap();
        assert!(result.config.providers.openai.is_some());
        assert_eq!(
            result.config.providers.openai.as_ref().unwrap().api_key,
            Some("sk-yaml-file-test".to_string())
        );
        assert!(result.config.channels.telegram.is_some());
        assert_eq!(
            result.config.channels.telegram.as_ref().unwrap().token,
            "111222:YAML-FILE"
        );
    }

    #[test]
    fn test_migrate_from_python_no_config_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let py_home = tmp.path().join("empty_kestrel");
        fs::create_dir_all(&py_home).unwrap();

        let opts = MigrationOptions {
            dry_run: true,
            ..Default::default()
        };

        let result = migrate_from_python(&py_home, &opts);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No config.json or config.yaml"));
    }

    #[test]
    fn test_migrate_from_python_auto_detect_json() {
        let tmp = tempfile::tempdir().unwrap();
        let py_home = tmp.path().join("kestrel");
        fs::create_dir_all(&py_home).unwrap();

        // Only JSON exists — should auto-detect
        fs::write(
            py_home.join("config.json"),
            r#"{"providers": {"openai": {"apiKey": "sk-auto-json"}}}"#,
        )
        .unwrap();

        let opts = MigrationOptions {
            dry_run: true,
            ..Default::default()
        };

        let result = migrate_from_python(&py_home, &opts).unwrap();
        assert!(result.config.providers.openai.is_some());
    }

    #[test]
    fn test_migrate_from_python_auto_detect_yaml_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let py_home = tmp.path().join("kestrel");
        fs::create_dir_all(&py_home).unwrap();

        // Only YAML exists — should auto-detect as fallback
        fs::write(
            py_home.join("config.yaml"),
            "providers:\n  openai:\n    apiKey: sk-auto-yaml\n",
        )
        .unwrap();

        let opts = MigrationOptions {
            dry_run: true,
            ..Default::default()
        };

        let result = migrate_from_python(&py_home, &opts).unwrap();
        assert!(result.config.providers.openai.is_some());
        assert_eq!(
            result.config.providers.openai.as_ref().unwrap().api_key,
            Some("sk-auto-yaml".to_string())
        );
    }

    // -------------------------------------------------------------------
    // Dry-run: does NOT write file
    // -------------------------------------------------------------------

    #[test]
    fn test_dry_run_does_not_write() {
        let tmp = tempfile::tempdir().unwrap();
        let py_home = tmp.path().join("kestrel");
        fs::create_dir_all(&py_home).unwrap();

        fs::write(
            py_home.join("config.json"),
            r#"{"providers": {"openai": {"apiKey": "sk-dry-run"}}}"#,
        )
        .unwrap();

        let output = tmp.path().join("output.yaml");
        let opts = MigrationOptions {
            dry_run: true,
            output_file: Some(output.clone()),
            ..Default::default()
        };

        migrate_from_python(&py_home, &opts).unwrap();

        // Output file should NOT exist
        assert!(!output.exists(), "dry-run should not create output file");
    }

    // -------------------------------------------------------------------
    // Non-dry-run: writes file
    // -------------------------------------------------------------------

    #[test]
    fn test_write_creates_output_file() {
        let tmp = tempfile::tempdir().unwrap();
        let py_home = tmp.path().join("kestrel");
        fs::create_dir_all(&py_home).unwrap();

        fs::write(
            py_home.join("config.json"),
            r#"{"providers": {"openai": {"apiKey": "sk-write-test"}}, "channels": {"telegram": {"token": "123:ABC", "enabled": true}}}"#,
        )
        .unwrap();

        let output = tmp.path().join("migrated.yaml");
        let opts = MigrationOptions {
            dry_run: false,
            output_file: Some(output.clone()),
            ..Default::default()
        };

        migrate_from_python(&py_home, &opts).unwrap();

        // Output file should exist and be valid YAML
        assert!(output.exists());
        let content = fs::read_to_string(&output).unwrap();
        let parsed: Config = serde_yaml::from_str(&content).unwrap();
        assert!(parsed.providers.openai.is_some());
    }

    // -------------------------------------------------------------------
    // Per-channel config file discovery
    // -------------------------------------------------------------------

    #[test]
    fn test_per_channel_yaml_discovery() {
        let tmp = tempfile::tempdir().unwrap();
        let py_home = tmp.path().join("kestrel");
        fs::create_dir_all(&py_home).unwrap();

        // Main config: no channels
        fs::write(py_home.join("config.json"), "{}").unwrap();

        // Per-channel telegram config in YAML
        let tg_dir = tmp.path().join("kestrel-telegram");
        fs::create_dir_all(&tg_dir).unwrap();
        fs::write(
            tg_dir.join("config.yaml"),
            "channels:\n  telegram:\n    enabled: true\n    token: '777888:PER-YAML'\n",
        )
        .unwrap();

        let opts = MigrationOptions {
            dry_run: true,
            ..Default::default()
        };

        let result = migrate_from_python(&py_home, &opts).unwrap();
        assert!(result.config.channels.telegram.is_some());
        assert_eq!(
            result.config.channels.telegram.as_ref().unwrap().token,
            "777888:PER-YAML"
        );
    }

    // -------------------------------------------------------------------
    // JSON roundtrip through serde
    // -------------------------------------------------------------------

    #[test]
    fn test_migrate_json_roundtrip() {
        let py = make_full_python_config();
        let json = serde_json::to_string(&py).unwrap();
        let py2: PythonConfig = serde_json::from_str(&json).unwrap();

        let mut report = MigrationReport::default();
        let config = convert_python_config(&py2, &[], &mut report);

        assert_eq!(config.agent.model, "anthropic/claude-opus-4-5");
        assert!(config.providers.openai.is_some());
    }

    // -------------------------------------------------------------------
    // read_python_config — auto-detect format
    // -------------------------------------------------------------------

    #[test]
    fn test_read_python_config_json_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        fs::write(
            &path,
            r#"{"providers": {"openai": {"apiKey": "sk-ext-test"}}}"#,
        )
        .unwrap();

        let config = read_python_config(&path).unwrap();
        assert!(config.providers.openai.is_some());
    }

    #[test]
    fn test_read_python_config_yaml_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.yaml");
        fs::write(&path, "providers:\n  openai:\n    apiKey: sk-yaml-ext\n").unwrap();

        let config = read_python_config(&path).unwrap();
        assert!(config.providers.openai.is_some());
    }

    #[test]
    fn test_read_python_config_yml_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.yml");
        fs::write(&path, "providers:\n  openai:\n    apiKey: sk-yml-ext\n").unwrap();

        let config = read_python_config(&path).unwrap();
        assert!(config.providers.openai.is_some());
    }

    #[test]
    fn test_read_python_config_no_extension_auto_detect() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config");
        fs::write(
            &path,
            r#"{"providers": {"openai": {"apiKey": "sk-noext"}}}"#,
        )
        .unwrap();

        let config = read_python_config(&path).unwrap();
        assert!(config.providers.openai.is_some());
    }
}
