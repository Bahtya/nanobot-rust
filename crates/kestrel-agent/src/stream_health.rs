//! Stream health monitor.
//!
//! Tracks token flow over a sliding window and classifies the SSE stream
//! as Healthy / Stale / Dead.  Thinking blocks receive more lenient thresholds.

use std::time::{Duration, Instant};

// ── Types ──────────────────────────────────────────────────────

/// Health state of an SSE stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthState {
    /// Content chunks are being received.
    Healthy,
    /// No content for a while but the connection may still be alive.
    Stale,
    /// No data at all beyond the absolute ceiling — must reconnect.
    Dead,
}

struct TokenRecord {
    at: Instant,
    token_count: usize,
}

// ── Monitor ────────────────────────────────────────────────────

/// Monitors the health of an SSE stream by tracking token flow.
pub struct StreamHealthMonitor {
    /// Instant when the last chunk of any type was received.
    last_chunk_at: Instant,
    /// Instant when the last chunk carrying actual content was received.
    last_content_at: Instant,
    /// Total content chunks received.
    chunks_received: usize,
    /// Total estimated tokens received.
    tokens_received: usize,
    /// Sliding window of token records for rate calculation.
    token_window: Vec<TokenRecord>,
    /// Current health state.
    state: HealthState,

    // Thresholds
    stale_threshold: Duration,
    absolute_max: Duration,

    // Thinking block handling
    in_thinking_block: bool,
    thinking_stale_multiplier: u32,

    // Rate window
    window_duration: Duration,
}

impl StreamHealthMonitor {
    /// Create a new monitor with the given thresholds.
    ///
    /// * `stale_threshold` — time without content before declaring Stale
    ///   (typically `ResolvedTimeouts.idle_timeout`).
    /// * `absolute_max` — hard ceiling beyond which the stream is Dead.
    pub fn new(stale_threshold: Duration, absolute_max: Duration) -> Self {
        let now = Instant::now();
        Self {
            last_chunk_at: now,
            last_content_at: now,
            chunks_received: 0,
            tokens_received: 0,
            token_window: Vec::with_capacity(64),
            state: HealthState::Healthy,
            stale_threshold,
            absolute_max,
            in_thinking_block: false,
            thinking_stale_multiplier: 3,
            window_duration: Duration::from_secs(30),
        }
    }

    /// Record a received chunk.
    ///
    /// * `has_content` — true if the chunk carries text/reasoning delta.
    /// * `token_count` — estimated token count of the delta.
    pub fn record_chunk(&mut self, has_content: bool, token_count: usize) {
        let now = Instant::now();
        self.last_chunk_at = now;
        self.chunks_received += 1;

        if has_content {
            self.last_content_at = now;
            self.tokens_received += token_count;
            self.token_window.push(TokenRecord {
                at: now,
                token_count,
            });
            self.prune_window();
            // Recovery: Stale/Dead → Healthy
            if self.state != HealthState::Healthy {
                self.state = HealthState::Healthy;
            }
        }
    }

    /// Mark whether we are currently inside a thinking/reasoning block.
    pub fn set_thinking(&mut self, in_thinking: bool) {
        self.in_thinking_block = in_thinking;
    }

    /// Check and update health state.  Call this periodically.
    pub fn check_health(&mut self) -> HealthState {
        let now = Instant::now();
        let effective_stale = if self.in_thinking_block {
            self.stale_threshold * self.thinking_stale_multiplier
        } else {
            self.stale_threshold
        };

        let since_content = now.duration_since(self.last_content_at);
        let since_any = now.duration_since(self.last_chunk_at);

        if since_any >= self.absolute_max {
            self.state = HealthState::Dead;
        } else if since_content >= effective_stale {
            self.state = HealthState::Stale;
        }
        // Note: Healthy transitions only happen in record_chunk

        self.state.clone()
    }

