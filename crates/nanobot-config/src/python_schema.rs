//! Python nanobot JSON config schema definitions.
//!
//! These structs mirror the Python nanobot `config.json` format exactly,
//! using `#[serde(rename_all = "camelCase")]` to match camelCase JSON keys.
//! All fields are `Option<T>` because the Python config is flexible —
//! sections may be absent or partially filled.

use serde::{Deserialize, Serialize};

/// Root Python nanobot config.json structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PythonConfig {
    /// Config version from Python nanobot.
    #[serde(rename = "_config_version")]
    pub _config_version: Option<u64>,

    /// LLM provider configurations.
    #[serde(default)]
    pub providers: PythonProviders,

    /// Channel-specific configurations.
    #[serde(default)]
    pub channels: PythonChannels,

    /// Agent settings.
    #[serde(default)]
    pub agents: PythonAgents,

    /// Heartbeat settings.
    #[serde(default)]
    pub heartbeat: PythonHeartbeat,

    /// Security settings.
    #[serde(default)]
    pub security: PythonSecurity,

    /// Dream (memory consolidation) settings.
    #[serde(default)]
    pub dream: PythonDream,
}

/// Python provider configurations.
/// Note: Provider names are snake_case in Python JSON (openrouter, azure_openai, etc.).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PythonProviders {
    /// OpenRouter provider.
    #[serde(default)]
    pub openrouter: Option<PythonProviderEntry>,

    /// OpenAI provider.
    #[serde(default)]
    pub openai: Option<PythonProviderEntry>,

    /// Anthropic (Claude) provider.
    #[serde(default)]
    pub anthropic: Option<PythonProviderEntry>,

    /// Azure OpenAI provider.
    #[serde(default)]
    pub azure_openai: Option<PythonAzureOpenAIEntry>,

    /// DeepSeek provider.
    #[serde(default)]
    pub deepseek: Option<PythonProviderEntry>,

    /// Groq provider.
    #[serde(default)]
    pub groq: Option<PythonProviderEntry>,

    /// Google Gemini provider.
    #[serde(default)]
    pub gemini: Option<PythonProviderEntry>,

    /// Ollama local provider.
    #[serde(default)]
    pub ollama: Option<PythonProviderEntry>,

    /// Custom provider (becomes a `CustomProviderConfig` in RS).
    #[serde(default)]
    pub custom: Option<PythonProviderEntry>,
}

/// A generic Python provider entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PythonProviderEntry {
    /// API key for authentication.
    #[serde(default)]
    pub api_key: Option<String>,

    /// API base URL (Python calls this `apiBase`).
    #[serde(default)]
    pub api_base: Option<String>,

    /// Default model to use.
    #[serde(default)]
    pub model: Option<String>,
}

/// Azure OpenAI provider entry with deployment-specific fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythonAzureOpenAIEntry {
    /// API key for Azure authentication.
    #[serde(default, rename = "apiKey")]
    pub api_key: Option<String>,

    /// Azure OpenAI resource endpoint URL.
    #[serde(default)]
    pub endpoint: Option<String>,

    /// Name of the Azure model deployment.
    #[serde(default)]
    pub deployment: Option<String>,

    /// Azure API version string.
    #[serde(default)]
    pub api_version: Option<String>,
}

/// Python channel configurations.
/// Note: Channel names are snake_case in Python JSON (telegram, discord, etc.).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PythonChannels {
    /// Telegram channel settings.
    #[serde(default)]
    pub telegram: Option<PythonTelegramConfig>,

    /// Discord channel settings.
    #[serde(default)]
    pub discord: Option<PythonDiscordConfig>,

    /// Slack channel settings.
    #[serde(default)]
    pub slack: Option<PythonSlackConfig>,

    /// Matrix channel settings.
    #[serde(default)]
    pub matrix: Option<PythonMatrixConfig>,

    /// Email channel settings.
    #[serde(default)]
    pub email: Option<PythonEmailConfig>,

    /// DingTalk channel settings.
    #[serde(default)]
    pub dingtalk: Option<PythonDingtalkConfig>,

    /// Feishu (Lark) channel settings.
    #[serde(default)]
    pub feishu: Option<PythonFeishuConfig>,

    /// WeCom (Enterprise WeChat) channel settings.
    #[serde(default)]
    pub wecom: Option<PythonWecomConfig>,

    /// WeChat Official Account channel settings.
    #[serde(default)]
    pub weixin: Option<PythonWeixinConfig>,

    /// QQ channel settings.
    #[serde(default)]
    pub qq: Option<PythonQQConfig>,
}

