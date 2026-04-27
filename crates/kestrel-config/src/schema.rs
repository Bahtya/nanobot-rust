//! Configuration schema matching the Python kestrel config.yaml format.
//!
//! Uses serde for deserialization with `#[serde(rename_all = "snake_case")]`
//! to maintain compatibility with the Python camelCase config keys.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Root configuration structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Config {
    /// Configuration format version for migration tracking.
    #[serde(default)]
    pub _config_version: Option<u64>,

    /// LLM provider configurations.
    #[serde(default)]
    pub providers: ProvidersConfig,

    /// Channel-specific configurations.
    #[serde(default)]
    pub channels: ChannelsConfig,

    /// Agent default settings.
    #[serde(default)]
    pub agent: AgentDefaults,

    /// Memory consolidation settings (dream).
    #[serde(default)]
    pub dream: DreamConfig,

    /// Heartbeat settings.
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,

    /// Cron job settings.
    #[serde(default)]
    pub cron: CronConfig,

    /// Security settings.
    #[serde(default)]
    pub security: SecurityConfig,

    /// Custom system prompt additions.
    #[serde(default)]
    pub custom_instructions: Option<String>,

    /// Agent identity name.
    #[serde(default)]
    pub name: Option<String>,

    /// Workspace path.
    #[serde(default)]
    pub workspace: Option<String>,

    /// Custom providers list.
    #[serde(default)]
    pub custom_providers: Vec<CustomProviderConfig>,

    /// MCP server configurations.
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,

    /// API server configuration.
    #[serde(default)]
    pub api: ApiConfig,

    /// Daemon mode configuration.
    #[serde(default)]
    pub daemon: DaemonConfig,

    /// Startup notification settings.
    #[serde(default)]
    pub notifications: NotificationsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            _config_version: Some(4),
            providers: ProvidersConfig::default(),
            channels: ChannelsConfig::default(),
            agent: AgentDefaults::default(),
            dream: DreamConfig::default(),
            heartbeat: HeartbeatConfig::default(),
            cron: CronConfig::default(),
            security: SecurityConfig::default(),
            custom_instructions: None,
            name: None,
            workspace: None,
            custom_providers: Vec::new(),
            mcp_servers: HashMap::new(),
            api: ApiConfig::default(),
            daemon: DaemonConfig::default(),
            notifications: NotificationsConfig::default(),
        }
    }
}

/// Startup notification configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NotificationsConfig {
    /// Whether to send an online notification when a channel connects.
    #[serde(default = "default_true")]
    pub online_notify: bool,
    /// Chat ID that receives the online notification.
    #[serde(default)]
    pub notify_chat_id: Option<String>,
    /// Plain-text message template used for online notifications.
    ///
    /// Supported placeholders:
    /// - `{version}` → the running Kestrel version
    /// - `{channel}` → the connected channel name
    #[serde(default = "default_online_message")]
    pub online_message: String,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            online_notify: true,
            notify_chat_id: None,
            online_message: default_online_message(),
        }
    }
}

/// Provider configurations for various LLM backends.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct ProvidersConfig {
    /// Anthropic (Claude) provider settings.
    #[serde(default)]
    pub anthropic: Option<ProviderEntry>,
    /// OpenAI provider settings.
    #[serde(default)]
    pub openai: Option<ProviderEntry>,
    /// OpenRouter provider settings.
    #[serde(default)]
    pub openrouter: Option<ProviderEntry>,
    /// Ollama local provider settings.
    #[serde(default)]
    pub ollama: Option<ProviderEntry>,
    /// DeepSeek provider settings.
    #[serde(default)]
    pub deepseek: Option<ProviderEntry>,
    /// Google Gemini provider settings.
    #[serde(default)]
    pub gemini: Option<ProviderEntry>,
    /// Groq provider settings.
    #[serde(default)]
    pub groq: Option<ProviderEntry>,
    /// Moonshot (Kimi) provider settings.
    #[serde(default)]
    pub moonshot: Option<ProviderEntry>,
    /// MiniMax provider settings.
    #[serde(default)]
    pub minimax: Option<ProviderEntry>,
    /// Azure OpenAI provider settings.
    #[serde(default)]
    pub azure_openai: Option<AzureOpenAIProviderEntry>,
    /// GitHub Copilot provider settings.
    #[serde(default)]
    pub github_copilot: Option<ProviderEntry>,
    /// OpenAI Codex provider settings.
    #[serde(default)]
    pub openai_codex: Option<ProviderEntry>,
}

