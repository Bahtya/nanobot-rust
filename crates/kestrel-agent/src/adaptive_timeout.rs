//! Adaptive timeout resolution.
//!
//! Computes per-request timeout values based on context size, model type,
//! provider locality, and per-model overrides from config.

use std::time::Duration;

// ── Public types ───────────────────────────────────────────────

/// Timeout values after applying all adaptive adjustments.
#[derive(Debug, Clone)]
pub struct ResolvedTimeouts {
    /// Max wait for the first SSE chunk.
    pub first_byte_timeout: Duration,
    /// Max idle time between consecutive content chunks.
    pub idle_timeout: Duration,
    /// Overall per-message hard limit (used by the agent-loop wrapper).
    pub message_timeout: Duration,
    /// TCP/TLS connect timeout for the HTTP client.
    pub connect_timeout: Duration,
    /// Absolute ceiling for the stream-health monitor — beyond this
    /// the stream is declared dead regardless of token flow.
    pub absolute_max: Duration,
}

// ── Token estimation ───────────────────────────────────────────

/// Rough token estimate: ~4 chars per token for English/code, ~2 for CJK.
/// We use 4 as a conservative over-estimate that errs on longer timeouts.
fn estimate_chars(messages: &[kestrel_core::types::Message]) -> usize {
    messages.iter().map(|m| m.content.chars().count()).sum()
}

/// Map estimated char count → scaling multiplier for first_byte_timeout.
fn context_scale(chars: usize) -> u32 {
    let tokens = chars / 4;
    if tokens < 10_000 {
        1
    } else if tokens < 50_000 {
        2
    } else if tokens < 100_000 {
        4
    } else {
        8
    }
}

// ── Local provider detection ───────────────────────────────────

fn is_local_provider(config: &kestrel_config::Config, provider_name: &str) -> bool {
    // Named local providers
    if provider_name == "ollama" {
        return true;
    }

    // Check custom providers for localhost URLs
    for custom in &config.custom_providers {
        if custom.name == provider_name {
            return is_localhost_url(&custom.base_url);
        }
    }

    // Check the *named* standard provider's base_url — not all providers
    if let Some(entry) = lookup_provider_entry(config, provider_name) {
        if let Some(ref url) = entry.base_url {
            if is_localhost_url(url) {
                return true;
            }
        }
    }

    false
}

fn is_localhost_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains("localhost")
        || lower.contains("127.0.0.1")
        || lower.contains("[::1]")
        || lower.contains("0.0.0.0")
}

/// Look up a standard provider entry by name.
fn lookup_provider_entry<'a>(
    config: &'a kestrel_config::Config,
    provider_name: &str,
) -> Option<&'a kestrel_config::schema::ProviderEntry> {
    match provider_name {
        "anthropic" => config.providers.anthropic.as_ref(),
        "openai" => config.providers.openai.as_ref(),
        "openrouter" => config.providers.openrouter.as_ref(),
        "ollama" => config.providers.ollama.as_ref(),
        "deepseek" => config.providers.deepseek.as_ref(),
        "gemini" => config.providers.gemini.as_ref(),
        "groq" => config.providers.groq.as_ref(),
        "moonshot" => config.providers.moonshot.as_ref(),
        "minimax" => config.providers.minimax.as_ref(),
        "github_copilot" => config.providers.github_copilot.as_ref(),
        "openai_codex" => config.providers.openai_codex.as_ref(),
        "opencode_go" => config.providers.opencode_go.as_ref(),
        "glm_coding_plan" => config.providers.glm_coding_plan.as_ref(),
        _ => None,
    }
}

// ── Model override lookup ──────────────────────────────────────

fn lookup_model_overrides<'a>(
    config: &'a kestrel_config::Config,
    provider_name: &str,
    model: &str,
) -> Option<&'a kestrel_config::schema::ModelTimeoutOverrides> {
    let entry = lookup_provider_entry(config, provider_name);

    if let Some(e) = entry {
        // Exact match first
        if let Some(ov) = e.model_timeouts.get(model) {
            return Some(ov);
        }
        // Wildcard suffix match (e.g. "claude-opus*" matches "claude-opus-4-7")
        for (pattern, overrides) in &e.model_timeouts {
            if let Some(prefix) = pattern.strip_suffix('*') {
                if model.starts_with(prefix) {
                    return Some(overrides);
                }
            }
        }
    }

    None
}

// ── Main resolver ──────────────────────────────────────────────

