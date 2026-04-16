//! Integration tests for config loading with env var expansion
//! and gateway wiring.

use kestrel_bus::MessageBus;
use kestrel_channels::base::BaseChannel;
use kestrel_channels::platforms::telegram::TelegramChannel;
use kestrel_channels::registry::ChannelRegistry;
use kestrel_channels::ChannelManager;
use kestrel_config::loader::{expand_env_vars, load_config};
use kestrel_core::Platform;

// ---------------------------------------------------------------------------
// Config env var expansion
// ---------------------------------------------------------------------------

#[test]
fn test_expand_env_vars_telegram_token() {
    std::env::set_var("TEST_TG_TOKEN", "123456:ABC-DEF");
    let input = "token: ${TEST_TG_TOKEN}";
    let expanded = expand_env_vars(input);
    assert_eq!(expanded, "token: 123456:ABC-DEF");
    std::env::remove_var("TEST_TG_TOKEN");
}

#[test]
fn test_expand_env_vars_with_default_in_yaml() {
    let yaml = r#"
channels:
  telegram:
    token: ${NONEXISTENT_TG_TOKEN:-default_token}
    enabled: true
"#;
    let expanded = expand_env_vars(yaml);
    assert!(expanded.contains("default_token"));
    assert!(!expanded.contains("${"));
}

#[test]
fn test_expand_env_vars_multiple_in_one_line() {
    std::env::set_var("TEST_HOST", "api.example.com");
    std::env::set_var("TEST_PORT", "8080");
    let input = "url: https://${TEST_HOST}:${TEST_PORT}/v1";
    let expanded = expand_env_vars(input);
    assert_eq!(expanded, "url: https://api.example.com:8080/v1");
    std::env::remove_var("TEST_HOST");
    std::env::remove_var("TEST_PORT");
}

#[test]
fn test_load_config_from_file_with_env_vars() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.yaml");

    std::env::set_var("TEST_KESTREL_TG_TOKEN", "999888:XYZ");
    std::env::set_var("TEST_KESTREL_OPENAI_KEY", "sk-test-key-12345");

    let yaml_content = r#"
_config_version: 4
providers:
  openai:
    api_key: ${TEST_KESTREL_OPENAI_KEY}
channels:
  telegram:
    token: ${TEST_KESTREL_TG_TOKEN}
    enabled: true
agent:
  model: gpt-4o
"#;
    std::fs::write(&config_path, yaml_content).unwrap();

    let config = load_config(Some(&config_path)).unwrap();
    assert_eq!(
        config.channels.telegram.as_ref().unwrap().token,
        "999888:XYZ"
    );
    assert_eq!(
        config.providers.openai.as_ref().unwrap().api_key.as_deref(),
        Some("sk-test-key-12345")
    );

    std::env::remove_var("TEST_KESTREL_TG_TOKEN");
    std::env::remove_var("TEST_KESTREL_OPENAI_KEY");
}

// ---------------------------------------------------------------------------
// Channel registry and manager
// ---------------------------------------------------------------------------

#[test]
fn test_registry_creates_all_builtin_channels() {
    let registry = ChannelRegistry::new();
    let names = registry.channel_names();
    assert!(names.contains(&"telegram".to_string()));
    assert!(names.contains(&"discord".to_string()));
}

#[test]
fn test_manager_creation_with_bus() {
    let registry = ChannelRegistry::new();
    let bus = MessageBus::new();
    let manager = ChannelManager::new(registry, bus);
    assert!(manager.running_channel_names().is_empty());
}

// ---------------------------------------------------------------------------
// Telegram channel unit tests
// ---------------------------------------------------------------------------

#[test]
fn test_telegram_channel_attributes() {
    std::env::remove_var("TELEGRAM_BOT_TOKEN");
    let channel = TelegramChannel::new();
    assert_eq!(channel.name(), "telegram");
    assert_eq!(channel.platform(), Platform::Telegram);
    assert!(!channel.is_connected());
}

#[tokio::test]
async fn test_telegram_connect_fails_without_token() {
    std::env::remove_var("TELEGRAM_BOT_TOKEN");
    let mut channel = TelegramChannel::new();
    let connected = channel.connect().await.unwrap();
    assert!(!connected);
}