/// A generic provider entry with API key and optional base URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ProviderEntry {
    /// API key for authentication.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Custom base URL override.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Default model to use with this provider.
    #[serde(default)]
    pub model: Option<String>,
    /// Skip proxy for this provider's API endpoint.
    /// Set to true for domestic Chinese APIs (e.g. ZAI, Qwen) that don't need a proxy.
    #[serde(default)]
    pub no_proxy: Option<bool>,
}

/// Azure OpenAI provider entry with deployment-specific fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AzureOpenAIProviderEntry {
    /// API key for Azure authentication.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Azure OpenAI resource endpoint URL.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Name of the Azure model deployment.
    #[serde(default)]
    pub deployment: Option<String>,
    /// Azure API version string (e.g. "2024-02-15-preview").
    #[serde(default)]
    pub api_version: Option<String>,
}

/// Channel configurations.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct ChannelsConfig {
    /// Telegram channel settings.
    #[serde(default)]
    pub telegram: Option<TelegramConfig>,
    /// Discord channel settings.
    #[serde(default)]
    pub discord: Option<DiscordConfig>,
    /// Slack channel settings.
    #[serde(default)]
    pub slack: Option<SlackConfig>,
    /// Matrix channel settings.
    #[serde(default)]
    pub matrix: Option<MatrixConfig>,
    /// WhatsApp channel settings.
    #[serde(default)]
    pub whatsapp: Option<WhatsappConfig>,
    /// Email channel settings.
    #[serde(default)]
    pub email: Option<EmailConfig>,
    /// DingTalk channel settings.
    #[serde(default)]
    pub dingtalk: Option<DingtalkConfig>,
    /// Feishu (Lark) channel settings.
    #[serde(default)]
    pub feishu: Option<FeishuConfig>,
    /// WeCom (Enterprise WeChat) channel settings.
    #[serde(default)]
    pub wecom: Option<WecomConfig>,
    /// WeChat Official Account channel settings.
    #[serde(default)]
    pub weixin: Option<WeixinConfig>,
    /// QQ channel settings.
    #[serde(default)]
    pub qq: Option<QQConfig>,
    /// Mochat channel settings.
    #[serde(default)]
    pub mochat: Option<MochatConfig>,
    /// WebSocket channel settings.
    #[serde(default)]
    pub websocket: Option<WebSocketConfig>,
}

/// Telegram channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TelegramConfig {
    /// Bot token from BotFather.
    pub token: String,
    /// User IDs allowed to interact with the bot.
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// User IDs with admin privileges.
    #[serde(default)]
    pub admin_users: Vec<String>,
    /// Whether this channel is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Whether to stream responses token-by-token.
    #[serde(default)]
    pub streaming: bool,
    /// HTTP/SOCKS5 proxy URL for Telegram API requests.
    ///
    /// - `"http://host:port"` or `"https://host:port"` → HTTP proxy
    /// - `"socks5://host:port"` or `"socks5h://host:port"` → SOCKS5 proxy
    /// - empty or absent → direct connection (no proxy)
    ///
    /// Config takes precedence over `HTTPS_PROXY`/`ALL_PROXY` env vars.
    #[serde(default)]
    pub proxy: Option<String>,
}

/// Discord channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DiscordConfig {
    /// Bot token from the Discord Developer Portal.
    pub token: String,
    /// Guild (server) IDs the bot is allowed to operate in.
    #[serde(default)]
    pub allowed_guilds: Vec<String>,
    /// Whether this channel is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Whether to stream responses token-by-token.
    #[serde(default)]
    pub streaming: bool,
    /// HTTP/SOCKS5 proxy URL for Discord API requests.
    ///
    /// - `"http://host:port"` or `"https://host:port"` → HTTP proxy
    /// - `"socks5://host:port"` or `"socks5h://host:port"` → SOCKS5 proxy
    /// - empty or absent → direct connection (no proxy)
    ///
    /// Config takes precedence over `HTTPS_PROXY`/`ALL_PROXY` env vars.
    #[serde(default)]
    pub proxy: Option<String>,
}

