//! Ratatui terminal UI for `dch`: themes, markdown rendering, the
//! application shell, and streaming display.

#![warn(missing_docs)]

pub mod app;
pub mod events;
pub mod input;
pub mod markdown;
pub mod message;
pub mod observer;
pub mod permission;
pub mod question;
pub mod terminal;
pub mod theme;
pub mod tool_capture;
pub mod tool_render;

pub use app::{CopyAnswer, TuiApp};
pub use events::TerminalEvents;
pub use input::{InputAction, InputEditor, InputHistory};
pub use message::{ActiveTool, ContentBlock, TokenCounts, TuiMessage};
pub use observer::{Graduation, HealthNotice, ToolResultDisplay, TuiObserver, TuiObserverState};
pub use permission::PermissionRequest;
pub use question::{QuestionChoice, QuestionPrompt};
pub use terminal::{
    TerminalGuard, arm_fatal_signal_restore, enter_signal_deaf_window, exit_signal_deaf_window,
    init_terminal, restore_terminal, sync_default_background,
};
pub use tool_capture::{CapturingMiddleware, ToolCapture, ToolCaptureSink};
