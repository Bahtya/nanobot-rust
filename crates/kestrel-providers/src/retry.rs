//! Retry logic with exponential backoff, jitter, and configurable retry policies.
//!
//! Handles 429 (rate-limited) and transient server errors (5xx) by retrying
//! requests with increasing delays and optional `Retry-After` header respect.
//!
//! ## Retry policy
//!
//! [`RetryPolicy`] controls which errors are retryable and how retries behave:
//! - **429 (rate limit)**: exponential backoff with jitter
//! - **503 (service unavailable)**: dedicated aggressive retry (5 attempts, 30s cap)
//! - **500/502 (server errors)**: retry with backoff
//! - **401/403 (auth errors)**: never retried
//!
//! ## Circuit breaker
//!
//! See [`CircuitBreaker`] for per-provider circuit breaking that trips after
//! consecutive failures and probes in half-open state.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info, warn};

/// Default maximum number of retry attempts.
pub const DEFAULT_MAX_RETRIES: u32 = 3;

/// Default initial backoff duration.
const DEFAULT_BASE_DELAY: Duration = Duration::from_millis(500);

/// Default maximum backoff cap.
const DEFAULT_MAX_DELAY: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// RetryPolicy
// ---------------------------------------------------------------------------

/// Configuration for retry behaviour.
///
/// Controls max retries, backoff timing, jitter, and which HTTP status codes
/// are retryable. The backoff formula is:
///
/// ```text
/// delay = min(base_delay * 2^attempt + jitter, max_delay)
/// ```
///
/// Jitter is a random value in `[0, base_delay * 2^attempt)` to prevent
/// thundering-herd effects when multiple clients retry simultaneously.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of retry attempts (0 = no retries).
    pub max_retries: u32,
    /// Base (initial) delay for exponential backoff.
    pub base_delay: Duration,
    /// Maximum delay cap to prevent unreasonably long waits.
    pub max_delay: Duration,
    /// Whether to add jitter to backoff delays (recommended for 429s).
    pub jitter: bool,
    /// HTTP status codes that should trigger a retry.
    /// Defaults to `[429, 500, 502, 503]`.
    pub retryable_status_codes: Vec<u16>,
    /// Maximum retries for 503 Service Unavailable errors.
    ///
    /// More aggressive than `max_retries` (5 vs 3) because 503 errors are
    /// typically transient service outages that resolve quickly.
    pub max_retries_503: u32,
    /// Maximum delay cap for 503 retries.
    ///
    /// Lower than `max_delay` (30s vs 60s) because 503 outages are typically
    /// short-lived and we want faster retry cycles.
    pub max_delay_503: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            base_delay: DEFAULT_BASE_DELAY,
            max_delay: DEFAULT_MAX_DELAY,
            jitter: true,
            retryable_status_codes: vec![429, 500, 502, 503],
            max_retries_503: 5,
            max_delay_503: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Create a policy with no retries.
    pub fn no_retries() -> Self {
        Self {
            max_retries: 0,
            base_delay: DEFAULT_BASE_DELAY,
            max_delay: DEFAULT_MAX_DELAY,
            jitter: false,
            retryable_status_codes: vec![],
            max_retries_503: 0,
            max_delay_503: Duration::from_secs(30),
        }
    }

    /// Set the maximum number of retries.
    pub fn with_max_retries(mut self, max: u32) -> Self {
        self.max_retries = max;
        self
    }

    /// Set the base delay for backoff.
    pub fn with_base_delay(mut self, delay: Duration) -> Self {
        self.base_delay = delay;
        self
    }

    /// Set the maximum delay cap.
    pub fn with_max_delay(mut self, delay: Duration) -> Self {
        self.max_delay = delay;
        self
    }

    /// Enable or disable jitter.
    pub fn with_jitter(mut self, enabled: bool) -> Self {
        self.jitter = enabled;
        self
    }

    /// Set the retryable HTTP status codes.
    pub fn with_retryable_codes(mut self, codes: Vec<u16>) -> Self {
        self.retryable_status_codes = codes;
        self
    }

    /// Set the maximum retries for 503 errors.
    pub fn with_max_retries_503(mut self, max: u32) -> Self {
        self.max_retries_503 = max;
        self
    }

    /// Set the maximum delay cap for 503 retries.
    pub fn with_max_delay_503(mut self, delay: Duration) -> Self {
        self.max_delay_503 = delay;
        self
    }

    /// Whether a given HTTP status code should trigger a retry.
    pub fn is_retryable(&self, status: u16) -> bool {
        self.retryable_status_codes.contains(&status)
    }
}

// ---------------------------------------------------------------------------
// Legacy RetryConfig (kept for backward compatibility)
// ---------------------------------------------------------------------------

