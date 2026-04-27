//! # kestrel-channels
//!
//! Channel system — base trait, manager, registry, and platform implementations.

pub mod base;
pub mod commands;
pub mod manager;
pub mod platforms;
pub mod registry;
pub mod stream_consumer;

pub use base::BaseChannel;
pub use commands::{
    handle_callback, handle_history, handle_history_callback, handle_settings_callback,
    handle_settings_paged, set_skill_registry, CommandDispatch, CommandResponse, HISTORY_PER_PAGE,
    SETTINGS_PER_PAGE,
};
pub use manager::ChannelManager;
pub use platforms::telegram::{
    CallbackAction, CallbackContext, CallbackResponse, CallbackRouter, InlineKeyboardBuilder,
    InlineKeyboardButton, InlineKeyboardMarkup,
};
pub use platforms::websocket::{run_ws_stream_consumer, WebSocketChannel};
pub use registry::ChannelRegistry;
pub use stream_consumer::{split_message, StreamConsumer};

#[cfg(test)]
pub(crate) mod test_support {
    use parking_lot::{Mutex, MutexGuard};
    use std::ffi::{OsStr, OsString};
    use std::sync::LazyLock;

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Serializes test access to process-global environment variables and
    /// restores the previous value when dropped.
    pub(crate) struct EnvVarGuard {
        _lock: MutexGuard<'static, ()>,
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        pub(crate) fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
            let lock = ENV_LOCK.lock();
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);

            Self {
                _lock: lock,
                key,
                previous,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
