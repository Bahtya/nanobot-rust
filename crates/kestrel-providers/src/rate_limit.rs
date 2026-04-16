//! Rate limiting for provider API calls.
//!
//! Provides a [`RateLimiter`] trait and a token-bucket implementation
//! ([`TokenBucket`]) that providers use to throttle outgoing requests.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::Instant;
use tracing::trace;

/// Trait for rate-limiting provider API calls.
///
/// Implementations track how many requests are allowed within a time window
/// and block when the quota is exhausted.
#[async_trait::async_trait]
pub trait RateLimiter: Send + Sync {
    /// Acquire a single permit, waiting if necessary until capacity is available.
    async fn acquire(&self);

    /// Try to acquire a permit without waiting.
    /// Returns `true` if a permit was acquired, `false` if the bucket is empty.
    fn try_acquire(&self) -> bool;

    /// Current number of available tokens.
    fn available(&self) -> u64;
}

/// Token-bucket rate limiter.
///
/// Tokens are refilled at a steady rate up to a maximum capacity.
/// Each API call consumes one token. When the bucket is empty, callers
/// must wait until new tokens become available.
///
/// # Example
///
/// ```ignore
/// use kestrel_providers::rate_limit::TokenBucket;
/// use std::time::Duration;
///
/// let bucket = TokenBucket::new(10, Duration::from_secs(1));
/// // Can make 10 requests immediately, then 1/sec
/// ```
pub struct TokenBucket {
    /// Maximum tokens the bucket can hold.
    capacity: u64,
    /// Current number of tokens (atomic for lock-free reads).
    tokens: AtomicU64,
    /// Number of tokens added per refill interval.
    refill_amount: u64,
    /// Duration between refills.
    refill_interval: Duration,
    /// Time of last refill (stored as millis since epoch for atomic access).
    last_refill_ms: AtomicU64,
}

impl TokenBucket {
    /// Create a new token bucket.
    ///
    /// - `capacity`: Maximum tokens (e.g. 60 for 60 requests per minute).
    /// - `refill_interval`: How often `refill_amount` tokens are added.
    ///
    /// The bucket starts full. Refill rate is 1 token per
    /// `refill_interval / capacity` on average.
    pub fn new(capacity: u64, refill_interval: Duration) -> Self {
        let now_ms = epoch_millis();
        Self {
            capacity,
            tokens: AtomicU64::new(capacity),
            refill_amount: 1,
            refill_interval,
            last_refill_ms: AtomicU64::new(now_ms),
        }
    }

    /// Create a token bucket with custom refill parameters.
    ///
    /// - `capacity`: Maximum tokens.
    /// - `refill_amount`: Tokens added per refill.
    /// - `refill_interval`: Duration between refills.
    pub fn with_refill(capacity: u64, refill_amount: u64, refill_interval: Duration) -> Self {
        let now_ms = epoch_millis();
        Self {
            capacity,
            tokens: AtomicU64::new(capacity),
            refill_amount,
            refill_interval,
            last_refill_ms: AtomicU64::new(now_ms),
        }
    }

    /// An unlimited rate limiter that never blocks.
    pub fn unlimited() -> Self {
        Self {
            capacity: u64::MAX,
            tokens: AtomicU64::new(u64::MAX),
            refill_amount: 1,
            refill_interval: Duration::from_secs(1),
            last_refill_ms: AtomicU64::new(epoch_millis()),
        }
    }

    /// Refill tokens based on elapsed time. Returns the number of tokens
    /// available after refill.
    fn refill(&self) -> u64 {
        let now_ms = epoch_millis();
        let last_ms = self.last_refill_ms.load(Ordering::Acquire);

        let elapsed = now_ms.saturating_sub(last_ms);
        let interval_ms = self.refill_interval.as_millis() as u64;

        if elapsed < interval_ms {
            return self.tokens.load(Ordering::Acquire);
        }

        // Calculate how many refill periods have passed
        let periods = elapsed / interval_ms;
        let tokens_to_add = periods * self.refill_amount;

        // Try to update last_refill_ms (CAS to avoid races)
        let new_last_ms = last_ms + periods * interval_ms;
        let _ = self.last_refill_ms.compare_exchange(
            last_ms,
            new_last_ms,
            Ordering::AcqRel,
            Ordering::Acquire,
        );

        // Add tokens up to capacity
        loop {
            let current = self.tokens.load(Ordering::Acquire);
            let new_tokens = (current + tokens_to_add).min(self.capacity);
            match self.tokens.compare_exchange(
                current,
                new_tokens,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return new_tokens,
                Err(_) => continue, // Another thread refilled, retry
            }
        }
    }