/// Slack channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SlackConfig {
    /// Bot user OAuth token (`xoxb-...`).
    pub bot_token: Option<String>,
    /// App-level token (`xapp-...`) for socket mode.
    pub app_token: Option<String>,
    /// Signing secret for request verification.
    pub signing_secret: Option<String>,
    /// Whether this channel is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Matrix channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MatrixConfig {
    /// Homeserver URL (e.g. `https://matrix.org`).
    pub homeserver: Option<String>,
    /// Fully-qualified Matrix user ID.
    pub user_id: Option<String>,
    /// Password for login (used when access_token is not set).
    pub password: Option<String>,
    /// Pre-existing access token (avoids password login).
    pub access_token: Option<String>,
    /// Whether this channel is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// WhatsApp channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WhatsappConfig {
    /// Whether this channel is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Email channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct EmailConfig {
    /// IMAP server hostname for receiving messages.
    pub imap_host: Option<String>,
    /// SMTP server hostname for sending messages.
    pub smtp_host: Option<String>,
    /// Login username for both IMAP and SMTP.
    pub username: Option<String>,
    /// Login password or app-specific password.
    pub password: Option<String>,
    /// SMTP server port.
    #[serde(default)]
    pub port: u16,
    /// Whether this channel is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// DingTalk channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DingtalkConfig {
    /// Webhook URL for the DingTalk robot.
    pub webhook: Option<String>,
    /// Secret for signing webhook requests.
    pub secret: Option<String>,
    /// Whether this channel is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Feishu (Lark) channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FeishuConfig {
    /// Feishu app ID.
    pub app_id: Option<String>,
    /// Feishu app secret.
    pub app_secret: Option<String>,
    /// Whether this channel is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// WeCom (Enterprise WeChat) channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WecomConfig {
    /// Enterprise corporation ID.
    pub corp_id: Option<String>,
    /// Application agent ID.
    pub agent_id: Option<String>,
    /// Application secret.
    pub secret: Option<String>,
    /// Callback verification token.
    pub token: Option<String>,
    /// Callback AES encoding key.
    pub encoding_aes_key: Option<String>,
    /// Whether this channel is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// WeChat Official Account channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WeixinConfig {
    /// WeChat app ID.
    pub app_id: Option<String>,
    /// WeChat app secret.
    pub app_secret: Option<String>,
    /// Callback verification token.
    pub token: Option<String>,
    /// Callback AES encoding key.
    pub encoding_aes_key: Option<String>,
    /// Whether this channel is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// QQ channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct QQConfig {
    /// QQ app ID.
    pub app_id: Option<String>,
    /// QQ app secret.
    pub app_secret: Option<String>,
    /// Callback verification token.
    pub token: Option<String>,
    /// Whether this channel is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Mochat channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MochatConfig {
    /// Webhook URL for the Mochat integration.
    pub webhook_url: Option<String>,
    /// Whether this channel is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// WebSocket channel configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WebSocketConfig {
    /// Whether WebSocket is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Listen address (e.g., "127.0.0.1:8090").
    #[serde(default = "default_ws_addr")]
    pub listen_addr: String,
    /// Authentication settings.
    #[serde(default)]
    pub auth: WsAuthConfig,
    /// Maximum concurrent clients.
    #[serde(default = "default_max_clients")]
    pub max_clients: u32,
    /// Maximum message size in bytes.
    #[serde(default = "default_max_msg_size")]
    pub max_message_size: u64,
}

impl Default for WebSocketConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen_addr: default_ws_addr(),
            auth: WsAuthConfig::default(),
            max_clients: default_max_clients(),
            max_message_size: default_max_msg_size(),
        }
    }
}

/// WebSocket authentication configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct WsAuthConfig {
    /// Whether authentication is required.
    #[serde(default)]
    pub required: bool,
    /// Shared secret token for authentication.
    #[serde(default)]
    pub token: Option<String>,
}

fn default_ws_addr() -> String {
    "127.0.0.1:8090".to_string()
}

fn default_max_clients() -> u32 {
    100
}

fn default_max_msg_size() -> u64 {
    1048576
}

/// Streaming display configuration for progressive message editing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct StreamDisplayConfig {
    /// Minimum seconds between Telegram message edits.
    #[serde(default = "default_edit_interval")]
    pub edit_interval_secs: f64,

    /// Minimum characters to buffer before triggering an edit.
    #[serde(default = "default_buffer_threshold")]
    pub buffer_threshold: usize,

    /// Cursor appended to the message during active streaming.
    #[serde(default = "default_stream_cursor")]
    pub cursor: String,
}

fn default_edit_interval() -> f64 {
    1.5
}

fn default_buffer_threshold() -> usize {
    40
}

