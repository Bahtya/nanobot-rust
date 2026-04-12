//! Python nanobot config migration to nanobot-rs YAML format.
//!
//! Reads Python JSON configs, converts to nanobot-rs [`Config`],
//! and reports unmapped or incompatible fields as warnings.

use crate::python_schema::*;
use crate::schema::*;
use anyhow::{Context, Result};
use std::path::Path;

/// Aggregated result of a Python config migration.
pub struct MigrationResult {
    /// The converted nanobot-rs config.
    pub config: Config,
    /// Report of mapped, unmapped, and noted fields.
    pub report: MigrationReport,
}

/// Migration-specific report with categorized findings.
#[derive(Debug, Clone, Default)]
pub struct MigrationReport {
    /// Fields that were successfully mapped.
    pub mapped: Vec<String>,
    /// Python fields with no nanobot-rs equivalent.
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
}

/// Known channel names for per-channel config discovery.
const KNOWN_CHANNELS: &[&str] = &[
    "telegram",
    "discord",
    "slack",
    "matrix",
    "whatsapp",
    "email",
    "dingtalk",
    "feishu",
    "wecom",
    "weixin",
    "qq",
];

/// Migrate Python nanobot config directory to nanobot-rs Config.
///
/// `python_home` is the Python nanobot config directory (e.g., `~/.nanobot`).
/// Reads `config.json` from that directory, plus any per-channel configs at
/// sibling directories like `~/.nanobot-telegram/config.json`.
pub fn migrate_from_python(python_home: &Path) -> Result<MigrationResult> {
    let mut report = MigrationReport::default();

    // 1. Read main config.json
    let main_config_path = python_home.join("config.json");
    let main_config = read_python_config(&main_config_path)?;

    // 2. Probe for per-channel config files at sibling directories
    //    If python_home is ~/.nanobot, per-channel is ~/.nanobot-telegram/config.json
    let parent = python_home.parent().unwrap_or(python_home);
    let home_name = python_home
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut per_channel_configs: Vec<(String, PythonConfig)> = Vec::new();
    for ch in KNOWN_CHANNELS {
        let ch_dir_name = format!("{}-{}", home_name, ch);
        let ch_path = parent.join(&ch_dir_name).join("config.json");
        if ch_path.exists() {
            tracing::info!("Found per-channel config: {}", ch_path.display());
            match read_python_config(&ch_path) {
                Ok(cfg) => per_channel_configs.push((ch.to_string(), cfg)),
                Err(e) => {
                    tracing::warn!("Failed to read per-channel config {}: {}", ch_path.display(), e);
                }
            }
        }
    }

    // 3. Convert
    let config = convert_python_config(&main_config, &per_channel_configs, &mut report);

    Ok(MigrationResult { config, report })
}

/// Read and parse a Python config.json file.
fn read_python_config(path: &Path) -> Result<PythonConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read Python config: {}", path.display()))?;
    let config: PythonConfig = serde_json::from_str(&raw)
        .with_context(|| format!("Failed to parse Python config JSON: {}", path.display()))?;
    Ok(config)
}

/// Convert a Python config (plus per-channel overrides) to nanobot-rs Config.
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
        base_url: py.api_base.clone(),
        model: py.model.clone(),
        no_proxy: None,
    }
}