/// Legacy retry configuration — superseded by [`RetryPolicy`].
///
/// Retained for backward compatibility with existing code. New code should
/// prefer `RetryPolicy`.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of retry attempts (0 = no retries).
    pub max_retries: u32,
    /// Initial backoff duration.
    pub initial_backoff: Duration,
    /// Whether to retry on server errors (5xx).
    pub retry_on_server_error: bool,
    /// Maximum retries for 503 errors (default 5, more aggressive than general).
    pub max_retries_503: u32,
    /// Maximum delay cap for 503 retries (default 30s).
    pub max_delay_503: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_MAX_RETRIES,
            initial_backoff: DEFAULT_BASE_DELAY,
            retry_on_server_error: true,
            max_retries_503: 5,
            max_delay_503: Duration::from_secs(30),
        }
    }
}

impl RetryConfig {
    /// Create a config with no retries.
    pub fn no_retries() -> Self {
        Self {
            max_retries: 0,
            initial_backoff: DEFAULT_BASE_DELAY,
            retry_on_server_error: false,
            max_retries_503: 0,
            max_delay_503: Duration::from_secs(30),
        }
    }

    /// Create a config with a custom max retry count.
    pub fn with_max_retries(mut self, max: u32) -> Self {
        self.max_retries = max;
        self
    }
}

impl From<RetryPolicy> for RetryConfig {
    fn from(policy: RetryPolicy) -> Self {
        let retry_on_server_error = policy
            .retryable_status_codes
            .iter()
            .any(|c| (500..600).contains(c));
        Self {
            max_retries: policy.max_retries,
            initial_backoff: policy.base_delay,
            retry_on_server_error,
            max_retries_503: policy.max_retries_503,
            max_delay_503: policy.max_delay_503,
        }
    }
}

// ---------------------------------------------------------------------------
// Circuit breaker
// ---------------------------------------------------------------------------

/// Circuit breaker states.
const CB_CLOSED: u8 = 0;
const CB_OPEN: u8 = 1;
const CB_HALF_OPEN: u8 = 2;

/// Configuration for a circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Number of consecutive failures before the circuit trips (opens).
    pub failure_threshold: u64,
    /// Duration to wait before transitioning from open to half-open.
    pub reset_timeout: Duration,
    /// Number of successful probes in half-open state before closing.
    pub success_threshold: u64,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            reset_timeout: Duration::from_secs(30),
            success_threshold: 2,
        }
    }
}

impl CircuitBreakerConfig {
    /// Create a config with custom failure threshold.
    pub fn with_failure_threshold(mut self, threshold: u64) -> Self {
        self.failure_threshold = threshold;
        self
    }

    /// Create a config with custom reset timeout.
    pub fn with_reset_timeout(mut self, timeout: Duration) -> Self {
        self.reset_timeout = timeout;
        self
    }

    /// Create a config with custom success threshold for half-open → closed.
    pub fn with_success_threshold(mut self, threshold: u64) -> Self {
        self.success_threshold = threshold;
        self
    }
}

/// A request-level circuit breaker.
///
/// Tracks consecutive failures for a single provider. After `failure_threshold`
/// consecutive failures, the circuit opens and all requests are rejected
/// immediately (fail-fast). After `reset_timeout`, the circuit transitions
/// to half-open and allows a limited number of probe requests through.
/// If probes succeed, the circuit closes. If they fail, it re-opens.
///
/// Thread-safe via atomic operations.
pub struct CircuitBreaker {
    /// Current state: CLOSED, OPEN, or HALF_OPEN.
    state: AtomicU8,
    /// Consecutive failure count (reset on success).
    failure_count: AtomicU64,
    /// Consecutive success count in half-open state.
    success_count: AtomicU64,
    /// Timestamp (ms) when the circuit opened — used to calculate reset.
    opened_at_ms: AtomicU64,
    /// Configuration.
    config: CircuitBreakerConfig,
}