fn default_stream_cursor() -> String {
    " ▌".to_string()
}

impl Default for StreamDisplayConfig {
    fn default() -> Self {
        Self {
            edit_interval_secs: default_edit_interval(),
            buffer_threshold: default_buffer_threshold(),
            cursor: default_stream_cursor(),
        }
    }
}

/// Agent default settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentDefaults {
    /// Default model to use.
    #[serde(default = "default_model")]
    pub model: String,

    /// Default temperature.
    #[serde(default = "default_temperature")]
    pub temperature: f32,

    /// Maximum tokens for responses.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,

    /// Maximum iterations for tool loop.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,

    /// Default workspace directory.
    #[serde(default)]
    pub workspace: Option<String>,

    /// System prompt template.
    #[serde(default)]
    pub system_prompt: Option<String>,

    /// Whether to enable streaming.
    #[serde(default = "default_true")]
    pub streaming: bool,

    /// Streaming display configuration (rate-limiting, cursor, etc.).
    #[serde(default)]
    pub stream_display: StreamDisplayConfig,

    /// Tool execution timeout in seconds.
    #[serde(default = "default_tool_timeout")]
    pub tool_timeout: u64,

    /// HTTP connect timeout in seconds for provider API calls.
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: u64,

    /// Timeout in seconds to wait for the first byte from a provider response.
    #[serde(default = "default_first_byte_timeout")]
    pub first_byte_timeout: u64,

    /// Per-chunk idle timeout in seconds during SSE streaming.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: u64,

    /// Message processing timeout in seconds.
    ///
    /// When a single message exceeds this duration, the agent loop sends a
    /// timeout reply to the user instead of silently dropping the message.
    #[serde(default = "default_message_timeout")]
    pub message_timeout: u64,
}

impl Default for AgentDefaults {
    fn default() -> Self {
        Self {
            model: default_model(),
            temperature: default_temperature(),
            max_tokens: default_max_tokens(),
            max_iterations: default_max_iterations(),
            workspace: None,
            system_prompt: None,
            streaming: true,
            stream_display: StreamDisplayConfig::default(),
            tool_timeout: default_tool_timeout(),
            connect_timeout: default_connect_timeout(),
            first_byte_timeout: default_first_byte_timeout(),
            idle_timeout: default_idle_timeout(),
            message_timeout: default_message_timeout(),
        }
    }
}

/// Dream (memory consolidation) configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DreamConfig {
    /// Whether dream is enabled.
    #[serde(default)]
    pub enabled: bool,

    /// Interval in seconds between dream cycles.
    #[serde(default = "default_dream_interval")]
    pub interval_secs: u64,

    /// Model to use for dream consolidation.
    #[serde(default)]
    pub model: Option<String>,
}

impl Default for DreamConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: default_dream_interval(),
            model: None,
        }
    }
}

/// Heartbeat configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HeartbeatConfig {
    /// Whether heartbeat is enabled.
    #[serde(default)]
    pub enabled: bool,

    /// Interval in seconds between heartbeat checks.
    #[serde(default = "default_heartbeat_interval")]
    pub interval_secs: u64,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: default_heartbeat_interval(),
        }
    }
}

/// Cron job configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CronConfig {
    /// Whether the cron scheduler is enabled.
    #[serde(default)]
    pub enabled: bool,

    /// Path to the cron state file.
    #[serde(default)]
    pub state_file: Option<String>,

    /// Tick interval in seconds for checking due jobs.
    #[serde(default = "default_cron_tick_secs")]
    pub tick_secs: u64,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            state_file: None,
            tick_secs: default_cron_tick_secs(),
        }
    }
}

/// Security configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct SecurityConfig {
    /// IP ranges allowed for outbound requests (SSRF whitelist).
    #[serde(default)]
    pub ssrf_whitelist: Vec<String>,

    /// Whether to block private IPs.
    #[serde(default = "default_true")]
    pub block_private_ips: bool,

    /// Additional blocked networks.
    #[serde(default)]
    pub blocked_networks: Vec<String>,
}

/// API server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ApiConfig {
    /// Bind host. Default: `"0.0.0.0"`.
    #[serde(default = "default_api_host")]
    pub host: String,

    /// Bind port. Default: `8080`.
    #[serde(default = "default_api_port")]
    pub port: u16,

    /// CORS allowed origins. Default: `["*"]` (allow all).
    #[serde(default = "default_api_allowed_origins")]
    pub allowed_origins: Vec<String>,

    /// Maximum request body size in bytes. Default: 10 MB.
    #[serde(default = "default_api_max_body_size")]
    pub max_body_size: usize,
}

