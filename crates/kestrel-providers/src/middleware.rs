//! Provider middleware — composable retry + rate-limit layer.
//!
//! [`ProviderMiddleware`] wraps any [`LlmProvider`] with configurable
//! retry and rate-limit policies. Each provider can be independently
//! configured via [`MiddlewareConfig`].
//!
//! # Example
//!
//! ```ignore
//! use kestrel_providers::middleware::{ProviderMiddleware, MiddlewareConfig};
//! use kestrel_providers::rate_limit::TokenBucket;
//! use kestrel_providers::retry::RetryConfig;
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
use crate::retry::{CircuitBreaker, CircuitBreakerConfig, RetryConfig, RetryPolicy};
use std::sync::Arc;
use tracing::debug;

/// Configuration for provider middleware.
///
/// Combines retry policy, rate limiter, and optional circuit breaker
/// into a single config that each provider can customize independently.
#[derive(Clone)]
pub struct MiddlewareConfig {
    /// Retry policy — controls max retries and exponential backoff.
    pub retry: Arc<RetryConfig>,
    /// Rate limiter — controls request throughput.
    pub rate_limiter: Arc<dyn RateLimiter>,
    /// Optional circuit breaker for fail-fast behavior.
    pub circuit_breaker: Option<Arc<CircuitBreaker>>,
}

impl std::fmt::Debug for MiddlewareConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MiddlewareConfig")
            .field("retry", &self.retry)
            .field("rate_limiter", &"<dyn RateLimiter>")
            .field("circuit_breaker", &self.circuit_breaker)
            .finish()
    }
}

impl MiddlewareConfig {
    /// Create a config with default retry and unlimited rate.
    pub fn default_unlimited() -> Self {
        Self {
            retry: Arc::new(RetryConfig::default()),
            rate_limiter: Arc::new(crate::rate_limit::UnlimitedLimiter),
            circuit_breaker: None,
        }
    }

    /// Create a config with no retries and unlimited rate.
    pub fn no_retry_unlimited() -> Self {
        Self {
            retry: Arc::new(RetryConfig::no_retries()),
            rate_limiter: Arc::new(crate::rate_limit::UnlimitedLimiter),
            circuit_breaker: None,
        }
    }

    /// Create with custom retry policy and unlimited rate.
    pub fn with_retry(retry: RetryConfig) -> Self {
        Self {
            retry: Arc::new(retry),
            rate_limiter: Arc::new(crate::rate_limit::UnlimitedLimiter),
            circuit_breaker: None,
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
            circuit_breaker: None,
        }
    }

    /// Create with custom retry and rate-limit bucket.
    pub fn with_retry_and_rate_limit(retry: RetryConfig, requests_per_minute: u64) -> Self {
        use crate::rate_limit::TokenBucket;
        use std::time::Duration;
        Self {
            retry: Arc::new(retry),
            rate_limiter: Arc::new(TokenBucket::new(
                requests_per_minute,
                Duration::from_secs(60),
            )),
            circuit_breaker: None,
        }
    }

    /// Create with a circuit breaker using default config.
    pub fn with_circuit_breaker(self) -> Self {
        Self {
            circuit_breaker: Some(Arc::new(CircuitBreaker::defaults())),
            ..self
        }
    }

    /// Create with a circuit breaker using custom config.
    pub fn with_circuit_breaker_config(self, config: CircuitBreakerConfig) -> Self {
        Self {
            circuit_breaker: Some(Arc::new(CircuitBreaker::new(config))),
            ..self
        }
    }

