//! Lightweight cross-platform terminal multiplexer built on `portable-pty`.
//!
//! Provides session management (create, send input, read output, kill, resize)
//! suitable for AI-driven terminal orchestration. Each session wraps a PTY
//! backed by the system's native pseudo-terminal (Unix PTY or Windows ConPTY).

mod manager;
mod session;
mod tools;

pub use manager::TerminalManager;
pub use session::{validate_shell, SessionInfo, TerminalSession};
pub use tools::register_terminal_tools;
pub use tools::{
    TerminalCreateSessionTool, TerminalKillSessionTool, TerminalListSessionsTool,
    TerminalReadOutputTool, TerminalResizeTool, TerminalSendInputTool,
};
