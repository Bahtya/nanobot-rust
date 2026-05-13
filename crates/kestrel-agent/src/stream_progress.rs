//! Stream progress tracker.
//!
//! State machine that tracks the lifecycle of a single LLM interaction and
//! decides when to send IM status messages to keep the user informed during
//! long waits.

use std::time::{Duration, Instant};

// ── Types ──────────────────────────────────────────────────────

/// Phases of the LLM interaction lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamPhase {
    /// Provider request initiated, waiting for TCP/TLS handshake.
    Connecting,
    /// Connection established, waiting for the first SSE chunk.
    WaitingFirstByte,
    /// Content is being streamed to the user.
    Streaming,
    /// Tool execution in progress (between LLM iterations).
    ToolExecution,
    /// Interaction complete.
    Done,
}

/// A progress message to send to the user.
#[derive(Debug, Clone)]
pub struct ProgressMessage {
    pub content: String,
    /// Whether this is a warning (vs informational).
    pub is_warning: bool,
}

// ── Tracker ────────────────────────────────────────────────────

/// Tracks the state of the current LLM interaction and decides when to emit
/// IM status messages.
pub struct StreamProgressTracker {
    phase: StreamPhase,
    phase_entered_at: Instant,
    stream_started_at: Instant,

    // Thresholds
    first_byte_warning_after: Duration,
    first_byte_slow_after: Duration,
    message_timeout_warning_ratio: f64,
    message_timeout: Duration,

    // Dedup flags
    sent_first_byte_warning: bool,
    sent_first_byte_slow: bool,
    sent_timeout_warning: bool,
    sent_reconnecting: bool,
}

impl StreamProgressTracker {
    pub fn new(message_timeout: Duration) -> Self {
        Self {
            phase: StreamPhase::Connecting,
            phase_entered_at: Instant::now(),
            stream_started_at: Instant::now(),
            first_byte_warning_after: Duration::from_secs(10),
            first_byte_slow_after: Duration::from_secs(30),
            message_timeout_warning_ratio: 0.8,
            message_timeout,
            sent_first_byte_warning: false,
            sent_first_byte_slow: false,
            sent_timeout_warning: false,
            sent_reconnecting: false,
        }
    }

    /// Transition to a new phase.
    pub fn transition(&mut self, new_phase: StreamPhase) {
        self.phase = new_phase;
        self.phase_entered_at = Instant::now();
        // Reset per-phase dedup flags
        self.sent_first_byte_warning = false;
        self.sent_first_byte_slow = false;
    }

    /// Called when a reconnection attempt starts.
    pub fn on_reconnect(&mut self) -> Option<ProgressMessage> {
        if !self.sent_reconnecting {
            self.sent_reconnecting = true;
            Some(ProgressMessage {
                content: "\u{26a0}\u{fe0f} \u{8fde}\u{63a5}\u{8d85}\u{65f6}\u{ff0c}\u{6b63}\u{5728}\u{5c1d}\u{8bd5}\u{91cd}\u{8fde}...".to_string(),
                is_warning: true,
            })
        } else {
            None
        }
    }

    /// Poll for progress messages.  Call periodically (every few seconds).
    pub fn poll(&mut self) -> Option<ProgressMessage> {
        let phase_elapsed = self.phase_entered_at.elapsed();
        let total_elapsed = self.stream_started_at.elapsed();

        match self.phase {
            StreamPhase::WaitingFirstByte => {
                if !self.sent_first_byte_warning && phase_elapsed >= self.first_byte_warning_after {
                    self.sent_first_byte_warning = true;
                    return Some(ProgressMessage {
                        content:
                            "\u{1f914} \u{6a21}\u{578b}\u{6b63}\u{5728}\u{63a8}\u{7406}\u{4e2d}..."
                                .to_string(),
                        is_warning: false,
                    });
                }
                if !self.sent_first_byte_slow && phase_elapsed >= self.first_byte_slow_after {
                    self.sent_first_byte_slow = true;
                    return Some(ProgressMessage {
                        content: format!(
                            "\u{23f3} \u{54cd}\u{5e94}\u{8f83}\u{6162}\u{ff0c}\u{6b63}\u{5728}\u{7b49}\u{5f85}\u{ff08}\u{5df2}\u{7b49}\u{5f85} {}s\u{ff09}...",
                            phase_elapsed.as_secs()
                        ),
                        is_warning: true,
                    });
                }
            }
            StreamPhase::Streaming | StreamPhase::ToolExecution
                if !self.sent_timeout_warning
                    && total_elapsed.as_secs_f64() / self.message_timeout.as_secs_f64()
                        >= self.message_timeout_warning_ratio =>
            {
                self.sent_timeout_warning = true;
                return Some(ProgressMessage {
                    content: format!(
                        "\u{26a0}\u{fe0f} \u{5373}\u{5c06}\u{8d85}\u{65f6}\u{ff08}\u{5df2}\u{7528} {}s / \u{9650}\u{5236} {}s\u{ff09}...",
                        total_elapsed.as_secs(),
                        self.message_timeout.as_secs()
                    ),
                    is_warning: true,
                });
            }
            StreamPhase::Streaming | StreamPhase::ToolExecution => {}
            _ => {}
        }
        None
    }