    /// Compute token rate (tokens / second) over the sliding window.
    pub fn token_rate(&self) -> f64 {
        if self.token_window.is_empty() {
            return 0.0;
        }
        let now = Instant::now();
        let window_start = now - self.window_duration;
        let in_window: Vec<&TokenRecord> = self
            .token_window
            .iter()
            .filter(|r| r.at >= window_start)
            .collect();

        if in_window.is_empty() {
            return 0.0;
        }

        let total_tokens: usize = in_window.iter().map(|r| r.token_count).sum();
        let duration = now.duration_since(in_window[0].at);
        let secs = duration.as_secs_f64().max(0.001);
        total_tokens as f64 / secs
    }

    /// Time since the last content chunk.
    pub fn time_since_content(&self) -> Duration {
        Instant::now().duration_since(self.last_content_at)
    }

    /// Time since the last chunk of any type.
    pub fn time_since_any(&self) -> Duration {
        Instant::now().duration_since(self.last_chunk_at)
    }

    /// Total content chunks received.
    pub fn content_chunks(&self) -> usize {
        self.token_window.len()
    }

    /// Total estimated tokens received.
    pub fn total_tokens(&self) -> usize {
        self.tokens_received
    }

    /// Current health state (no re-check).
    pub fn state(&self) -> &HealthState {
        &self.state
    }

    fn prune_window(&mut self) {
        let cutoff = Instant::now() - self.window_duration;
        self.token_window.retain(|r| r.at >= cutoff);
    }
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_is_healthy() {
        let m = StreamHealthMonitor::new(Duration::from_secs(60), Duration::from_secs(600));
        assert_eq!(*m.state(), HealthState::Healthy);
    }

    #[test]
    fn record_content_chunk_stays_healthy() {
        let mut m = StreamHealthMonitor::new(Duration::from_secs(60), Duration::from_secs(600));
        m.record_chunk(true, 10);
        assert_eq!(*m.state(), HealthState::Healthy);
        assert_eq!(m.total_tokens(), 10);
    }

    #[test]
    fn record_non_content_chunk_updates_last_chunk() {
        let mut m = StreamHealthMonitor::new(Duration::from_secs(60), Duration::from_secs(600));
        m.record_chunk(false, 0);
        assert_eq!(m.content_chunks(), 0);
    }

    #[test]
    fn stale_after_threshold() {
        let mut m = StreamHealthMonitor::new(Duration::from_millis(50), Duration::from_secs(10));
        m.record_chunk(true, 5);
        // Wait past stale threshold
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(m.check_health(), HealthState::Stale);
    }

    #[test]
    fn dead_after_absolute_max() {
        let mut m = StreamHealthMonitor::new(Duration::from_millis(30), Duration::from_millis(80));
        m.record_chunk(false, 0);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(m.check_health(), HealthState::Dead);
    }

    #[test]
    fn recovery_from_stale() {
        let mut m = StreamHealthMonitor::new(Duration::from_millis(50), Duration::from_secs(10));
        m.record_chunk(true, 5);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(m.check_health(), HealthState::Stale);
        // New content chunk → Healthy
        m.record_chunk(true, 3);
        assert_eq!(*m.state(), HealthState::Healthy);
    }

    #[test]
    fn thinking_block_extends_stale() {
        let mut m = StreamHealthMonitor::new(Duration::from_millis(50), Duration::from_secs(10));
        m.set_thinking(true);
        m.record_chunk(true, 5);
        // 80ms > 50ms base but < 150ms (50ms * 3 multiplier)
        std::thread::sleep(Duration::from_millis(80));
        // Should still be healthy (threshold is 150ms)
        assert_eq!(m.check_health(), HealthState::Healthy);
    }

    #[test]
    fn token_rate_calculation() {
        let mut m = StreamHealthMonitor::new(Duration::from_secs(60), Duration::from_secs(600));
        m.record_chunk(true, 20);
        let rate = m.token_rate();
        assert!(rate > 0.0, "token rate should be positive: {rate}");
    }

    #[test]
    fn token_rate_zero_when_empty() {
        let m = StreamHealthMonitor::new(Duration::from_secs(60), Duration::from_secs(600));
        assert_eq!(m.token_rate(), 0.0);
    }
}