/// Compute adaptive timeout values for a single LLM request.
pub fn resolve_timeouts(
    config: &kestrel_config::Config,
    messages: &[kestrel_core::types::Message],
    model: &str,
    provider_name: &str,
) -> ResolvedTimeouts {
    let agent = &config.agent;

    let mut first_byte = agent.first_byte_timeout;
    let mut idle = agent.idle_timeout;
    let message = agent.message_timeout;
    let connect = agent.connect_timeout;

    // Scale first_byte_timeout by context size
    let chars = estimate_chars(messages);
    let scale = context_scale(chars);
    first_byte = first_byte.saturating_mul(scale as u64);

    // Local provider: generous timeouts for CPU-bound inference
    if is_local_provider(config, provider_name) {
        first_byte = first_byte.max(120);
        idle = idle.max(180);
    }

    // Per-model overrides from config (single lookup, reused for absolute_max)
    let model_overrides = lookup_model_overrides(config, provider_name, model);
    if let Some(ov) = model_overrides {
        if let Some(v) = ov.first_byte_timeout {
            first_byte = v;
        }
        if let Some(v) = ov.idle_timeout {
            idle = v;
        }
    }

    // absolute_max defaults to the larger of idle*10 or 600s,
    // can be overridden per-model
    let absolute_max = model_overrides
        .and_then(|ov| ov.absolute_max)
        .unwrap_or_else(|| (idle * 10).max(600));

    ResolvedTimeouts {
        first_byte_timeout: Duration::from_secs(first_byte),
        idle_timeout: Duration::from_secs(idle),
        message_timeout: Duration::from_secs(message),
        connect_timeout: Duration::from_secs(connect),
        absolute_max: Duration::from_secs(absolute_max),
    }
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_config::Config;

    fn default_config() -> Config {
        Config::default()
    }

    #[test]
    fn estimate_chars_empty() {
        assert_eq!(estimate_chars(&[]), 0);
    }

    #[test]
    fn estimate_chars_ascii() {
        let msgs = vec![kestrel_core::types::Message {
            role: kestrel_core::types::MessageRole::User,
            content: "Hello world!".to_string(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
        }];
        assert_eq!(estimate_chars(&msgs), 12);
    }

    #[test]
    fn context_scale_tiers() {
        // ~2500 tokens (10000 chars / 4)
        assert_eq!(context_scale(10_000 - 1), 1); // < 10K tokens
        assert_eq!(context_scale(10_000 * 4), 2); // 10K tokens
        assert_eq!(context_scale(50_000 * 4), 4); // 50K tokens
        assert_eq!(context_scale(100_000 * 4), 8); // 100K tokens
    }

    #[test]
    fn resolve_defaults_small_context() {
        let config = default_config();
        let msgs = vec![];
        let t = resolve_timeouts(&config, &msgs, "gpt-4o", "openai");

        assert_eq!(t.first_byte_timeout, Duration::from_secs(15));
        assert_eq!(t.idle_timeout, Duration::from_secs(60));
        assert_eq!(t.message_timeout, Duration::from_secs(300));
        assert_eq!(t.connect_timeout, Duration::from_secs(10));
        // absolute_max = max(idle*10, 600) = max(600, 600) = 600
        assert_eq!(t.absolute_max, Duration::from_secs(600));
    }

    #[test]
    fn resolve_scales_with_context() {
        let config = default_config();
        // 50K tokens = 200K chars → scale 4
        let big_msg = kestrel_core::types::Message {
            role: kestrel_core::types::MessageRole::User,
            content: "x".repeat(200_000),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
        };
        let t = resolve_timeouts(&config, &[big_msg], "gpt-4o", "openai");
        // first_byte = 15 * 4 = 60
        assert_eq!(t.first_byte_timeout, Duration::from_secs(60));
    }

    #[test]
    fn is_localhost_various() {
        assert!(is_localhost_url("http://localhost:11434/v1"));
        assert!(is_localhost_url("http://127.0.0.1:8080"));
        assert!(is_localhost_url("http://[::1]:8080"));
        assert!(is_localhost_url("http://0.0.0.0:11434"));
        assert!(!is_localhost_url("https://api.openai.com/v1"));
    }

    #[test]
    fn resolve_local_provider_ollama() {
        let config = default_config();
        let msgs = vec![];
        let t = resolve_timeouts(&config, &msgs, "llama3", "ollama");
        assert!(t.first_byte_timeout >= Duration::from_secs(120));
        assert!(t.idle_timeout >= Duration::from_secs(180));
    }

    #[test]
    fn absolute_max_default_calculation() {
        let config = default_config();
        let msgs = vec![];
        let t = resolve_timeouts(&config, &msgs, "gpt-4o", "openai");
        // idle=60, max(60*10, 600) = 600
        assert_eq!(t.absolute_max, Duration::from_secs(600));
    }

    #[test]
    fn resolve_overrides_from_config() {
        let mut config = default_config();
        config.providers.anthropic = Some(kestrel_config::schema::ProviderEntry {
            model_timeouts: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "claude-opus*".to_string(),
                    kestrel_config::schema::ModelTimeoutOverrides {
                        first_byte_timeout: Some(120),
                        idle_timeout: Some(180),
                        absolute_max: Some(900),
                    },
                );
                map
            },
            ..Default::default()
        });

        let msgs = vec![];
        let t = resolve_timeouts(&config, &msgs, "claude-opus-4-7", "anthropic");
        assert_eq!(t.first_byte_timeout, Duration::from_secs(120));
        assert_eq!(t.idle_timeout, Duration::from_secs(180));
        assert_eq!(t.absolute_max, Duration::from_secs(900));
    }

    #[test]
    fn wildcard_no_match_falls_through() {
        let mut config = default_config();
        config.providers.anthropic = Some(kestrel_config::schema::ProviderEntry {
            model_timeouts: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "claude-opus*".to_string(),
                    kestrel_config::schema::ModelTimeoutOverrides {
                        first_byte_timeout: Some(999),
                        ..Default::default()
                    },
                );
                map
            },
            ..Default::default()
        });

        let msgs = vec![];
        let t = resolve_timeouts(&config, &msgs, "claude-sonnet-4-6", "anthropic");
        // "claude-opus*" doesn't match "claude-sonnet-4-6"
        assert_eq!(t.first_byte_timeout, Duration::from_secs(15));
    }
}