impl CircuitBreaker {
    /// Create a new circuit breaker with the given configuration.
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            state: AtomicU8::new(CB_CLOSED),
            failure_count: AtomicU64::new(0),
            success_count: AtomicU64::new(0),
            opened_at_ms: AtomicU64::new(0),
            config,
        }
    }

    /// Create a circuit breaker with default configuration.
    pub fn defaults() -> Self {
        Self::new(CircuitBreakerConfig::default())
    }

    /// Check if a request is allowed to proceed.
    ///
    /// Returns `Ok(())` if the circuit is closed or half-open (probe allowed).
    /// Returns `Err` with a message if the circuit is open (fail-fast).
    pub fn allow_request(&self) -> Result<(), String> {
        let state = self.state.load(Ordering::Acquire);
        match state {
            CB_CLOSED => Ok(()),
            CB_OPEN => {
                // Check if enough time has passed to transition to half-open.
                let opened = self.opened_at_ms.load(Ordering::Acquire);
                let now = current_millis();
                if now >= opened + self.config.reset_timeout.as_millis() as u64 {
                    // Transition to half-open.
                    let _ = self.state.compare_exchange(
                        CB_OPEN,
                        CB_HALF_OPEN,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    self.success_count.store(0, Ordering::Release);
                    debug!("Circuit breaker transitioning to HALF_OPEN");
                    Ok(())
                } else {
                    Err("Circuit breaker is OPEN — requests are failing fast".to_string())
                }
            }
            CB_HALF_OPEN => Ok(()), // Allow probe requests
            _ => Ok(()),
        }
    }

    /// Record a successful request.
    ///
    /// In closed state: resets failure count.
    /// In half-open state: increments success count, closes if threshold met.
    pub fn record_success(&self) {
        self.failure_count.store(0, Ordering::Release);
        let state = self.state.load(Ordering::Acquire);
        if state == CB_HALF_OPEN {
            let successes = self.success_count.fetch_add(1, Ordering::AcqRel) + 1;
            if successes >= self.config.success_threshold {
                self.state.store(CB_CLOSED, Ordering::Release);
                self.success_count.store(0, Ordering::Release);
                info!("Circuit breaker CLOSED — service recovered");
            }
        }
    }

    /// Record a failed request.
    ///
    /// In closed state: increments failure count, opens if threshold met.
    /// In half-open state: re-opens immediately.
    pub fn record_failure(&self) {
        let state = self.state.load(Ordering::Acquire);
        match state {
            CB_CLOSED => {
                let failures = self.failure_count.fetch_add(1, Ordering::AcqRel) + 1;
                if failures >= self.config.failure_threshold {
                    self.state.store(CB_OPEN, Ordering::Release);
                    self.opened_at_ms.store(current_millis(), Ordering::Release);
                    self.failure_count.store(0, Ordering::Release);
                    warn!(
                        "Circuit breaker OPEN — {} consecutive failures detected",
                        failures
                    );
                }
            }
            CB_HALF_OPEN => {
                // Re-open immediately on failure in half-open.
                self.state.store(CB_OPEN, Ordering::Release);
                self.opened_at_ms.store(current_millis(), Ordering::Release);
                self.success_count.store(0, Ordering::Release);
                warn!("Circuit breaker re-OPENED — probe request failed");
            }
            _ => {}
        }
    }

    /// Get the current state name for diagnostics.
    pub fn state_name(&self) -> &'static str {
        match self.state.load(Ordering::Acquire) {
            CB_CLOSED => "CLOSED",
            CB_OPEN => "OPEN",
            CB_HALF_OPEN => "HALF_OPEN",
            _ => "UNKNOWN",
        }
    }

    /// Get the current failure count (for diagnostics/testing).
    pub fn failure_count(&self) -> u64 {
        self.failure_count.load(Ordering::Acquire)
    }

    /// Get the current success count in half-open state.
    pub fn success_count(&self) -> u64 {
        self.success_count.load(Ordering::Acquire)
    }

    /// Force-reset the circuit breaker to closed state.
    pub fn reset(&self) {
        self.state.store(CB_CLOSED, Ordering::Release);
        self.failure_count.store(0, Ordering::Release);
        self.success_count.store(0, Ordering::Release);
    }

    /// Set the `opened_at_ms` timestamp for testing purposes.
    ///
    /// Allows simulating the passage of time to test OPEN → HALF_OPEN transitions
    /// without waiting for the real `reset_timeout`.
    pub fn set_opened_at_for_test(&self, millis: u64) {
        self.opened_at_ms.store(millis, Ordering::Release);
    }
}

impl std::fmt::Debug for CircuitBreaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CircuitBreaker")
            .field("state", &self.state_name())
            .field("failures", &self.failure_count())
            .field("successes", &self.success_count())
            .finish()
    }
}

/// Get current time in milliseconds (monotonic-ish for circuit breaker timing).
fn current_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Backoff helpers
// ---------------------------------------------------------------------------

/// Decide whether a status code is retryable (legacy helper).
pub fn is_retryable_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// Extract the `Retry-After` header value in seconds.
/// Returns `None` if the header is absent or unparseable.
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let val = headers.get("retry-after")?.to_str().ok()?;
    if let Ok(secs) = val.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    None
}

/// Compute the backoff duration for a given attempt (0-indexed).
///
/// Uses exponential backoff: `base * 2^attempt`, capped at `max_delay`.
pub fn backoff_duration(base: Duration, attempt: u32, max_delay: Duration) -> Duration {
    let millis = base.as_millis() as u64;
    let factor = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    let delay_millis = (millis * factor).min(max_delay.as_millis() as u64);
    Duration::from_millis(delay_millis)
}

/// Compute backoff with jitter.
///
/// Adds a random jitter in `[0, base_delay)` to the exponential backoff.
/// Uses a simple xorshift PRNG seeded from the attempt number and current
/// thread id — fast, no external rand dependency needed.
pub fn backoff_with_jitter(base: Duration, attempt: u32, max_delay: Duration) -> Duration {
    let raw = backoff_duration(base, attempt, max_delay);
    // Simple pseudo-random jitter using xorshift.
    // Use attempt + a stack address for entropy (no unstable ThreadId::as_u64).
    let stack_seed = &raw as *const _ as u64;
    let seed = (attempt as u64).wrapping_mul(0x9E3779B97F4A7C15) ^ stack_seed;
    let jitter_frac = (seed >> 32) as u32 as u64; // upper 32 bits as fraction
    let jitter_max = base.as_millis() as u64;
    if jitter_max == 0 {
        return raw;
    }
    let jitter = (jitter_frac % jitter_max) as u64;
    let total = (raw.as_millis() as u64 + jitter).min(max_delay.as_millis() as u64);
    Duration::from_millis(total)
}

