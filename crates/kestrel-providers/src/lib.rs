//! # kestrel-providers
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
pub mod discovery;
pub mod middleware;
pub mod openai_compat;
pub mod rate_limit;
pub mod registry;
pub mod retry;

pub use base::{CompletionRequest, CompletionResponse, LlmProvider};
pub use discovery::{build_catalog, ModelCatalog, ModelDiscovery, ModelInfo};
pub use middleware::{MiddlewareConfig, ProviderMiddleware};
pub use rate_limit::{RateLimiter, TokenBucket};
pub use registry::ProviderRegistry;
pub use retry::{CircuitBreaker, CircuitBreakerConfig, RetryConfig, RetryPolicy};

/// Build a reqwest client with a total request timeout.
///
/// Used for non-streaming requests where the entire response must be read
/// within 30 seconds.
pub(crate) fn build_client(no_proxy: bool) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .dns_resolver(kestrel_core::dns::build_dns_resolver());
    if no_proxy {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {}", e))
}

/// Build a reqwest client for SSE streaming without a total timeout.
///
/// SSE streams can last minutes. Instead of a total timeout, this client
/// uses `connect_timeout` for the TCP/TLS handshake and relies on
/// application-level idle timeouts (`parse_sse_stream`, `complete_streaming`)
/// to detect dead connections.
pub(crate) fn build_streaming_client(no_proxy: bool) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .dns_resolver(kestrel_core::dns::build_dns_resolver())
        .http1_only();
    if no_proxy {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to create streaming HTTP client: {}", e))
}