fn default_api_host() -> String {
    "0.0.0.0".to_string()
}

const fn default_api_port() -> u16 {
    8080
}

/// Default CORS allowed origins: allow all.
fn default_api_allowed_origins() -> Vec<String> {
    vec!["*".to_string()]
}

/// Default max body size: 10 MB.
const fn default_api_max_body_size() -> usize {
    10 * 1024 * 1024
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            host: default_api_host(),
            port: default_api_port(),
            allowed_origins: default_api_allowed_origins(),
            max_body_size: default_api_max_body_size(),
        }
    }
}

/// Daemon mode configuration.
///
/// Controls native Unix daemon behavior: background process with PID file,
/// signal handling, and file-based logging. Activated via `kestrel daemon start`
/// on the CLI — there is no config-file toggle.
///
/// ```yaml
/// daemon:
///   pid_file: ~/.kestrel/kestrel.pid
///   log_dir: ~/.kestrel/logs
///   working_directory: /
///   grace_period_secs: 30
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DaemonConfig {
    /// Path to the PID file.
    #[serde(default = "default_daemon_pid_file")]
    pub pid_file: String,

    /// Directory for log files.
    #[serde(default = "default_daemon_log_dir")]
    pub log_dir: String,

    /// Working directory after daemonizing.
    #[serde(default = "default_daemon_working_directory")]
    pub working_directory: String,

    /// Grace period in seconds for in-flight work during shutdown.
    #[serde(default = "default_daemon_grace_period")]
    pub grace_period_secs: u64,

    /// Log level for daemon file logging (trace, debug, info, warn, error).
    #[serde(default = "default_daemon_log_level")]
    pub log_level: String,

    /// Number of days to retain log files before auto-cleanup.
    #[serde(default = "default_daemon_log_retain_days")]
    pub log_retain_days: u64,

    /// Log output format: `"text"` (human-readable) or `"json"` (structured).
    #[serde(default = "default_daemon_log_format")]
    pub log_format: String,
}

fn default_daemon_pid_file() -> String {
    let fallback = if crate::platform::is_termux() {
        std::path::PathBuf::from(crate::platform::TERMUX_HOME_FALLBACK)
    } else {
        std::path::PathBuf::from("/tmp")
    };
    let home = dirs::home_dir().unwrap_or(fallback);
    home.join(".kestrel")
        .join("kestrel.pid")
        .to_string_lossy()
        .to_string()
}

fn default_daemon_log_dir() -> String {
    let fallback = if crate::platform::is_termux() {
        std::path::PathBuf::from(crate::platform::TERMUX_HOME_FALLBACK)
    } else {
        std::path::PathBuf::from("/tmp")
    };
    let home = dirs::home_dir().unwrap_or(fallback);
    home.join(".kestrel")
        .join("logs")
        .to_string_lossy()
        .to_string()
}

fn default_daemon_working_directory() -> String {
    if crate::platform::is_termux() {
        dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from(crate::platform::TERMUX_HOME_FALLBACK))
            .to_string_lossy()
            .to_string()
    } else {
        "/".to_string()
    }
}

const fn default_daemon_grace_period() -> u64 {
    30
}

fn default_daemon_log_level() -> String {
    "info".to_string()
}

const fn default_daemon_log_retain_days() -> u64 {
    30
}

fn default_daemon_log_format() -> String {
    "text".to_string()
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            pid_file: default_daemon_pid_file(),
            log_dir: default_daemon_log_dir(),
            working_directory: default_daemon_working_directory(),
            grace_period_secs: default_daemon_grace_period(),
            log_level: default_daemon_log_level(),
            log_retain_days: default_daemon_log_retain_days(),
            log_format: default_daemon_log_format(),
        }
    }
}

/// Custom provider configuration for non-standard endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CustomProviderConfig {
    /// Provider name.
    pub name: String,

    /// API base URL.
    pub base_url: String,

    /// API key.
    #[serde(default)]
    pub api_key: Option<String>,

    /// Model keyword patterns this provider handles.
    #[serde(default)]
    pub model_patterns: Vec<String>,

    /// Skip proxy for this provider (for domestic APIs).
    #[serde(default)]
    pub no_proxy: Option<bool>,
}