// ---------------------------------------------------------------------------
// Retry executor
// ---------------------------------------------------------------------------

/// Execute an async operation with retry and exponential backoff (legacy API).
///
/// Uses [`RetryConfig`] for backward compatibility. New code should use
/// `retry_with_policy` instead.
pub async fn retry_with_backoff<F, Fut, T>(config: &RetryConfig, op: F) -> anyhow::Result<T>
where
    F: Fn(u32) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let max_delay = Duration::from_secs(60);
    let mut attempt = 0u32;
    loop {
        match op(attempt).await {
            Ok(val) => return Ok(val),
            Err(err) => {
                // 503 gets dedicated, more aggressive retry parameters.
                let is_503 = extract_status_code(&err.to_string()) == Some(503);
                let (max_retries_for_err, max_delay_for_err) = if is_503 {
                    (config.max_retries_503, config.max_delay_503)
                } else {
                    (config.max_retries, max_delay)
                };

                let retries_left = max_retries_for_err.saturating_sub(attempt);
                if retries_left == 0 || !is_retryable_err(&err, config.retry_on_server_error) {
                    return Err(err);
                }
                let delay = backoff_duration(config.initial_backoff, attempt, max_delay_for_err);
                warn!(
                    attempt = attempt + 1,
                    max_retries = max_retries_for_err,
                    ?delay,
                    "Retrying after error: {}",
                    err
                );
                sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

/// Execute an async operation with a [`RetryPolicy`] and optional [`CircuitBreaker`].
///
/// Combines:
/// 1. Circuit breaker check (if provided) — fail-fast if open
/// 2. Exponential backoff with jitter
/// 3. Status-code-aware retryability
pub async fn retry_with_policy<F, Fut, T>(
    policy: &RetryPolicy,
    circuit_breaker: Option<&CircuitBreaker>,
    op: F,
) -> anyhow::Result<T>
where
    F: Fn(u32) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut attempt = 0u32;
    loop {
        // Circuit breaker check.
        if let Some(cb) = circuit_breaker {
            if let Err(msg) = cb.allow_request() {
                return Err(anyhow::anyhow!("{}", msg));
            }
        }

        match op(attempt).await {
            Ok(val) => {
                if let Some(cb) = circuit_breaker {
                    cb.record_success();
                }
                return Ok(val);
            }
            Err(err) => {
                // Record failure for circuit breaker.
                if let Some(cb) = circuit_breaker {
                    cb.record_failure();
                }

                // 503 gets dedicated, more aggressive retry parameters.
                let is_503 = extract_status_code(&err.to_string()) == Some(503);
                let (max_retries_for_err, max_delay_for_err) = if is_503 {
                    (policy.max_retries_503, policy.max_delay_503)
                } else {
                    (policy.max_retries, policy.max_delay)
                };

                let retries_left = max_retries_for_err.saturating_sub(attempt);
                if retries_left == 0 || !is_retryable_err_policy(&err, policy) {
                    return Err(err);
                }

                let delay = if policy.jitter {
                    backoff_with_jitter(policy.base_delay, attempt, max_delay_for_err)
                } else {
                    backoff_duration(policy.base_delay, attempt, max_delay_for_err)
                };

                warn!(
                    attempt = attempt + 1,
                    max_retries = max_retries_for_err,
                    ?delay,
                    "Retrying after error: {}",
                    err
                );
                sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

/// Check if an error is retryable using the legacy config.
fn is_retryable_err(err: &anyhow::Error, retry_on_server_error: bool) -> bool {
    let msg = err.to_string();
    if let Some(code) = extract_status_code(&msg) {
        if code == 429 {
            return true;
        }
        if retry_on_server_error && (500..600).contains(&code) {
            return true;
        }
        return false;
    }
    msg.contains("connection") || msg.contains("timeout") || msg.contains("refused")
}

/// Check if an error is retryable using a RetryPolicy.
fn is_retryable_err_policy(err: &anyhow::Error, policy: &RetryPolicy) -> bool {
    let msg = err.to_string();
    if let Some(code) = extract_status_code(&msg) {
        return policy.is_retryable(code);
    }
    // Connection-level errors are always retryable.
    msg.contains("connection") || msg.contains("timeout") || msg.contains("refused")
}

/// Extract HTTP status code from an error message like "API error (429): ..."
/// or "API error (429 Too Many Requests): ..."
pub fn extract_status_code(msg: &str) -> Option<u16> {
    let start = msg.find('(')?;
    let end = msg.find(')').filter(|e| *e > start)?;
    let inner = &msg[start + 1..end];
    let code_str = inner.split_whitespace().next()?;
    code_str.parse().ok()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // -----------------------------------------------------------------------
    // RetryPolicy
    // -----------------------------------------------------------------------

    #[test]
    fn test_retry_policy_default() {
        let p = RetryPolicy::default();
        assert_eq!(p.max_retries, 3);
        assert_eq!(p.base_delay, Duration::from_millis(500));
        assert_eq!(p.max_delay, Duration::from_secs(60));
        assert!(p.jitter);
        assert!(p.is_retryable(429));
        assert!(p.is_retryable(500));
        assert!(p.is_retryable(502));
        assert!(p.is_retryable(503));
        assert!(!p.is_retryable(400));
        assert!(!p.is_retryable(401));
        assert!(!p.is_retryable(403));
        assert!(!p.is_retryable(404));
        assert_eq!(p.max_retries_503, 5);
        assert_eq!(p.max_delay_503, Duration::from_secs(30));
    }

    #[test]
    fn test_retry_policy_no_retries() {
        let p = RetryPolicy::no_retries();
        assert_eq!(p.max_retries, 0);
        assert!(!p.is_retryable(429));
    }

    #[test]
    fn test_retry_policy_custom_codes() {
        let p = RetryPolicy::default().with_retryable_codes(vec![429, 503]);
        assert!(p.is_retryable(429));
        assert!(p.is_retryable(503));
        assert!(!p.is_retryable(500));
        assert!(!p.is_retryable(502));
    }

    #[test]
    fn test_retry_policy_builder() {
        let p = RetryPolicy::default()
            .with_max_retries(5)
            .with_base_delay(Duration::from_millis(200))
            .with_max_delay(Duration::from_secs(30))
            .with_jitter(false);
        assert_eq!(p.max_retries, 5);
        assert_eq!(p.base_delay, Duration::from_millis(200));
        assert_eq!(p.max_delay, Duration::from_secs(30));
        assert!(!p.jitter);
    }

    // -----------------------------------------------------------------------
    // RetryConfig (legacy)
    // -----------------------------------------------------------------------

    #[test]
    fn test_retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 3);
        assert!(config.retry_on_server_error);
        assert_eq!(config.max_retries_503, 5);
        assert_eq!(config.max_delay_503, Duration::from_secs(30));
    }

    #[test]
    fn test_retry_config_no_retries() {
        let config = RetryConfig::no_retries();
        assert_eq!(config.max_retries, 0);
        assert!(!config.retry_on_server_error);
    }

    #[test]
    fn test_retry_config_from_policy() {
        let policy = RetryPolicy::default().with_max_retries(5);
        let config = RetryConfig::from(policy);
        assert_eq!(config.max_retries, 5);
    }

    // -----------------------------------------------------------------------
    // is_retryable_status (legacy)
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_retryable_status() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(502));
        assert!(is_retryable_status(503));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(403));
        assert!(!is_retryable_status(404));
    }

    // -----------------------------------------------------------------------
    // Backoff
    // -----------------------------------------------------------------------

    #[test]
    fn test_backoff_duration() {
        let base = Duration::from_secs(1);
        let max = Duration::from_secs(60);
        assert_eq!(backoff_duration(base, 0, max), Duration::from_secs(1));
        assert_eq!(backoff_duration(base, 1, max), Duration::from_secs(2));
        assert_eq!(backoff_duration(base, 2, max), Duration::from_secs(4));
        assert_eq!(backoff_duration(base, 3, max), Duration::from_secs(8));
        // Capped at max
        assert_eq!(backoff_duration(base, 20, max), max);
    }

    #[test]
    fn test_backoff_with_jitter_produces_reasonable_range() {
        let base = Duration::from_millis(100);
        let max = Duration::from_secs(60);
        for attempt in 0..5u32 {
            let delay = backoff_with_jitter(base, attempt, max);
            let raw = backoff_duration(base, attempt, max);
            // Jitter adds [0, base) so delay should be >= raw and < raw + base
            assert!(
                delay >= raw,
                "jittered delay {:?} should be >= raw {:?}",
                delay,
                raw
            );
            assert!(
                delay < raw + base,
                "jittered delay {:?} should be < raw + base {:?}",
                delay,
                raw + base
            );
        }
    }

    // -----------------------------------------------------------------------
    // parse_retry_after
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_retry_after_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "30".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(30)));
    }

    #[test]
    fn test_parse_retry_after_missing() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn test_parse_retry_after_invalid() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "not-a-number".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), None);
    }

    // -----------------------------------------------------------------------
    // extract_status_code
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_status_code() {
        assert_eq!(
            extract_status_code("API error (429): rate limited"),
            Some(429)
        );
        assert_eq!(
            extract_status_code("Anthropic API error (503): unavailable"),
            Some(503)
        );
        assert_eq!(
            extract_status_code("API error (401): unauthorized"),
            Some(401)
        );
        assert_eq!(
            extract_status_code("API error (429 Too Many Requests): rate limited"),
            Some(429)
        );
        assert_eq!(
            extract_status_code("Anthropic API error (503 Service Unavailable): "),
            Some(503)
        );
        assert_eq!(extract_status_code("Some other error"), None);
    }

    // -----------------------------------------------------------------------
    // CircuitBreakerConfig
    // -----------------------------------------------------------------------

    #[test]
    fn test_circuit_breaker_config_default() {
        let cfg = CircuitBreakerConfig::default();
        assert_eq!(cfg.failure_threshold, 5);
        assert_eq!(cfg.reset_timeout, Duration::from_secs(30));
        assert_eq!(cfg.success_threshold, 2);
    }

    #[test]
    fn test_circuit_breaker_config_builder() {
        let cfg = CircuitBreakerConfig::default()
            .with_failure_threshold(3)
            .with_reset_timeout(Duration::from_secs(10))
            .with_success_threshold(1);
        assert_eq!(cfg.failure_threshold, 3);
        assert_eq!(cfg.reset_timeout, Duration::from_secs(10));
        assert_eq!(cfg.success_threshold, 1);
    }

    // -----------------------------------------------------------------------
    // CircuitBreaker
    // -----------------------------------------------------------------------

    #[test]
    fn test_circuit_breaker_starts_closed() {
        let cb = CircuitBreaker::defaults();
        assert_eq!(cb.state_name(), "CLOSED");
        assert!(cb.allow_request().is_ok());
    }

    #[test]
    fn test_circuit_breaker_trips_after_threshold() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default().with_failure_threshold(3));
        assert_eq!(cb.state_name(), "CLOSED");

        cb.record_failure();
        assert_eq!(cb.state_name(), "CLOSED");
        cb.record_failure();
        assert_eq!(cb.state_name(), "CLOSED");
        cb.record_failure();
        // 3 failures = threshold → open
        assert_eq!(cb.state_name(), "OPEN");
        assert!(cb.allow_request().is_err());
    }

    #[test]
    fn test_circuit_breaker_success_resets_failures() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default().with_failure_threshold(3));
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.failure_count(), 2);

        cb.record_success();
        assert_eq!(cb.failure_count(), 0);
        assert_eq!(cb.state_name(), "CLOSED");
    }

    #[test]
    fn test_circuit_breaker_half_open_to_closed() {
        let cb = CircuitBreaker::new(
            CircuitBreakerConfig::default()
                .with_failure_threshold(1)
                .with_success_threshold(2),
        );

        // Trip the breaker.
        cb.record_failure();
        assert_eq!(cb.state_name(), "OPEN");

        // Manually set opened_at to past so reset_timeout has passed.
        let past = current_millis().saturating_sub(31_000);
        cb.opened_at_ms.store(past, Ordering::Release);

        // Should transition to half-open.
        assert!(cb.allow_request().is_ok());
        assert_eq!(cb.state_name(), "HALF_OPEN");

        // Two successes in half-open → close.
        cb.record_success();
        assert_eq!(cb.state_name(), "HALF_OPEN"); // Need 2 successes
        cb.record_success();
        assert_eq!(cb.state_name(), "CLOSED");
    }

    #[test]
    fn test_circuit_breaker_half_open_reopens_on_failure() {
        let cb = CircuitBreaker::new(
            CircuitBreakerConfig::default()
                .with_failure_threshold(1)
                .with_success_threshold(2),
        );

        cb.record_failure();
        assert_eq!(cb.state_name(), "OPEN");

        // Transition to half-open.
        let past = current_millis().saturating_sub(31_000);
        cb.opened_at_ms.store(past, Ordering::Release);
        cb.allow_request().unwrap();
        assert_eq!(cb.state_name(), "HALF_OPEN");

        // Failure in half-open → re-open.
        cb.record_failure();
        assert_eq!(cb.state_name(), "OPEN");
    }

    #[test]
    fn test_circuit_breaker_reset() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default().with_failure_threshold(1));
        cb.record_failure();
        assert_eq!(cb.state_name(), "OPEN");

        cb.reset();
        assert_eq!(cb.state_name(), "CLOSED");
        assert_eq!(cb.failure_count(), 0);
        assert!(cb.allow_request().is_ok());
    }

    #[test]
    fn test_circuit_breaker_debug() {
        let cb = CircuitBreaker::defaults();
        let s = format!("{:?}", cb);
        assert!(s.contains("CLOSED"));
    }

    // -----------------------------------------------------------------------
    // retry_with_backoff (legacy)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_retry_succeeds_on_first_try() {
        let config = RetryConfig::default();
        let result = retry_with_backoff(&config, |_attempt| async { Ok::<_, anyhow::Error>(42) })
            .await
            .unwrap();
        assert_eq!(result, 42);
    }

    #[tokio::test]
    async fn test_retry_succeeds_after_retries() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let config = RetryConfig::default().with_max_retries(3);

        let calls_clone = calls.clone();
        let result = retry_with_backoff(&config, move |_attempt| {
            let calls = calls_clone.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 3 {
                    Err(anyhow::anyhow!("API error (429): rate limited"))
                } else {
                    Ok::<_, anyhow::Error>("success")
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(result, "success");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_exhausted() {
        let config = RetryConfig::default().with_max_retries(2);
        let result = retry_with_backoff(&config, |_attempt| async {
            Err::<(), _>(anyhow::anyhow!("API error (429): rate limited"))
        })
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("429"));
    }

    #[tokio::test]
    async fn test_retry_non_retryable_error() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let config = RetryConfig::default();

        let calls_clone = calls.clone();
        let result = retry_with_backoff(&config, move |_attempt| {
            let calls = calls_clone.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(anyhow::anyhow!("API error (401): unauthorized"))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1); // No retry on 401
    }

    // -----------------------------------------------------------------------
    // retry_with_policy (new API)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_retry_policy_succeeds_on_first_try() {
        let policy = RetryPolicy::default();
        let result = retry_with_policy(&policy, None, |_attempt| async {
            Ok::<_, anyhow::Error>("ok")
        })
        .await
        .unwrap();
        assert_eq!(result, "ok");
    }

    #[tokio::test]
    async fn test_retry_policy_retries_on_429() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let policy = RetryPolicy::default()
            .with_max_retries(3)
            .with_jitter(false);

        let calls_clone = calls.clone();
        let result = retry_with_policy(&policy, None, move |_attempt| {
            let calls = calls_clone.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 3 {
                    Err(anyhow::anyhow!("API error (429): rate limited"))
                } else {
                    Ok::<_, anyhow::Error>("ok")
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(result, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_policy_retries_on_503() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let policy = RetryPolicy::default()
            .with_max_retries(2)
            .with_jitter(false);

        let calls_clone = calls.clone();
        let result = retry_with_policy(&policy, None, move |_attempt| {
            let calls = calls_clone.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 2 {
                    Err(anyhow::anyhow!("API error (503): service unavailable"))
                } else {
                    Ok::<_, anyhow::Error>("ok")
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(result, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_retry_policy_no_retry_on_401() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let policy = RetryPolicy::default();

        let calls_clone = calls.clone();
        let result = retry_with_policy(&policy, None, move |_attempt| {
            let calls = calls_clone.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(anyhow::anyhow!("API error (401): unauthorized"))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_retry_policy_no_retry_on_403() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let policy = RetryPolicy::default();

        let calls_clone = calls.clone();
        let result = retry_with_policy(&policy, None, move |_attempt| {
            let calls = calls_clone.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(anyhow::anyhow!("API error (403): forbidden"))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_retry_policy_retries_on_connection_error() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let policy = RetryPolicy::default()
            .with_max_retries(2)
            .with_jitter(false);

        let calls_clone = calls.clone();
        let result = retry_with_policy(&policy, None, move |_attempt| {
            let calls = calls_clone.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 2 {
                    Err(anyhow::anyhow!("connection refused"))
                } else {
                    Ok::<_, anyhow::Error>("ok")
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(result, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_retry_policy_exhausted() {
        let policy = RetryPolicy::default()
            .with_max_retries(1)
            .with_jitter(false);
        let result = retry_with_policy(&policy, None, |_attempt| async {
            Err::<(), _>(anyhow::anyhow!("API error (500): internal error"))
        })
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_retry_policy_with_circuit_breaker() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let cb = CircuitBreaker::new(
            CircuitBreakerConfig::default()
                .with_failure_threshold(3)
                .with_success_threshold(1),
        );
        let policy = RetryPolicy::default()
            .with_max_retries(0)
            .with_jitter(false);

        let calls_clone = calls.clone();
        // Fail 3 times to trip the breaker (no retries so each call = 1 attempt).
        for _ in 0..3 {
            let _ = retry_with_policy(&policy, Some(&cb), {
                let calls = calls_clone.clone();
                move |_attempt| {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Err::<(), _>(anyhow::anyhow!("API error (500): error"))
                    }
                }
            })
            .await;
        }

        assert_eq!(cb.state_name(), "OPEN");
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // Next request should be rejected by circuit breaker.
        let result = retry_with_policy(&policy, Some(&cb), |_attempt| async {
            Ok::<_, anyhow::Error>("should not reach")
        })
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("OPEN"));
        // Should not have incremented call count.
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_retry_policy_success_records_to_circuit_breaker() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default().with_failure_threshold(1));
        let policy = RetryPolicy::default()
            .with_max_retries(0)
            .with_jitter(false);

        // Succeed.
        let result = retry_with_policy(&policy, Some(&cb), |_attempt| async {
            Ok::<_, anyhow::Error>("ok")
        })
        .await
        .unwrap();
        assert_eq!(result, "ok");
        assert_eq!(cb.state_name(), "CLOSED");
        assert_eq!(cb.failure_count(), 0);
    }

    // -----------------------------------------------------------------------
    // 503-specific retry tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_retry_policy_503_defaults() {
        let p = RetryPolicy::default();
        assert_eq!(p.max_retries_503, 5);
        assert_eq!(p.max_delay_503, Duration::from_secs(30));
    }

    #[test]
    fn test_retry_policy_503_builder() {
        let p = RetryPolicy::default()
            .with_max_retries_503(10)
            .with_max_delay_503(Duration::from_secs(15));
        assert_eq!(p.max_retries_503, 10);
        assert_eq!(p.max_delay_503, Duration::from_secs(15));
    }

    #[test]
    fn test_backoff_503_capped_at_30s() {
        let base = Duration::from_secs(1);
        let max_503 = Duration::from_secs(30);
        // At attempt 5, raw backoff would be 32s, but should be capped at 30s.
        assert_eq!(backoff_duration(base, 5, max_503), Duration::from_secs(30));
        // At attempt 4, raw backoff is 16s, under the cap.
        assert_eq!(backoff_duration(base, 4, max_503), Duration::from_secs(16));
    }

    #[tokio::test]
    async fn test_retry_policy_503_uses_dedicated_retries() {
        // Default policy: max_retries=3, max_retries_503=5.
        // Fail 4 times with 503 then succeed — uses the 503 budget (would fail with only 3 retries).
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let policy = RetryPolicy::default().with_jitter(false);

        let calls_clone = calls.clone();
        let result = retry_with_policy(&policy, None, move |_attempt| {
            let calls = calls_clone.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 5 {
                    Err(anyhow::anyhow!("API error (503): service unavailable"))
                } else {
                    Ok::<_, anyhow::Error>("ok")
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(result, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn test_retry_policy_503_exhausted_at_5_retries() {
        // max_retries_503=5 means initial attempt + 5 retries = 6 total calls.
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let policy = RetryPolicy::default().with_jitter(false);

        let calls_clone = calls.clone();
        let result = retry_with_policy(&policy, None, move |_attempt| {
            let calls = calls_clone.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(anyhow::anyhow!("API error (503): service unavailable"))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 6);
    }

    #[tokio::test]
    async fn test_retry_policy_503_consecutive_recovery() {
        // Simulate 5 consecutive 503s then recovery on the 6th call.
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let policy = RetryPolicy::default()
            .with_max_retries_503(6)
            .with_jitter(false);

        let calls_clone = calls.clone();
        let result = retry_with_policy(&policy, None, move |_attempt| {
            let calls = calls_clone.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 6 {
                    Err(anyhow::anyhow!("API error (503): service unavailable"))
                } else {
                    Ok::<_, anyhow::Error>("recovered")
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(result, "recovered");
        assert_eq!(calls.load(Ordering::SeqCst), 6);
    }

    #[tokio::test]
    async fn test_retry_with_backoff_503_recovery() {
        // Legacy path: RetryConfig with 503-specific params.
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let config = RetryConfig::default(); // max_retries_503=5

        let calls_clone = calls.clone();
        let result = retry_with_backoff(&config, move |_attempt| {
            let calls = calls_clone.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 5 {
                    Err(anyhow::anyhow!("API error (503): service unavailable"))
                } else {
                    Ok::<_, anyhow::Error>("ok")
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(result, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn test_retry_with_backoff_503_exhausted() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let config = RetryConfig::default();

        let calls_clone = calls.clone();
        let result = retry_with_backoff(&config, move |_attempt| {
            let calls = calls_clone.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(anyhow::anyhow!("API error (503): service unavailable"))
            }
        })
        .await;
        assert!(result.is_err());
        // max_retries_503=5 → initial + 5 retries = 6 total calls
        assert_eq!(calls.load(Ordering::SeqCst), 6);
    }

    #[tokio::test]
    async fn test_circuit_breaker_503_opens_and_recovers() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let cb = CircuitBreaker::new(
            CircuitBreakerConfig::default()
                .with_failure_threshold(3)
                .with_success_threshold(1),
        );
        let policy = RetryPolicy::default()
            .with_max_retries(0)
            .with_jitter(false);

        let calls_clone = calls.clone();

        // Fail 3 times with 503 to trip the breaker (no retries, each call = 1 attempt).
        for _ in 0..3 {
            let _ = retry_with_policy(&policy, Some(&cb), {
                let calls = calls_clone.clone();
                move |_attempt| {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Err::<(), _>(anyhow::anyhow!("API error (503): service unavailable"))
                    }
                }
            })
            .await;
        }

        assert_eq!(cb.state_name(), "OPEN");
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // Next request should be rejected by circuit breaker (fail-fast).
        let result = retry_with_policy(&policy, Some(&cb), |_attempt| async {
            Ok::<_, anyhow::Error>("should not reach")
        })
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("OPEN"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // Simulate reset_timeout passing.
        let past = current_millis().saturating_sub(31_000);
        cb.opened_at_ms.store(past, Ordering::Release);

        // Half-open: probe request succeeds → breaker closes.
        let result = retry_with_policy(&policy, Some(&cb), {
            let calls = calls_clone.clone();
            move |_attempt| {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, anyhow::Error>("recovered")
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(result, "recovered");
        assert_eq!(cb.state_name(), "CLOSED");
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn test_retry_policy_503_does_not_affect_429_budget() {
        // Verify that 429 still uses the general max_retries, not the 503 budget.
        use std::sync::atomic::{AtomicU32, Ordering};
        let calls = Arc::new(AtomicU32::new(0));
        let policy = RetryPolicy::default().with_jitter(false);
        // max_retries=3, max_retries_503=5.

        let calls_clone = calls.clone();
        let result = retry_with_policy(&policy, None, move |_attempt| {
            let calls = calls_clone.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(anyhow::anyhow!("API error (429): rate limited"))
            }
        })
        .await;
        assert!(result.is_err());
        // max_retries=3 → initial + 3 retries = 4 total calls
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }
}
