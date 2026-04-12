//! # nanobot-providers
//!
//! LLM provider abstraction with support for multiple backends.

pub mod anthropic;
pub mod base;
pub mod openai_compat;
pub mod registry;

pub use base::{CompletionRequest, CompletionResponse, LlmProvider};
pub use registry::ProviderRegistry;

/// Build a reqwest client with proper proxy handling.
///
/// By default, reqwest reads `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY` from the environment.
/// When `no_proxy` is true, all proxy env vars are ignored (for domestic APIs like ZAI).
pub(crate) fn build_client(no_proxy: bool) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(120));
    if no_proxy {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {}", e))
}