/// Python Telegram channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PythonTelegramConfig {
    /// Whether this channel is enabled.
    #[serde(default)]
    pub enabled: Option<bool>,

    /// Bot token from BotFather.
    #[serde(default)]
    pub token: Option<String>,

    /// User IDs allowed to interact with the bot.
    #[serde(default)]
    pub allow_from: Option<Vec<String>>,
}

/// Python Discord channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PythonDiscordConfig {
    /// Whether this channel is enabled.
    #[serde(default)]
    pub enabled: Option<bool>,

    /// Bot token from the Discord Developer Portal.
    #[serde(default)]
    pub token: Option<String>,

    /// User IDs allowed to interact with the bot.
    #[serde(default)]
    pub allow_from: Option<Vec<String>>,

    /// How the bot responds in groups ("mention", "all", etc.).
    /// No nanobot-rs equivalent — reported as unmapped.
    #[serde(default)]
    pub group_policy: Option<String>,

    /// Whether to stream responses token-by-token.
    #[serde(default)]
    pub streaming: Option<bool>,
}

/// Python Slack channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PythonSlackConfig {
    /// Whether this channel is enabled.
    #[serde(default)]
    pub enabled: Option<bool>,

    /// Bot user OAuth token (`xoxb-...`).
    #[serde(default)]
    pub bot_token: Option<String>,

    /// App-level token (`xapp-...`) for socket mode.
    #[serde(default)]
    pub app_token: Option<String>,

    /// User IDs allowed to interact with the bot.
    /// No nanobot-rs equivalent — reported as unmapped.
    #[serde(default)]
    pub allow_from: Option<Vec<String>>,
}

/// Python Matrix channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PythonMatrixConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub homeserver: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub access_token: Option<String>,
}

/// Python Email channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PythonEmailConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub imap_host: Option<String>,
    #[serde(default)]
    pub smtp_host: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
}

/// Python DingTalk channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PythonDingtalkConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub webhook: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
}

/// Python Feishu channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PythonFeishuConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub app_secret: Option<String>,
}

/// Python WeCom channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PythonWecomConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub corp_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
}

/// Python WeChat channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PythonWeixinConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub app_secret: Option<String>,
}

/// Python QQ channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PythonQQConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub app_secret: Option<String>,
}

/// Python agent settings (contains a `defaults` sub-object).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PythonAgents {
    /// Default agent settings.
    #[serde(default)]
    pub defaults: Option<PythonAgentDefaults>,
}

/// Python agent defaults.
/// Note: Python uses snake_case for these fields (max_tokens, max_iterations, tool_timeout).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythonAgentDefaults {
    /// Default model to use.
    #[serde(default)]
    pub model: Option<String>,

    /// Default provider to use (no RS equivalent — noted).
    #[serde(default)]
    pub provider: Option<String>,

    /// Sampling temperature.
    #[serde(default)]
    pub temperature: Option<f32>,

    /// Maximum tokens for responses.
    #[serde(default)]
    pub max_tokens: Option<u32>,

    /// Maximum iterations for tool loop.
    #[serde(default)]
    pub max_iterations: Option<usize>,

    /// Whether to enable streaming.
    #[serde(default)]
    pub streaming: Option<bool>,

    /// Tool execution timeout in seconds.
    #[serde(default)]
    pub tool_timeout: Option<u64>,
}

/// Python heartbeat configuration.
/// Note: Python uses snake_case for these fields (interval_secs, not intervalSecs).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PythonHeartbeat {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub interval_secs: Option<u64>,
}

/// Python security configuration.
/// Note: Python uses snake_case for these fields (block_private_ips, ssrf_whitelist).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PythonSecurity {
    #[serde(default)]
    pub block_private_ips: Option<bool>,
    #[serde(default)]
    pub ssrf_whitelist: Option<Vec<String>>,
}

