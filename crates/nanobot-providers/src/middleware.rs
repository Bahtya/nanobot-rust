//! Provider middleware — composable retry + rate-limit layer.
//!
//! [`ProviderMiddleware`] wraps any [`LlmProvider`] with configurable
//! retry and rate-limit policies. Each provider can be independently
//! configured via [`MiddlewareConfig`].
//!
//! # Example
//!
//! ```ignore
//! use nanobot_providers::middleware::{ProviderMiddleware, MiddlewareConfig};
//! use nanobot_providers::rate_limit::TokenBucket;
//! use nanobot_providers::retry::RetryConfig;
//! use std::time::Duration;
//!
//! let retry = RetryConfig::default().with_max_retries(3);
//! let rate_limiter = TokenBucket::new(60, Duration::from_secs(60));
//!
//! let config = MiddlewareConfig {
//!     retry: retry.into(),
//!     rate_limiter: rate_limiter.into(),
//! };
//!
//! let middleware = ProviderMiddleware::new(inner_provider, config);
//! // middleware now implements LlmProvider
//! ```

use crate::base::{BoxStream, CompletionRequest, CompletionResponse, LlmProvider};
use crate::rate_limit::RateLimiter;
use crate::retry::RetryConfig;
use std::sync::Arc;
use tracing::debug;

/// Configuration for provider middleware.
///
/// Combines retry policy and rate limiter into a single config that
/// each provider can customize independently.
#[derive(Clone)]
pub struct MiddlewareConfig {
    /// Retry policy — controls max retries and exponential backoff.
    pub retry: Arc<RetryConfig>,
    /// Rate limiter — controls request throughput.
    pub rate_limiter: Arc<dyn RateLimiter>,
}

impl std::fmt::Debug for MiddlewareConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MiddlewareConfig")
            .field("retry", &self.retry)
            .field("rate_limiter", &"<dyn RateLimiter>")
            .finish()
    }
}

impl MiddlewareConfig {
    /// Create a config with default retry and unlimited rate.
    pub fn default_unlimited() -> Self {
        Self {
            retry: Arc::new(RetryConfig::default()),
            rate_limiter: Arc::new(crate::rate_limit::UnlimitedLimiter),
        }
    }

    /// Create a config with no retries and unlimited rate.
    pub fn no_retry_unlimited() -> Self {
        Self {
            retry: Arc::new(RetryConfig::no_retries()),
            rate_limiter: Arc::new(crate::rate_limit::UnlimitedLimiter),
        }
    }

    /// Create with custom retry policy and unlimited rate.
    pub fn with_retry(retry: RetryConfig) -> Self {
        Self {
            retry: Arc::new(retry),
            rate_limiter: Arc::new(crate::rate_limit::UnlimitedLimiter),
        }
    }

    /// Create with default retry and a token-bucket rate limiter.
    ///
    /// - `requests_per_minute`: Maximum requests allowed per minute.
    pub fn with_rate_limit(requests_per_minute: u64) -> Self {
        use crate::rate_limit::TokenBucket;
        use std::time::Duration;
        Self {
            retry: Arc::new(RetryConfig::default()),
            rate_limiter: Arc::new(TokenBucket::new(
                requests_per_minute,
                Duration::from_secs(60),
            )),
        }
    }

    /// Create with custom retry and rate-limit bucket.
    pub fn with_retry_and_rate_limit(
        retry: RetryConfig,
        requests_per_minute: u64,
    ) -> Self {
        use crate::rate_limit::TokenBucket;
        use std::time::Duration;
        Self {
            retry: Arc::new(retry),
            rate_limiter: Arc::new(TokenBucket::new(
                requests_per_minute,
                Duration::from_secs(60),
            )),
        }
    }
}

/// Middleware wrapping an inner [`LlmProvider`] with retry and rate-limit policies.
///
/// This implements `LlmProvider` so it can be used anywhere a provider is expected.
/// All calls pass through rate limiting (wait for a token) then retry logic
/// (exponential backoff on transient errors).
pub struct ProviderMiddleware {
    inner: Arc<dyn LlmProvider>,
    config: MiddlewareConfig,
}

impl ProviderMiddleware {
    /// Create new middleware wrapping an inner provider.
    pub fn new(provider: impl LlmProvider + 'static, config: MiddlewareConfig) -> Self {
        Self {
            inner: Arc::new(provider),
            config,
        }
    }

    /// Create from an already-Arc'd provider.
    pub fn from_arc(provider: Arc<dyn LlmProvider>, config: MiddlewareConfig) -> Self {
        Self {
            inner: provider,
            config,
        }
    }
}

impl std::fmt::Debug for ProviderMiddleware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderMiddleware")
            .field("inner", &self.inner.name())
            .field("config", &self.config)
            .finish()
    }
}

