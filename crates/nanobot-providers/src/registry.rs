//! Provider registry — resolves model names to appropriate providers.
//!
//! Matches the Python `providers/registry.py` keyword-based model matching.

use crate::anthropic::{AnthropicConfig, AnthropicProvider};
use crate::base::LlmProvider;
use crate::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use anyhow::Result;
use nanobot_config::schema::Config;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info};

/// Model keyword to provider name mapping.
/// This mirrors the Python registry's MODEL_KEYWORDS map.
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

/// Registry of LLM providers.
#[derive(Clone)]
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
    default_provider: Option<String>,
}

impl ProviderRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            default_provider: None,
        }
    }

    /// Build the registry from a Config.
    pub fn from_config(config: &Config) -> Result<Self> {
        let mut registry = Self::new();

        // Register Anthropic provider if configured
        if let Some(entry) = &config.providers.anthropic {
            if let Some(api_key) = &entry.api_key {
                let provider = AnthropicProvider::new(AnthropicConfig {
                    api_key: api_key.clone(),
                    model: entry
                        .model
                        .clone()
                        .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string()),
                    api_version: None,
                    base_url: entry.base_url.clone(),
                })?;
                registry.register("anthropic", provider);
                info!("Registered Anthropic provider");
            }
        }

        // Register OpenAI provider if configured
        if let Some(entry) = &config.providers.openai {
            if let Some(api_key) = &entry.api_key {
                let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
                    api_key: api_key.clone(),
                    base_url: entry
                        .base_url
                        .clone()
                        .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
                    model: entry.model.clone().unwrap_or_default(),
                    organization: None,
                    no_proxy: entry.no_proxy.unwrap_or(false),
                })?;
                registry.register("openai", provider);
                info!("Registered OpenAI provider");
            }
        }

        // Register DeepSeek provider
        if let Some(entry) = &config.providers.deepseek {
            if let Some(api_key) = &entry.api_key {
                let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
                    api_key: api_key.clone(),
                    base_url: entry
                        .base_url
                        .clone()
                        .unwrap_or_else(|| "https://api.deepseek.com/v1".to_string()),
                    model: entry.model.clone().unwrap_or_default(),
                    organization: None,
                    no_proxy: entry.no_proxy.unwrap_or(false),
                })?;
                registry.register("deepseek", provider);
                info!("Registered DeepSeek provider");
            }
        }

        // Register Groq provider
        if let Some(entry) = &config.providers.groq {
            if let Some(api_key) = &entry.api_key {
                let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
                    api_key: api_key.clone(),
                    base_url: entry
                        .base_url
                        .clone()
                        .unwrap_or_else(|| "https://api.groq.com/openai/v1".to_string()),
                    model: entry.model.clone().unwrap_or_default(),
                    organization: None,
                    no_proxy: entry.no_proxy.unwrap_or(false),
                })?;
                registry.register("groq", provider);
                info!("Registered Groq provider");
            }
        }

        // Register OpenRouter provider
        if let Some(entry) = &config.providers.openrouter {
            if let Some(api_key) = &entry.api_key {
                let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
                    api_key: api_key.clone(),
                    base_url: entry
                        .base_url
                        .clone()
                        .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string()),
                    model: entry.model.clone().unwrap_or_default(),
                    organization: None,
                    no_proxy: entry.no_proxy.unwrap_or(false),
                })?;
                registry.register("openrouter", provider);
                info!("Registered OpenRouter provider");
            }
        }

        // Register Ollama provider (localhost — always skip proxy)
        if let Some(entry) = &config.providers.ollama {
            let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
                api_key: entry.api_key.clone().unwrap_or_default(),
                base_url: entry
                    .base_url
                    .clone()
                    .unwrap_or_else(|| "http://localhost:11434/v1".to_string()),
                model: entry.model.clone().unwrap_or_default(),
                organization: None,
                no_proxy: true,
            })?;
            registry.register("ollama", provider);
            info!("Registered Ollama provider");
        }

        // Register custom providers
        for custom in &config.custom_providers {
            let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
                api_key: custom.api_key.clone().unwrap_or_default(),
                base_url: custom.base_url.clone(),
                model: String::new(),
                organization: None,
                no_proxy: custom.no_proxy.unwrap_or(false),
            })?;
            registry.register(&custom.name, provider);
            info!("Registered custom provider: {}", custom.name);
        }

        // Set default provider based on agent model
        let model = &config.agent.model;
        let default = registry
            .resolve_provider_name(model)
            .map(|s| s.to_string())
            .or_else(|| registry.providers.keys().next().cloned());
        registry.default_provider = default;
        if let Some(ref name) = registry.default_provider {
            debug!("Default provider for model '{}': {}", model, name);
        }

        Ok(registry)
    }

    /// Register a provider.
    pub fn register(&mut self, name: &str, provider: impl LlmProvider + 'static) {
        self.providers.insert(name.to_string(), Arc::new(provider));
    }

    /// Set the default provider by name.
    pub fn set_default(&mut self, name: &str) {
        if self.providers.contains_key(name) {
            self.default_provider = Some(name.to_string());
        }
    }

    /// Resolve a model name to a provider name.
    pub fn resolve_provider_name(&self, model: &str) -> Option<&str> {
        let lower = model.to_lowercase();
        for (keyword, provider_name) in MODEL_KEYWORD_MAP {
            if lower.contains(keyword) && self.providers.contains_key(*provider_name) {
                return Some(provider_name);
            }
        }
        self.default_provider.as_deref()
    }

    /// Get a provider for a given model.
    pub fn get_provider(&self, model: &str) -> Option<Arc<dyn LlmProvider>> {
        if let Some(name) = self.resolve_provider_name(model) {
            self.providers.get(name).cloned()
        } else {
            self.default_provider
                .as_ref()
                .and_then(|name| self.providers.get(name).cloned())
        }
    }

    /// Get a provider by name.
    pub fn get_provider_by_name(&self, name: &str) -> Option<Arc<dyn LlmProvider>> {
        self.providers.get(name).cloned()
    }

    /// List all registered provider names.
    pub fn provider_names(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::{
        BoxStream, CompletionChunk, CompletionRequest, CompletionResponse, LlmProvider,
    };
    use async_trait::async_trait;

    struct MockProvider {
        provider_name: String,
        supported_model: String,
    }

    impl MockProvider {
        fn new(name: &str, supported: &str) -> Self {
            Self {
                provider_name: name.to_string(),
                supported_model: supported.to_string(),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        fn name(&self) -> &str {
            &self.provider_name
        }
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> anyhow::Result<CompletionResponse> {
            Ok(CompletionResponse {
                content: Some("mock".to_string()),
                tool_calls: None,
                usage: None,
                finish_reason: None,
            })
        }
        async fn complete_stream(&self, request: CompletionRequest) -> anyhow::Result<BoxStream> {
            let response = self.complete(request).await?;
            let chunk = CompletionChunk {
                delta: response.content,
                tool_call_deltas: None,
                usage: None,
                done: true,
            };
            Ok(Box::pin(futures::stream::once(async move { Ok(chunk) })))
        }
        fn supports_model(&self, model: &str) -> bool {
            model.contains(&self.supported_model)
        }
    }

    #[test]
    fn test_registry_new() {
        let reg = ProviderRegistry::new();
        assert!(reg.provider_names().is_empty());
    }

    #[test]
    fn test_registry_register() {
        let mut reg = ProviderRegistry::new();
        reg.register("mock", MockProvider::new("mock", "test"));
        let names = reg.provider_names();
        assert_eq!(names.len(), 1);
        assert!(names.contains(&"mock".to_string()));
    }

    #[test]
    fn test_registry_resolve_provider_name() {
        let mut reg = ProviderRegistry::new();
        reg.register("anthropic", MockProvider::new("anthropic", "claude"));
        reg.register("openai", MockProvider::new("openai", "gpt"));

        // "claude-3.5-sonnet" should resolve to "anthropic"
        let resolved = reg.resolve_provider_name("claude-3.5-sonnet");
        assert_eq!(resolved, Some("anthropic"));

        // "gpt-4o" should resolve to "openai"
        let resolved = reg.resolve_provider_name("gpt-4o");
        assert_eq!(resolved, Some("openai"));
    }

    #[test]
    fn test_registry_default_provider() {
        let reg = ProviderRegistry::default();
        assert!(reg.provider_names().is_empty());
        assert!(reg.default_provider.is_none());
    }
}
