//! # nanobot-providers
//!
//! LLM provider abstraction with support for multiple backends.
//!
//! ## Middleware
//!
//! Providers can be wrapped with [`ProviderMiddleware`] to add
//! retry logic and rate limiting. Each provider gets independent
//! configuration via [`MiddlewareConfig`].

pub mod anthropic;
pub mod base;
pub mod middleware;
pub mod openai_compat;
pub mod rate_limit;
pub mod registry;
pub mod retry;

pub use base::{CompletionRequest, CompletionResponse, LlmProvider};
pub use middleware::{MiddlewareConfig, ProviderMiddleware};
pub use rate_limit::{RateLimiter, TokenBucket};
pub use registry::ProviderRegistry;
pub use retry::RetryConfig;

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