/// MCP server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct McpServerConfig {
    /// Transport type: "stdio", "sse", or "http".
    #[serde(default = "default_mcp_transport")]
    pub transport: String,

    /// Command to start the server (for stdio transport).
    #[serde(default)]
    pub command: Option<String>,

    /// Arguments for the command.
    #[serde(default)]
    pub args: Option<Vec<String>>,

    /// URL for SSE/HTTP transport.
    #[serde(default)]
    pub url: Option<String>,

    /// Environment variables to pass.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn default_true() -> bool {
    true
}

fn default_model() -> String {
    "gpt-4o".to_string()
}

fn default_temperature() -> f32 {
    0.7
}

fn default_max_tokens() -> u32 {
    4096
}

fn default_max_iterations() -> usize {
    50
}

fn default_tool_timeout() -> u64 {
    120
}

const fn default_message_timeout() -> u64 {
    90
}

const fn default_connect_timeout() -> u64 {
    10
}

const fn default_first_byte_timeout() -> u64 {
    15
}

const fn default_idle_timeout() -> u64 {
    30
}

fn default_dream_interval() -> u64 {
    7200
}

fn default_heartbeat_interval() -> u64 {
    1800
}

fn default_cron_tick_secs() -> u64 {
    60
}

fn default_online_message() -> String {
    "🟢 Kestrel v{version} online — {channel} connected".to_string()
}