    /// Consume one token. Returns true if a token was consumed.
    fn consume_one(&self) -> bool {
        loop {
            let current = self.tokens.load(Ordering::Acquire);
            if current == 0 {
                return false;
            }
            match self.tokens.compare_exchange(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(_) => continue,
            }
        }
    }
}

#[async_trait::async_trait]
impl RateLimiter for TokenBucket {
    async fn acquire(&self) {
        loop {
            self.refill();
            if self.consume_one() {
                trace!(
                    available = self.tokens.load(Ordering::Relaxed),
                    "Rate limit token acquired"
                );
                return;
            }
            // Wait for a fraction of the refill interval before retrying
            tokio::time::sleep(self.refill_interval / self.refill_amount.max(1) as u32).await;
        }
    }

    fn try_acquire(&self) -> bool {
        self.refill();
        self.consume_one()
    }

    fn available(&self) -> u64 {
        self.refill()
    }
}

/// An unlimited rate limiter that never blocks.
/// Useful for providers or tests that don't need rate limiting.
pub struct UnlimitedLimiter;

#[async_trait::async_trait]
impl RateLimiter for UnlimitedLimiter {
    async fn acquire(&self) {
        // No-op: unlimited
    }

    fn try_acquire(&self) -> bool {
        true
    }

    fn available(&self) -> u64 {
        u64::MAX
    }
}

/// Get current time as milliseconds since an arbitrary epoch (uses Instant).
fn epoch_millis() -> u64 {
    Instant::now().elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_bucket_new_starts_full() {
        let bucket = TokenBucket::new(10, Duration::from_secs(1));
        assert_eq!(bucket.available(), 10);
    }

    #[test]
    fn test_token_bucket_try_acquire() {
        let bucket = TokenBucket::new(3, Duration::from_secs(60));
        assert!(bucket.try_acquire());
        assert!(bucket.try_acquire());
        assert!(bucket.try_acquire());
        assert!(!bucket.try_acquire()); // exhausted
        assert_eq!(bucket.available(), 0);
    }

    #[test]
    fn test_token_bucket_unlimited() {
        let limiter = UnlimitedLimiter;
        assert!(limiter.try_acquire());
        assert!(limiter.try_acquire());
        assert_eq!(limiter.available(), u64::MAX);
    }

    #[tokio::test]
    async fn test_unlimited_limiter_acquire() {
        let limiter = UnlimitedLimiter;
        limiter.acquire().await; // Should return immediately
    }

    #[test]
    fn test_token_bucket_with_refill() {
        let bucket = TokenBucket::with_refill(5, 2, Duration::from_secs(1));
        assert_eq!(bucket.available(), 5);
        assert!(bucket.try_acquire());
        assert!(bucket.try_acquire());
        assert_eq!(bucket.available(), 3);
    }

    #[test]
    fn test_token_bucket_concurrency_safe() {
        let bucket = TokenBucket::new(100, Duration::from_secs(1));
        let bucket = std::sync::Arc::new(bucket);

        let mut handles = Vec::new();
        for _ in 0..10 {
            let b = bucket.clone();
            handles.push(std::thread::spawn(move || {
                let mut acquired = 0;
                for _ in 0..10 {
                    if b.try_acquire() {
                        acquired += 1;
                    }
                }
                acquired
            }));
        }

        let total: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(total, 100); // Exactly 100 tokens consumed
        assert_eq!(bucket.available(), 0);
    }
}
