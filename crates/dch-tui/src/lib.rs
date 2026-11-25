//! Ratatui terminal UI for `dch`: themes, markdown rendering, the
//! application shell, and streaming display.

#![warn(missing_docs)]
// Tests assert on internals and fixtures: panicking helpers,
// `unwrap` and `expect` on states the fixture itself built, and
// docs-free test fns are the test canon, so the denied-lint set
// stands down for the whole crate under `cfg(test)` — one
// justification, in one place, instead of one per module.
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::let_underscore_must_use,
        clippy::missing_errors_doc,
        clippy::missing_panics_doc,
        clippy::panic,
        clippy::unwrap_used,
    )
)]

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
pub mod splash;
pub mod terminal;
pub mod theme;
pub mod todo_view;
pub mod tool_capture;
pub mod tool_render;

pub use app::TuiApp;
pub use app::clipboard::CopyAnswer;
pub use commands::{Command, CommandSpec, CompletionSources, Parsed};
pub use events::TerminalEvents;
pub use guidance::Guidance;
pub use input::{InputAction, InputEditor, InputHistory};
pub use markdown::cells;
pub use markdown::cells::{char_cells, display_width, for_display, str_width};
pub use message::{ActiveTool, ContentBlock, TokenCounts, TuiMessage};
pub use observer::{
    CompactionEvent, CompactionFiling, Graduation, HealthNotice, ToolResultDisplay, TuiObserver,
    TuiObserverState, TurnPublication,
};
pub use permission::PermissionRequest;
pub use question::{QuestionChoice, QuestionPrompt};
pub use queue::{
    PermissionMemory, PermissionRule, PromptQueue, QueuedPrompt, ResumedTokens, SessionCommand,
    SessionUpdate, Submission,
};
pub use terminal::{
    SessionTerminal, TerminalGuard, arm_fatal_signal_restore, enter_signal_deaf_window,
    exit_signal_deaf_window, ignore_fatal_signals, init_terminal, reset_cursor_color,
    restore_terminal, set_cursor_color, sync_default_background,
};
pub use tool_capture::{CapturingMiddleware, ToolCapture, ToolCaptureSink};