fn convert_providers(py: &PythonProviders, config: &mut Config, report: &mut MigrationReport) {
    if let Some(ref p) = py.openrouter {
        config.providers.openrouter = Some(convert_provider_entry(p));
        report.add_mapped("providers.openrouter");
    }

    if let Some(ref p) = py.openai {
        config.providers.openai = Some(convert_provider_entry(p));
        report.add_mapped("providers.openai");
    }

    if let Some(ref p) = py.anthropic {
        config.providers.anthropic = Some(convert_provider_entry(p));
        report.add_mapped("providers.anthropic");
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
        report.add_mapped("providers.ollama");
    }

    if let Some(ref p) = py.azure_openai {
        config.providers.azure_openai = Some(AzureOpenAIProviderEntry {
            api_key: p.api_key.clone(),
            endpoint: p.endpoint.clone(),
            deployment: p.deployment.clone(),
            api_version: p.api_version.clone(),
        });
        report.add_mapped("providers.azure_openai");
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
            report.add_mapped("providers.custom -> custom_providers[0]");
        } else {
            report.add_note("providers.custom", "No apiBase set, skipping custom provider");
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
    // Merge per-channel override
    for (name, ch_cfg) in per_channel {
        if name == "telegram" {
            telegram_py = Some(merge_telegram(telegram_py.as_ref(), &ch_cfg.channels.telegram));
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
        report.add_mapped("channels.telegram");
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
        report.add_mapped("channels.discord");
        if dc.group_policy.is_some() {
            report.add_unmapped(
                "channels.discord.groupPolicy",
                "no nanobot-rs equivalent (group behavior is configured differently)",
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
        report.add_mapped("channels.slack");
        if sl.allow_from.is_some() {
            report.add_unmapped(
                "channels.slack.allowFrom",
                "no nanobot-rs equivalent (user allowlisting not yet supported)",
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
        report.add_mapped("channels.matrix");
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
        report.add_mapped("channels.email");
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
        report.add_mapped("channels.feishu");
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
        report.add_mapped("channels.wecom");
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
        report.add_mapped("channels.weixin");
    }

    // QQ
    if let Some(ref q) = py.qq {
        config.channels.qq = Some(QQConfig {
            app_id: q.app_id.clone(),
            app_secret: q.app_secret.clone(),
            token: None,
            enabled: q.enabled.unwrap_or(true),
        });
        report.add_mapped("channels.qq");
    }
}

/// Merge a per-channel Telegram config into the main config.
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

/// Merge a per-channel Discord config into the main config.
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

/// Merge a per-channel Slack config into the main config.
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
        report.add_mapped("agents.defaults -> agent");

        if defaults.provider.is_some() {
            report.add_note(
                "agents.defaults.provider",
                "nanobot-rs selects providers by model name convention, not explicit provider field",
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
        report.add_mapped("heartbeat");
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
        report.add_mapped("security");
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
        report.add_mapped("dream");
    }
}

// ─── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::python_schema::PythonConfig;

    /// Build a fully-populated Python config for testing.
    fn make_full_python_config() -> PythonConfig {
        let json = r#"{
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
        }"#;
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn test_migrate_providers() {
        let py = make_full_python_config();
        let mut report = MigrationReport::default();
        let mut config = Config::default();
        convert_providers(&py.providers, &mut config, &mut report);

        // OpenRouter
        assert!(config.providers.openrouter.is_some());
        assert_eq!(
            config.providers.openrouter.as_ref().unwrap().api_key,
            Some("sk-or-v1-test".to_string())
        );

        // OpenAI
        assert!(config.providers.openai.is_some());
        assert_eq!(
            config.providers.openai.as_ref().unwrap().api_key,
            Some("sk-openai-test".to_string())
        );
        assert_eq!(
            config.providers.openai.as_ref().unwrap().model,
            Some("gpt-4o".to_string())
        );

        // Anthropic
        assert!(config.providers.anthropic.is_some());
        assert_eq!(
            config.providers.anthropic.as_ref().unwrap().api_key,
            Some("sk-ant-test".to_string())
        );

        // DeepSeek
        assert!(config.providers.deepseek.is_some());

        // Groq
        assert!(config.providers.groq.is_some());

        // Gemini
        assert!(config.providers.gemini.is_some());

        // Ollama — apiBase maps to base_url
        assert!(config.providers.ollama.is_some());
        assert_eq!(
            config.providers.ollama.as_ref().unwrap().base_url,
            Some("http://localhost:11434".to_string())
        );

        // Azure OpenAI
        assert!(config.providers.azure_openai.is_some());
        let azure = config.providers.azure_openai.as_ref().unwrap();
        assert_eq!(azure.api_key, Some("azure-key".to_string()));
        assert_eq!(
            azure.endpoint,
            Some("https://my-resource.openai.azure.com".to_string())
        );
        assert_eq!(azure.deployment, Some("gpt-4".to_string()));
        assert_eq!(azure.api_version, Some("2024-02-15-preview".to_string()));

        // Custom -> custom_providers[0]
        assert_eq!(config.custom_providers.len(), 1);
        assert_eq!(config.custom_providers[0].name, "custom");
        assert_eq!(
            config.custom_providers[0].base_url,
            "https://custom.api/v1"
        );
        assert_eq!(
            config.custom_providers[0].api_key,
            Some("custom-key".to_string())
        );

        // Report
        assert!(report.mapped.contains(&"providers.openrouter".to_string()));
        assert!(report.mapped.contains(&"providers.openai".to_string()));
        assert!(report.mapped.contains(&"providers.custom -> custom_providers[0]".to_string()));
    }

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

        assert!(report.mapped.contains(&"channels.telegram".to_string()));
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
        assert!(dc.enabled);
        assert!(dc.streaming);

        // groupPolicy should be reported as unmapped
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
        assert!(sl.enabled);

        // allowFrom should be reported as unmapped
        assert!(report
            .unmapped
            .iter()
            .any(|u| u.contains("channels.slack.allowFrom")));
    }

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

        // provider field should be noted
        assert!(report
            .notes
            .iter()
            .any(|n| n.contains("agents.defaults.provider")));
        assert!(report.mapped.contains(&"agents.defaults -> agent".to_string()));
    }

    #[test]
    fn test_migrate_heartbeat() {
        let py = make_full_python_config();
        let mut report = MigrationReport::default();
        let mut config = Config::default();
        convert_heartbeat(&py.heartbeat, &mut config, &mut report);

        assert!(config.heartbeat.enabled);
        assert_eq!(config.heartbeat.interval_secs, 900);
        assert!(report.mapped.contains(&"heartbeat".to_string()));
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
        assert!(report.mapped.contains(&"security".to_string()));
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
        assert!(report.mapped.contains(&"dream".to_string()));
    }

    #[test]
    fn test_migrate_custom_provider_no_base() {
        let json = r#"{"providers": {"custom": {"apiKey": "key-only"}}}"#;
        let py: PythonConfig = serde_json::from_str(json).unwrap();
        let mut report = MigrationReport::default();
        let mut config = Config::default();
        convert_providers(&py.providers, &mut config, &mut report);

        // No apiBase → custom provider skipped
        assert!(config.custom_providers.is_empty());
        assert!(report
            .notes
            .iter()
            .any(|n| n.contains("No apiBase set")));
    }

    #[test]
    fn test_migrate_empty_config() {
        let json = "{}";
        let py: PythonConfig = serde_json::from_str(json).unwrap();
        let mut report = MigrationReport::default();
        let config = convert_python_config(&py, &[], &mut report);

        // Should produce defaults
        assert_eq!(config.agent.model, "gpt-4o");
        assert!(config.providers.openai.is_none());
        assert!(config.channels.telegram.is_none());
        assert!(report.mapped.is_empty());
        assert!(report.unmapped.is_empty());
    }

    #[test]
    fn test_migrate_json_roundtrip() {
        // Serialize a Python config to JSON, parse it back, and convert
        let py = make_full_python_config();
        let json = serde_json::to_string(&py).unwrap();
        let py2: PythonConfig = serde_json::from_str(&json).unwrap();

        let mut report = MigrationReport::default();
        let config = convert_python_config(&py2, &[], &mut report);

        assert_eq!(config.agent.model, "anthropic/claude-opus-4-5");
        assert!(config.providers.openai.is_some());
    }

    #[test]
    fn test_migrate_yaml_output_valid() {
        // Migrate then serialize to YAML; verify it parses back as valid Config
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

    #[test]
    fn test_migrate_no_sections() {
        // Config with only a provider — other sections remain defaults
        let json = r#"{"providers": {"openai": {"apiKey": "sk-test"}}}"#;
        let py: PythonConfig = serde_json::from_str(json).unwrap();
        let mut report = MigrationReport::default();
        let config = convert_python_config(&py, &[], &mut report);

        assert!(config.providers.openai.is_some());
        assert!(config.channels.telegram.is_none());
        assert!(config.channels.discord.is_none());
        assert_eq!(config.agent.model, "gpt-4o"); // default
        assert!(!config.heartbeat.enabled); // default
    }

    #[test]
    fn test_migrate_per_channel_merge() {
        // Main config has no telegram; per-channel config does
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
        assert_eq!(
            azure.endpoint,
            Some("https://my-aoai.openai.azure.com".to_string())
        );
        assert_eq!(azure.deployment, Some("gpt-4-turbo".to_string()));
        assert_eq!(azure.api_version, Some("2024-06-01".to_string()));
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

        // Verify a few specifics
        assert_eq!(config.channels.matrix.as_ref().unwrap().homeserver, Some("https://matrix.org".to_string()));
        assert_eq!(config.channels.dingtalk.as_ref().unwrap().webhook, Some("https://dt.webhook".to_string()));
        assert_eq!(config.channels.feishu.as_ref().unwrap().app_id, Some("fs-app".to_string()));
    }
}