fn default_mcp_transport() -> String {
    "stdio".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config._config_version, Some(4));
        assert!(config.custom_instructions.is_none());
        assert!(config.name.is_none());
        assert!(config.workspace.is_none());
        assert!(config.custom_providers.is_empty());
        assert!(config.mcp_servers.is_empty());
        assert!(config.notifications.online_notify);
        assert!(config.notifications.notify_chat_id.is_none());
        assert_eq!(
            config.notifications.online_message,
            "🟢 Kestrel v{version} online — {channel} connected"
        );
    }

    #[test]
    fn test_agent_defaults_values() {
        let agent = AgentDefaults::default();
        assert_eq!(agent.model, "gpt-4o");
        assert!((agent.temperature - 0.7).abs() < f32::EPSILON);
        assert_eq!(agent.max_tokens, 4096);
        assert_eq!(agent.max_iterations, 50);
        assert!(agent.workspace.is_none());
        assert!(agent.system_prompt.is_none());
        assert!(agent.streaming);
        assert_eq!(agent.tool_timeout, 120);
    }

    #[test]
    fn test_config_yaml_roundtrip() {
        let config = Config::default();
        let yaml = serde_yaml::to_string(&config).expect("serialize to yaml");
        let parsed: Config = serde_yaml::from_str(&yaml).expect("deserialize from yaml");
        assert_eq!(parsed._config_version, config._config_version);
        assert_eq!(parsed.agent.model, config.agent.model);
        assert_eq!(parsed.agent.temperature, config.agent.temperature);
        assert_eq!(parsed.agent.max_tokens, config.agent.max_tokens);
    }

    #[test]
    fn test_provider_entry_optional_fields() {
        let entry = ProviderEntry {
            api_key: None,
            base_url: None,
            model: None,
            no_proxy: None,
        };
        assert!(entry.api_key.is_none());
        assert!(entry.base_url.is_none());
        assert!(entry.model.is_none());
    }

    #[test]
    fn test_security_config_default() {
        let security = SecurityConfig::default();
        // SecurityConfig derives Default, so block_private_ips is false via Rust default.
        // The serde default = "default_true" only applies on deserialization.
        assert!(!security.block_private_ips);
        assert!(security.ssrf_whitelist.is_empty());
        assert!(security.blocked_networks.is_empty());

        // Verify that serde deserialization of empty yaml uses default_true.
        let from_yaml: SecurityConfig = serde_yaml::from_str("{}").unwrap();
        assert!(from_yaml.block_private_ips);
    }

    #[test]
    fn test_mcp_server_config_default_transport() {
        let mcp = McpServerConfig {
            transport: default_mcp_transport(),
            command: None,
            args: None,
            url: None,
            env: HashMap::new(),
        };
        assert_eq!(mcp.transport, "stdio");
    }

    #[test]
    fn test_config_parse_minimal_yaml() {
        let yaml = r#"
agent:
  model: gpt-4o-mini
  temperature: 0.5
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.agent.model, "gpt-4o-mini");
        assert!((config.agent.temperature - 0.5).abs() < f32::EPSILON);
        assert!(config.providers.anthropic.is_none());
        assert!(config.providers.openai.is_none());
    }

    #[test]
    fn test_config_parse_full_yaml() {
        let yaml = r#"
_config_version: 4
providers:
  openai:
    api_key: sk-test-123
    base_url: https://api.openai.com/v1
    model: gpt-4o
  anthropic:
    api_key: sk-ant-123
channels:
  telegram:
    token: "123456:ABC"
    allowed_users:
      - user1
    streaming: true
agent:
  model: gpt-4o
  temperature: 0.8
  max_tokens: 2048
  max_iterations: 30
  streaming: false
  tool_timeout: 60
dream:
  enabled: true
  interval_secs: 3600
heartbeat:
  enabled: true
  interval_secs: 600
security:
  block_private_ips: true
  ssrf_whitelist:
    - 10.0.0.0/8
custom_instructions: "Be helpful"
name: "TestBot"
workspace: "/tmp/workspace"
mcp_servers:
  filesystem:
    transport: stdio
    command: "mcp-filesystem"
    args:
      - "--root"
      - "/data"
notifications:
  online_notify: false
  notify_chat_id: "-1001234567890"
  online_message: "Kestrel {version} online on {channel}"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config._config_version, Some(4));
        assert_eq!(config.agent.model, "gpt-4o");
        assert!((config.agent.temperature - 0.8).abs() < f32::EPSILON);
        assert_eq!(config.agent.max_tokens, 2048);
        assert_eq!(config.agent.max_iterations, 30);
        assert!(!config.agent.streaming);
        assert_eq!(config.agent.tool_timeout, 60);

        assert!(config.providers.openai.is_some());
        assert_eq!(
            config.providers.openai.as_ref().unwrap().api_key,
            Some("sk-test-123".to_string())
        );
        assert!(config.providers.anthropic.is_some());

        assert!(config.channels.telegram.is_some());
        let tg = config.channels.telegram.as_ref().unwrap();
        assert_eq!(tg.token, "123456:ABC");
        assert_eq!(tg.allowed_users, vec!["user1"]);
        assert!(tg.streaming);
        assert!(tg.enabled);
        assert!(tg.proxy.is_none());

        assert!(config.dream.enabled);
        assert_eq!(config.dream.interval_secs, 3600);

        assert!(config.heartbeat.enabled);
        assert_eq!(config.heartbeat.interval_secs, 600);

        assert!(config.security.block_private_ips);
        assert_eq!(config.security.ssrf_whitelist, vec!["10.0.0.0/8"]);

        assert_eq!(config.custom_instructions, Some("Be helpful".to_string()));
        assert_eq!(config.name, Some("TestBot".to_string()));
        assert_eq!(config.workspace, Some("/tmp/workspace".to_string()));

        assert_eq!(config.mcp_servers.len(), 1);
        let mcp = &config.mcp_servers["filesystem"];
        assert_eq!(mcp.transport, "stdio");
        assert_eq!(mcp.command, Some("mcp-filesystem".to_string()));
        assert_eq!(
            mcp.args,
            Some(vec!["--root".to_string(), "/data".to_string()])
        );
        assert!(!config.notifications.online_notify);
        assert_eq!(
            config.notifications.notify_chat_id.as_deref(),
            Some("-1001234567890")
        );
        assert_eq!(
            config.notifications.online_message,
            "Kestrel {version} online on {channel}"
        );
    }

    #[test]
    fn test_notifications_config_parse_defaults_when_missing() {
        let config: Config = serde_yaml::from_str("{}").unwrap();
        assert!(config.notifications.online_notify);
        assert!(config.notifications.notify_chat_id.is_none());
        assert_eq!(
            config.notifications.online_message,
            "🟢 Kestrel v{version} online — {channel} connected"
        );
    }

    #[test]
    fn test_custom_provider_config_parse() {
        let yaml = r#"
custom_providers:
  - name: my_provider
    base_url: https://my-api.com/v1
    api_key: key123
    model_patterns:
      - my-model
    no_proxy: true
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.custom_providers.len(), 1);
        let cp = &config.custom_providers[0];
        assert_eq!(cp.name, "my_provider");
        assert_eq!(cp.base_url, "https://my-api.com/v1");
        assert_eq!(cp.api_key, Some("key123".to_string()));
        assert_eq!(cp.model_patterns, vec!["my-model"]);
        assert_eq!(cp.no_proxy, Some(true));
    }

    #[test]
    fn test_dream_config_default() {
        let dc = DreamConfig::default();
        assert!(dc.enabled);
        assert_eq!(dc.interval_secs, 7200);
        assert!(dc.model.is_none());
    }

    #[test]
    fn test_heartbeat_config_default() {
        let hc = HeartbeatConfig::default();
        assert!(!hc.enabled);
        assert_eq!(hc.interval_secs, 1800);
    }

    #[test]
    fn test_cron_config_default() {
        let cc = CronConfig::default();
        assert!(!cc.enabled);
        assert!(cc.state_file.is_none());
        assert_eq!(cc.tick_secs, 60);
    }

    #[test]
    fn test_cron_config_parse() {
        let yaml = r#"
cron:
  enabled: true
  state_file: /tmp/cron_state.json
  tick_secs: 30
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.cron.enabled);
        assert_eq!(
            config.cron.state_file,
            Some("/tmp/cron_state.json".to_string())
        );
        assert_eq!(config.cron.tick_secs, 30);
    }

    #[test]
    fn test_empty_config_yaml() {
        let yaml = "{}";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.agent.model, "gpt-4o");
        assert!(config.agent.streaming);
    }

    #[test]
    fn test_daemon_config_default() {
        let dc = DaemonConfig::default();
        assert!(dc.pid_file.contains("kestrel.pid"));
        assert!(dc.log_dir.contains("logs"));
        assert_eq!(dc.working_directory, "/");
        assert_eq!(dc.grace_period_secs, 30);
        assert_eq!(dc.log_retain_days, 30);
        assert_eq!(dc.log_format, "text");
    }

    #[test]
    fn test_daemon_config_parse() {
        let yaml = r#"
daemon:
  pid_file: /var/run/kestrel.pid
  log_dir: /var/log/kestrel
  working_directory: /opt
  grace_period_secs: 60
  log_retain_days: 7
  log_format: json
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.daemon.pid_file, "/var/run/kestrel.pid");
        assert_eq!(config.daemon.log_dir, "/var/log/kestrel");
        assert_eq!(config.daemon.working_directory, "/opt");
        assert_eq!(config.daemon.grace_period_secs, 60);
        assert_eq!(config.daemon.log_retain_days, 7);
        assert_eq!(config.daemon.log_format, "json");
    }

    #[test]
    fn test_daemon_config_yaml_roundtrip() {
        let dc = DaemonConfig::default();
        let yaml = serde_yaml::to_string(&dc).unwrap();
        let parsed: DaemonConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.pid_file, dc.pid_file);
        assert_eq!(parsed.log_dir, dc.log_dir);
        assert_eq!(parsed.working_directory, dc.working_directory);
        assert_eq!(parsed.grace_period_secs, dc.grace_period_secs);
        assert_eq!(parsed.log_retain_days, dc.log_retain_days);
        assert_eq!(parsed.log_format, dc.log_format);
    }

    #[test]
    fn test_websocket_config_default() {
        let wc = WebSocketConfig::default();
        assert!(!wc.enabled);
        assert_eq!(wc.listen_addr, "127.0.0.1:8090");
        assert!(!wc.auth.required);
        assert!(wc.auth.token.is_none());
        assert_eq!(wc.max_clients, 100);
        assert_eq!(wc.max_message_size, 1048576);
    }

    #[test]
    fn test_websocket_config_parse() {
        let yaml = r#"
channels:
  websocket:
    enabled: true
    listen_addr: "0.0.0.0:9090"
    auth:
      required: true
      token: "my-secret"
    max_clients: 50
    max_message_size: 2097152
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let ws = config.channels.websocket.unwrap();
        assert!(ws.enabled);
        assert_eq!(ws.listen_addr, "0.0.0.0:9090");
        assert!(ws.auth.required);
        assert_eq!(ws.auth.token, Some("my-secret".to_string()));
        assert_eq!(ws.max_clients, 50);
        assert_eq!(ws.max_message_size, 2097152);
    }

    #[test]
    fn test_websocket_config_yaml_roundtrip() {
        let wc = WebSocketConfig::default();
        let yaml = serde_yaml::to_string(&wc).unwrap();
        let parsed: WebSocketConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.enabled, wc.enabled);
        assert_eq!(parsed.listen_addr, wc.listen_addr);
        assert_eq!(parsed.max_clients, wc.max_clients);
        assert_eq!(parsed.max_message_size, wc.max_message_size);
    }
}
