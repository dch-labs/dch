//! Ratatui terminal UI for `dch`: themes, markdown rendering, the
//! application shell, and streaming display.

#![warn(missing_docs)]

pub mod app;
pub mod commands;
pub mod config_fields;
pub mod diff;
pub mod events;
pub mod guidance;
pub mod input;
pub mod markdown;
pub mod message;
pub mod observer;
pub mod permission;
pub mod question;
pub mod queue;
pub mod terminal;
pub mod theme;
pub mod todo_view;
pub mod tool_capture;
pub mod tool_render;

pub use app::{CopyAnswer, TuiApp};
pub use commands::{Command, CommandSpec, CompletionSources, Parsed};
pub use events::TerminalEvents;
pub use guidance::Guidance;
pub use input::{InputAction, InputEditor, InputHistory};
pub use message::{ActiveTool, ContentBlock, TokenCounts, TuiMessage};
pub use observer::{
    CompactionEvent, CompactionFiling, Graduation, HealthNotice, ToolResultDisplay, TuiObserver,
    TuiObserverState, TurnPublication,
};
pub use permission::PermissionRequest;
pub use question::{QuestionChoice, QuestionPrompt};
pub use queue::{
    PromptQueue, QueuedPrompt, ResumedTokens, SessionCommand, SessionUpdate, Submission,
};
pub use terminal::{
    TerminalGuard, arm_fatal_signal_restore, enter_signal_deaf_window, exit_signal_deaf_window,
    ignore_fatal_signals, init_terminal, reset_cursor_color, restore_terminal, set_cursor_color,
    sync_default_background,
};
pub use tool_capture::{CapturingMiddleware, ToolCapture, ToolCaptureSink};