#[async_trait::async_trait]
impl LlmProvider for ProviderMiddleware {
    fn name(&self) -> &str {
        self.inner.name()
    }

    async fn complete(&self, request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
        // 1. Rate limit: acquire a token (may block)
        self.config.rate_limiter.acquire().await;
        debug!(provider = self.inner.name(), "Rate limit token acquired for complete()");

        // 2. Retry with exponential backoff
        let inner = self.inner.clone();
        let retry_config = self.config.retry.clone();
        crate::retry::retry_with_backoff(&retry_config, move |_attempt| {
            let inner = inner.clone();
            let req = request.clone();
            async move { inner.complete(req).await }
        })
        .await
    }

    async fn complete_stream(&self, request: CompletionRequest) -> anyhow::Result<BoxStream> {
        // 1. Rate limit: acquire a token (may block)
        self.config.rate_limiter.acquire().await;
        debug!(provider = self.inner.name(), "Rate limit token acquired for complete_stream()");

        // 2. Retry with exponential backoff
        // Note: we only retry the initial HTTP connection/stream-open.
        // Once the stream is established, retries are not applicable.
        let inner = self.inner.clone();
        let retry_config = self.config.retry.clone();
        crate::retry::retry_with_backoff(&retry_config, move |_attempt| {
            let inner = inner.clone();
            let req = request.clone();
            async move { inner.complete_stream(req).await }
        })
        .await
    }