/// Python dream (memory consolidation) configuration.
/// Note: Python uses snake_case for these fields (interval_secs).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PythonDream {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub interval_secs: Option<u64>,
    #[serde(default)]
    pub model: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_python_config_minimal() {
        let json = r#"{"_config_version": 4}"#;
        let config: PythonConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config._config_version, Some(4));
        assert!(config.providers.openai.is_none());
        assert!(config.channels.telegram.is_none());
    }

    #[test]
    fn test_parse_python_config_full() {
        let json = r#"{
            "_config_version": 4,
            "providers": {
                "openrouter": {"apiKey": "sk-or-v1-xxx"},
                "openai": {"apiKey": "sk-xxx", "model": "gpt-4o"},
                "anthropic": {"apiKey": "sk-ant-xxx"},
                "custom": {"apiKey": "key", "apiBase": "https://custom.api/v1"}
            },
            "channels": {
                "telegram": {"enabled": true, "token": "123456:ABC", "allowFrom": ["user1"]},
                "discord": {"enabled": true, "token": "discord-token", "groupPolicy": "mention", "streaming": true},
                "slack": {"enabled": true, "botToken": "xoxb-123", "appToken": "xapp-456"}
            },
            "agents": {
                "defaults": {
                    "model": "claude-opus-4-5",
                    "provider": "openrouter",
                    "temperature": 0.7,
                    "max_tokens": 4096,
                    "max_iterations": 50,
                    "streaming": true,
                    "tool_timeout": 120
                }
            },
            "heartbeat": {"enabled": true, "interval_secs": 1800},
            "security": {"block_private_ips": true, "ssrf_whitelist": ["10.0.0.0/8"]},
            "dream": {"enabled": true, "interval_secs": 7200, "model": null}
        }"#;
        let config: PythonConfig = serde_json::from_str(json).unwrap();

        // Providers
        assert!(config.providers.openrouter.is_some());
        assert_eq!(
            config.providers.openrouter.as_ref().unwrap().api_key,
            Some("sk-or-v1-xxx".to_string())
        );
        assert!(config.providers.openai.is_some());
        assert_eq!(
            config.providers.openai.as_ref().unwrap().api_key,
            Some("sk-xxx".to_string())
        );
        assert_eq!(
            config.providers.openai.as_ref().unwrap().model,
            Some("gpt-4o".to_string())
        );
        assert!(config.providers.custom.is_some());
        assert_eq!(
            config.providers.custom.as_ref().unwrap().api_base,
            Some("https://custom.api/v1".to_string())
        );

        // Channels
        assert!(config.channels.telegram.is_some());
        assert_eq!(
            config.channels.telegram.as_ref().unwrap().token,
            Some("123456:ABC".to_string())
        );
        assert_eq!(
            config.channels.telegram.as_ref().unwrap().allow_from,
            Some(vec!["user1".to_string()])
        );
        assert!(config.channels.discord.is_some());
        assert_eq!(
            config.channels.discord.as_ref().unwrap().group_policy,
            Some("mention".to_string())
        );

        // Agents
        assert!(config.agents.defaults.is_some());
        let defaults = config.agents.defaults.as_ref().unwrap();
        assert_eq!(defaults.model, Some("claude-opus-4-5".to_string()));
        assert_eq!(defaults.provider, Some("openrouter".to_string()));
        assert_eq!(defaults.temperature, Some(0.7));

        // Heartbeat
        assert!(config.heartbeat.enabled == Some(true));
        assert!(config.heartbeat.interval_secs == Some(1800));

        // Security
        assert!(config.security.block_private_ips == Some(true));
        assert_eq!(
            config.security.ssrf_whitelist,
            Some(vec!["10.0.0.0/8".to_string()])
        );

        // Dream
        assert!(config.dream.enabled == Some(true));
        assert!(config.dream.model.is_none()); // null in JSON
    }

    #[test]
    fn test_parse_python_config_empty() {
        let json = "{}";
        let config: PythonConfig = serde_json::from_str(json).unwrap();
        assert!(config._config_version.is_none());
        assert!(config.providers.openai.is_none());
    }

    #[test]
    fn test_parse_python_config_unknown_fields_ignored() {
        let json = r#"{
            "unknownSection": {"foo": "bar"},
            "providers": {"openai": {"apiKey": "sk-test", "unknownField": 42}}
        }"#;
        let config: PythonConfig = serde_json::from_str(json).unwrap();
        assert!(config.providers.openai.is_some());
        assert_eq!(
            config.providers.openai.as_ref().unwrap().api_key,
            Some("sk-test".to_string())
        );
    }
}
