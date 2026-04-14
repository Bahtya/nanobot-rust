//! Agent hook system — lifecycle event handlers.
//!
//! Matches the Python `agent/hook.py` AgentHook and CompositeHook pattern.

use async_trait::async_trait;
use nanobot_bus::events::AgentEvent;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Hook context passed to lifecycle handlers.
#[derive(Debug, Clone)]
pub struct HookContext {
    pub event: AgentEvent,
    pub data: HashMap<String, Value>,
}

/// Trait for agent lifecycle hooks.
#[async_trait]
pub trait AgentHook: Send + Sync {
    /// Called when an agent event occurs.
    async fn on_event(&self, context: &HookContext) -> anyhow::Result<()> {
        let _ = context;
        Ok(())
    }

    /// Hook name for identification.
    fn name(&self) -> &str;
}

/// A composite hook that delegates to multiple hooks.
/// Errors in individual hooks are logged but don't prevent other hooks from running.
pub struct CompositeHook {
    hooks: Vec<Arc<dyn AgentHook>>,
}

impl CompositeHook {
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    /// Add a hook to the composite chain.
    pub fn add(&mut self, hook: Arc<dyn AgentHook>) {
        self.hooks.push(hook);
    }

    /// Emit an event to all registered hooks in order.
    pub async fn emit(&self, context: &HookContext) {
        for hook in &self.hooks {
            match hook.on_event(context).await {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!("Hook '{}' error: {}", hook.name(), e);
                }
            }
        }
    }
}

impl Default for CompositeHook {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestHook {
        call_count: AtomicUsize,
    }

    impl TestHook {
        fn new() -> Self {
            Self {
                call_count: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl AgentHook for TestHook {
        fn name(&self) -> &str {
            "test_hook"
        }

        async fn on_event(&self, _context: &HookContext) -> anyhow::Result<()> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn test_composite_hook_new() {
        let hook = CompositeHook::new();
        assert!(hook.hooks.is_empty());
    }

    #[tokio::test]
    async fn test_composite_hook_emit_no_hooks() {
        let hook = CompositeHook::new();
        let ctx = HookContext {
            event: AgentEvent::Started {
                session_key: "test".to_string(),
            },
            data: std::collections::HashMap::new(),
        };
        // Should not panic
        hook.emit(&ctx).await;
    }

    #[tokio::test]
    async fn test_composite_hook_emit_with_hook() {
        let mut composite = CompositeHook::new();
        let test_hook = Arc::new(TestHook::new());
        composite.add(test_hook.clone());

        let ctx = HookContext {
            event: AgentEvent::Started {
                session_key: "test".to_string(),
            },
            data: std::collections::HashMap::new(),
        };
        composite.emit(&ctx).await;
        assert_eq!(test_hook.call_count.load(Ordering::SeqCst), 1);

        // Emit again
        composite.emit(&ctx).await;
        assert_eq!(test_hook.call_count.load(Ordering::SeqCst), 2);
    }
}