    /// Create from a [`RetryPolicy`] with circuit breaker support.
    ///
    /// Converts the policy to a legacy [`RetryConfig`] and enables
    /// a circuit breaker with default settings.
    pub fn from_retry_policy(policy: RetryPolicy) -> Self {
        let cb = Arc::new(CircuitBreaker::defaults());
        Self {
            retry: Arc::new(RetryConfig::from(policy)),
            rate_limiter: Arc::new(crate::rate_limit::UnlimitedLimiter),
            circuit_breaker: Some(cb),
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
        // 1. Circuit breaker check (fail-fast if open)
        if let Some(ref cb) = self.config.circuit_breaker {
            cb.allow_request().map_err(|e| anyhow::anyhow!(e))?;
        }

        // 2. Rate limit: acquire a token (may block)
        self.config.rate_limiter.acquire().await;
        debug!(
            provider = self.inner.name(),
            "Rate limit token acquired for complete()"
        );

        // 3. Retry with exponential backoff + circuit breaker recording
        let inner = self.inner.clone();
        let retry_config = self.config.retry.clone();
        let cb = self.config.circuit_breaker.clone();
        crate::retry::retry_with_backoff(&retry_config, move |_attempt| {
            let inner = inner.clone();
            let req = request.clone();
            let cb = cb.clone();
            async move {
                let result = inner.complete(req).await;
                match &result {
                    Ok(_) => {
                        if let Some(ref cb) = cb {
                            cb.record_success();
                        }
                    }
                    Err(_) => {
                        if let Some(ref cb) = cb {
                            cb.record_failure();
                        }
                    }
                }
                result
            }
        })
        .await
    }

    async fn complete_stream(&self, request: CompletionRequest) -> anyhow::Result<BoxStream> {
        // 1. Circuit breaker check (fail-fast if open)
        if let Some(ref cb) = self.config.circuit_breaker {
            cb.allow_request().map_err(|e| anyhow::anyhow!(e))?;
        }

        // 2. Rate limit: acquire a token (may block)
        self.config.rate_limiter.acquire().await;
        debug!(
            provider = self.inner.name(),
            "Rate limit token acquired for complete_stream()"
        );

        // 3. Retry with exponential backoff + circuit breaker recording
        // Note: we only retry the initial HTTP connection/stream-open.
        // Once the stream is established, retries are not applicable.
        let inner = self.inner.clone();
        let retry_config = self.config.retry.clone();
        let cb = self.config.circuit_breaker.clone();
        crate::retry::retry_with_backoff(&retry_config, move |_attempt| {
            let inner = inner.clone();
            let req = request.clone();
            let cb = cb.clone();
            async move {
                let result = inner.complete_stream(req).await;
                match &result {
                    Ok(_) => {
                        if let Some(ref cb) = cb {
                            cb.record_success();
                        }
                    }
                    Err(_) => {
                        if let Some(ref cb) = cb {
                            cb.record_failure();
                        }
                    }
                }
                result
            }
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
    use kestrel_core::Usage;
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
        fn name(&self) -> &str {
            "mock_middleware"
        }

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

        fn supports_model(&self, _model: &str) -> bool {
            true
        }
    }

    /// Mock provider that returns 503 errors for testing 503-specific retry.
    struct MockProvider503 {
        call_count: AtomicU32,
        fail_until: AtomicU32,
    }

    impl MockProvider503 {
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
    impl LlmProvider for MockProvider503 {
        fn name(&self) -> &str {
            "mock_503"
        }

        async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<CompletionResponse> {
            let n = self.call_count.fetch_add(1, Ordering::SeqCst);
            let fail_until = self.fail_until.load(Ordering::SeqCst);
            if n < fail_until {
                anyhow::bail!("API error (503): service unavailable");
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

        fn supports_model(&self, _model: &str) -> bool {
            true
        }
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
            messages: vec![kestrel_core::Message {
                role: kestrel_core::MessageRole::User,
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
            circuit_breaker: None,
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
            circuit_breaker: None,
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

    // -------------------------------------------------------------------
    // Circuit breaker integration tests
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_middleware_circuit_breaker_trips() {
        let mock = Arc::new(MockProvider::new());
        mock.fail_n_times(100); // Always fail
        let cb = Arc::new(CircuitBreaker::new(
            CircuitBreakerConfig::default().with_failure_threshold(3),
        ));
        let config = MiddlewareConfig {
            retry: Arc::new(RetryConfig::no_retries()),
            rate_limiter: Arc::new(crate::rate_limit::UnlimitedLimiter),
            circuit_breaker: Some(cb.clone()),
        };
        let middleware = ProviderMiddleware::from_arc(mock.clone(), config);

        // Fail 3 times to trip the breaker
        for _ in 0..3 {
            let _ = middleware.complete(make_request()).await;
        }
        assert_eq!(cb.state_name(), "OPEN");
        assert_eq!(mock.call_count.load(Ordering::SeqCst), 3);

        // Next request should be rejected by circuit breaker (fail-fast)
        let result = middleware.complete(make_request()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("OPEN"));
        // Call count should NOT have increased (fail-fast, no actual call)
        assert_eq!(mock.call_count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_middleware_circuit_breaker_success_resets() {
        let mock = Arc::new(MockProvider::new());
        let cb = Arc::new(CircuitBreaker::new(
            CircuitBreakerConfig::default().with_failure_threshold(5),
        ));
        let config = MiddlewareConfig {
            retry: Arc::new(RetryConfig::no_retries()),
            rate_limiter: Arc::new(crate::rate_limit::UnlimitedLimiter),
            circuit_breaker: Some(cb.clone()),
        };
        let middleware = ProviderMiddleware::from_arc(mock.clone(), config);

        // Success
        let result = middleware.complete(make_request()).await.unwrap();
        assert!(result.content.is_some());
        assert_eq!(cb.state_name(), "CLOSED");
        assert_eq!(cb.failure_count(), 0);
    }

    #[tokio::test]
    async fn test_middleware_config_with_circuit_breaker() {
        let config = MiddlewareConfig::default_unlimited().with_circuit_breaker();
        assert!(config.circuit_breaker.is_some());
        assert_eq!(
            config.circuit_breaker.as_ref().unwrap().state_name(),
            "CLOSED"
        );
    }

    #[tokio::test]
    async fn test_middleware_config_with_circuit_breaker_config() {
        let cb_config = CircuitBreakerConfig::default()
            .with_failure_threshold(10)
            .with_reset_timeout(Duration::from_secs(60));
        let config = MiddlewareConfig::default_unlimited().with_circuit_breaker_config(cb_config);
        assert!(config.circuit_breaker.is_some());
    }

    #[tokio::test]
    async fn test_middleware_from_retry_policy() {
        let policy = RetryPolicy::default()
            .with_max_retries(5)
            .with_jitter(false);
        let config = MiddlewareConfig::from_retry_policy(policy);
        assert_eq!(config.retry.max_retries, 5);
        assert!(config.circuit_breaker.is_some());
    }

    // -------------------------------------------------------------------
    // 503-specific middleware tests
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn test_middleware_retry_on_503() {
        let mock = Arc::new(MockProvider503::new());
        mock.fail_n_times(4); // Fail 4 times, succeed on 5th (uses 503 budget of 5)
        let config = MiddlewareConfig::with_retry(RetryConfig::default());
        let middleware = ProviderMiddleware::from_arc(mock.clone(), config);

        let result = middleware.complete(make_request()).await.unwrap();
        assert_eq!(result.content, Some("response-4".to_string()));
        assert_eq!(mock.call_count.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn test_middleware_circuit_breaker_503_trips() {
        let mock = Arc::new(MockProvider503::new());
        mock.fail_n_times(100); // Always fail
        let cb = Arc::new(CircuitBreaker::new(
            CircuitBreakerConfig::default().with_failure_threshold(3),
        ));
        let config = MiddlewareConfig {
            retry: Arc::new(RetryConfig::no_retries()),
            rate_limiter: Arc::new(crate::rate_limit::UnlimitedLimiter),
            circuit_breaker: Some(cb.clone()),
        };
        let middleware = ProviderMiddleware::from_arc(mock.clone(), config);

        // Fail 3 times to trip the breaker with 503 errors.
        for _ in 0..3 {
            let _ = middleware.complete(make_request()).await;
        }
        assert_eq!(cb.state_name(), "OPEN");
        assert_eq!(mock.call_count.load(Ordering::SeqCst), 3);

        // Next request should be rejected by circuit breaker (fail-fast).
        let result = middleware.complete(make_request()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("OPEN"));
        assert_eq!(mock.call_count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_middleware_circuit_breaker_503_recovery() {
        let mock = Arc::new(MockProvider503::new());
        mock.fail_n_times(3); // Fail 3 times then succeed
        let cb = Arc::new(CircuitBreaker::new(
            CircuitBreakerConfig::default()
                .with_failure_threshold(3)
                .with_success_threshold(1),
        ));
        let config = MiddlewareConfig {
            retry: Arc::new(RetryConfig::no_retries()),
            rate_limiter: Arc::new(crate::rate_limit::UnlimitedLimiter),
            circuit_breaker: Some(cb.clone()),
        };
        let middleware = ProviderMiddleware::from_arc(mock.clone(), config);

        // Fail 3 times to trip the breaker.
        for _ in 0..3 {
            let _ = middleware.complete(make_request()).await;
        }
        assert_eq!(cb.state_name(), "OPEN");

        // Simulate reset_timeout passing.
        let past = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64)
            .saturating_sub(31_000);
        cb.set_opened_at_for_test(past);

        // Next request succeeds → breaker closes.
        let result = middleware.complete(make_request()).await.unwrap();
        assert!(result.content.is_some());
        assert_eq!(cb.state_name(), "CLOSED");
    }
}
