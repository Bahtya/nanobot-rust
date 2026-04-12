//! # nanobot-heartbeat
//!
//! Heartbeat service — periodic task checking with two-phase LLM evaluation.

pub mod service;

pub use service::HeartbeatService;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lib_exports() {
        // Verify the module compiles and re-exports are accessible
        let _ = std::mem::size_of::<HeartbeatService>();
    }
}