    /// Current phase.
    pub fn phase(&self) -> &StreamPhase {
        &self.phase
    }

    /// Total elapsed time since stream started.
    pub fn total_elapsed(&self) -> Duration {
        self.stream_started_at.elapsed()
    }
}

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_phase_is_connecting() {
        let t = StreamProgressTracker::new(Duration::from_secs(300));
        assert_eq!(*t.phase(), StreamPhase::Connecting);
    }

    #[test]
    fn transition_updates_phase() {
        let mut t = StreamProgressTracker::new(Duration::from_secs(300));
        t.transition(StreamPhase::WaitingFirstByte);
        assert_eq!(*t.phase(), StreamPhase::WaitingFirstByte);
    }

    #[test]
    fn poll_connecting_returns_none() {
        let mut t = StreamProgressTracker::new(Duration::from_secs(300));
        assert!(t.poll().is_none());
    }

    #[test]
    fn first_byte_warning_after_threshold() {
        let mut t = StreamProgressTracker::new(Duration::from_secs(300));
        t.first_byte_warning_after = Duration::from_millis(50);
        t.transition(StreamPhase::WaitingFirstByte);
        std::thread::sleep(Duration::from_millis(60));
        let msg = t.poll();
        assert!(msg.is_some());
        let msg = msg.unwrap();
        assert!(!msg.is_warning);
        assert!(msg.content.contains("\u{6a21}\u{578b}")); // 模型
    }

    #[test]
    fn first_byte_warning_fires_only_once() {
        let mut t = StreamProgressTracker::new(Duration::from_secs(300));
        t.first_byte_warning_after = Duration::from_millis(50);
        t.transition(StreamPhase::WaitingFirstByte);
        std::thread::sleep(Duration::from_millis(60));
        assert!(t.poll().is_some());
        assert!(t.poll().is_none());
    }

    #[test]
    fn slow_warning_after_longer_threshold() {
        let mut t = StreamProgressTracker::new(Duration::from_secs(300));
        t.first_byte_warning_after = Duration::from_millis(20);
        t.first_byte_slow_after = Duration::from_millis(50);
        t.transition(StreamPhase::WaitingFirstByte);
        std::thread::sleep(Duration::from_millis(60));
        let _first = t.poll(); // warning
        let slow = t.poll(); // slow warning
        assert!(slow.is_some());
        assert!(slow.unwrap().is_warning);
    }

    #[test]
    fn timeout_warning_at_80_percent() {
        let mut t = StreamProgressTracker::new(Duration::from_millis(200));
        t.transition(StreamPhase::Streaming);
        // Wait for 80% of 200ms = 160ms
        std::thread::sleep(Duration::from_millis(170));
        let msg = t.poll();
        assert!(msg.is_some());
        let msg = msg.unwrap();
        assert!(msg.is_warning);
        assert!(msg.content.contains("\u{5373}\u{5c06}")); // 即将
    }

    #[test]
    fn on_reconnect_fires_once() {
        let mut t = StreamProgressTracker::new(Duration::from_secs(300));
        let first = t.on_reconnect();
        assert!(first.is_some());
        assert!(t.on_reconnect().is_none());
    }

    #[test]
    fn done_phase_no_messages() {
        let mut t = StreamProgressTracker::new(Duration::from_secs(300));
        t.transition(StreamPhase::Done);
        assert!(t.poll().is_none());
    }
}