    fn supports_model(&self, model: &str) -> bool {
        self.inner.supports_model(model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::CompletionChunk;
    use crate::rate_limit::TokenBucket;
    use crate::retry::RetryConfig;
    use nanobot_core::Usage;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    /// Mock provider that tracks call count.
    struct MockProvider {
        call_count: AtomicU32,
        fail_until: AtomicU32,
    }

    impl MockProvider {
        fn new() -> Self {
            Self {
                call_count: AtomicU32::new(0),
                fail_until: AtomicU32::new(0),
            }
        }

        fn fail_n_times(&self, n: u32) {
            self.fail_until.store(n, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for MockProvider {
        fn name(&self) -> &str { "mock_middleware" }

        async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<CompletionResponse> {
            let n = self.call_count.fetch_add(1, Ordering::SeqCst);
            let fail_until = self.fail_until.load(Ordering::SeqCst);
            if n < fail_until {
                anyhow::bail!("API error (429): rate limited");
            }
            Ok(CompletionResponse {
                content: Some(format!("response-{}", n)),
                tool_calls: None,
                usage: Some(Usage {
                    prompt_tokens: Some(10),
                    completion_tokens: Some(5),
                    total_tokens: Some(15),
                }),
                finish_reason: Some("stop".to_string()),
            })
        }

        async fn complete_stream(&self, req: CompletionRequest) -> anyhow::Result<BoxStream> {
            let resp = self.complete(req).await?;
            let chunk = CompletionChunk {
                delta: resp.content,
                tool_call_deltas: None,
                usage: resp.usage,
                done: true,
            };
            Ok(Box::pin(futures::stream::once(async move { Ok(chunk) })))
        }

        fn supports_model(&self, _model: &str) -> bool { true }
    }

    // -------------------------------------------------------------------
    // MiddlewareConfig tests
    // -------------------------------------------------------------------

    #[test]
    fn test_middleware_config_default_unlimited() {
        let config = MiddlewareConfig::default_unlimited();
        assert_eq!(config.retry.max_retries, 3);
        assert!(config.rate_limiter.try_acquire());
    }

    #[test]
    fn test_middleware_config_no_retry() {
        let config = MiddlewareConfig::no_retry_unlimited();
        assert_eq!(config.retry.max_retries, 0);
    }

    #[test]
    fn test_middleware_config_with_retry() {
        let config = MiddlewareConfig::with_retry(RetryConfig::default().with_max_retries(5));
        assert_eq!(config.retry.max_retries, 5);
    }

    #[test]
    fn test_middleware_config_with_rate_limit() {
        let config = MiddlewareConfig::with_rate_limit(10);
        assert_eq!(config.retry.max_retries, 3);
        // Should be able to acquire tokens
        assert!(config.rate_limiter.try_acquire());
    }

    #[test]
    fn test_middleware_config_debug() {
        let config = MiddlewareConfig::default_unlimited();
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("MiddlewareConfig"));
    }

    // -------------------------------------------------------------------
    // ProviderMiddleware tests
    // -------------------------------------------------------------------

    fn make_request() -> CompletionRequest {
        CompletionRequest {
            model: "test-model".to_string(),
            messages: vec![nanobot_core::Message {
                role: nanobot_core::MessageRole::User,
                content: "hello".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            tools: None,
            max_tokens: Some(100),
            temperature: Some(0.7),
            stream: false,
        }
    }

    #[tokio::test]
    async fn test_middleware_basic_complete() {
        let mock = Arc::new(MockProvider::new());
        let config = MiddlewareConfig::no_retry_unlimited();
        let middleware = ProviderMiddleware::from_arc(mock.clone(), config);

        let result = middleware.complete(make_request()).await.unwrap();
        assert_eq!(result.content, Some("response-0".to_string()));
        assert_eq!(mock.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_middleware_retry_on_429() {
        let mock = Arc::new(MockProvider::new());
        mock.fail_n_times(2); // Fail twice, succeed on third
        let config = MiddlewareConfig::with_retry(RetryConfig::default().with_max_retries(3));
        let middleware = ProviderMiddleware::from_arc(mock.clone(), config);

        let result = middleware.complete(make_request()).await.unwrap();
        assert_eq!(result.content, Some("response-2".to_string()));
        assert_eq!(mock.call_count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_middleware_retry_exhausted() {
        let mock = Arc::new(MockProvider::new());
        mock.fail_n_times(100); // Always fail
        let config = MiddlewareConfig::with_retry(RetryConfig::default().with_max_retries(2));
        let middleware = ProviderMiddleware::from_arc(mock.clone(), config);

        let result = middleware.complete(make_request()).await;
        assert!(result.is_err());
        // Should have tried: initial + 2 retries = 3 attempts
        assert_eq!(mock.call_count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_middleware_rate_limiting() {
        let mock = Arc::new(MockProvider::new());
        let bucket = Arc::new(TokenBucket::new(2, Duration::from_secs(60)));
        let config = MiddlewareConfig {
            retry: Arc::new(RetryConfig::no_retries()),
            rate_limiter: bucket.clone(),
        };
        let middleware = ProviderMiddleware::from_arc(mock.clone(), config);

        // First two should succeed immediately
        let r1 = middleware.complete(make_request()).await.unwrap();
        assert_eq!(r1.content, Some("response-0".to_string()));
        let r2 = middleware.complete(make_request()).await.unwrap();
        assert_eq!(r2.content, Some("response-1".to_string()));

        // Third should block — but we can check available tokens
        assert_eq!(bucket.available(), 0);
    }

    #[tokio::test]
    async fn test_middleware_streaming() {
        let mock = Arc::new(MockProvider::new());
        let config = MiddlewareConfig::no_retry_unlimited();
        let middleware = ProviderMiddleware::from_arc(mock.clone(), config);

        let stream = middleware.complete_stream(make_request()).await.unwrap();
        use futures::StreamExt;
        let chunks: Vec<_> = stream.collect().await;
        assert_eq!(chunks.len(), 1);
        let chunk = chunks[0].as_ref().unwrap();
        assert_eq!(chunk.delta, Some("response-0".to_string()));
    }

    #[tokio::test]
    async fn test_middleware_supports_model() {
        let mock = Arc::new(MockProvider::new());
        let config = MiddlewareConfig::no_retry_unlimited();
        let middleware = ProviderMiddleware::from_arc(mock, config);
        assert!(middleware.supports_model("anything"));
    }

    #[tokio::test]
    async fn test_middleware_name() {
        let mock = Arc::new(MockProvider::new());
        let config = MiddlewareConfig::no_retry_unlimited();
        let middleware = ProviderMiddleware::from_arc(mock, config);
        assert_eq!(middleware.name(), "mock_middleware");
    }

    #[tokio::test]
    async fn test_middleware_debug() {
        let mock = Arc::new(MockProvider::new());
        let config = MiddlewareConfig::no_retry_unlimited();
        let middleware = ProviderMiddleware::from_arc(mock, config);
        let debug_str = format!("{:?}", middleware);
        assert!(debug_str.contains("ProviderMiddleware"));
        assert!(debug_str.contains("mock_middleware"));
    }

    // -------------------------------------------------------------------
    // Concurrent rate-limit test
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_middleware_concurrent_rate_limit() {
        let mock = Arc::new(MockProvider::new());
        // Allow 5 concurrent tokens, refill slowly
        let bucket = Arc::new(TokenBucket::new(5, Duration::from_secs(60)));
        let config = MiddlewareConfig {
            retry: Arc::new(RetryConfig::no_retries()),
            rate_limiter: bucket.clone(),
        };
        let middleware = Arc::new(ProviderMiddleware::from_arc(mock.clone(), config));

        let mut handles = Vec::new();
        for _ in 0..5 {
            let mw = middleware.clone();
            handles.push(tokio::spawn(async move {
                mw.complete(make_request()).await.unwrap()
            }));
        }

        // All 5 should succeed
        for handle in handles {
            let result = handle.await.unwrap();
            assert!(result.content.is_some());
        }

        // Bucket should be empty now
        assert_eq!(bucket.available(), 0);
        assert_eq!(mock.call_count.load(Ordering::SeqCst), 5);
    }
}
