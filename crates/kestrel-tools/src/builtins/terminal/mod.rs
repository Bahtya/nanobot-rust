//! Lightweight cross-platform terminal multiplexer built on `portable-pty`.
//!
//! Provides session management (create, send input, read output, kill, resize)
//! suitable for AI-driven terminal orchestration. Each session wraps a PTY
//! backed by the system's native pseudo-terminal (Unix PTY or Windows ConPTY).

mod emulator;
mod manager;
mod screen;
mod session;
mod tools;

pub use emulator::{
    escape_control, parse_bytes, strip_ansi, EraseMode, IncrementalUtf8Decoder, ReadMode,
    TerminalOp,
};
pub use manager::TerminalManager;
pub use screen::{
    Cell, CellAttributes, ChangedLine, Color, ScreenDiff, ScreenSnapshot, TerminalScreen,
    DEFAULT_MAX_SCROLLBACK,
};
pub use session::{validate_shell, SessionInfo, TerminalSession};
pub use tools::register_terminal_tools;
pub use tools::{
    TerminalCaptureScreenTool, TerminalCaptureScrollbackTool, TerminalCreateSessionTool,
    TerminalKillSessionTool, TerminalListSessionsTool, TerminalReadOutputTool, TerminalResizeTool,
    TerminalSendInputTool, TerminalSendKeyTool, TerminalWaitForScreenChangeTool,
};
