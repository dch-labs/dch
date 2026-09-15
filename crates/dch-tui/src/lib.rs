//! Ratatui terminal UI for `dch`: themes, markdown rendering, the
//! application shell, and streaming display.

#![warn(missing_docs)]

pub mod app;
pub mod events;
pub mod input;
pub mod markdown;
pub mod message;
pub mod observer;
pub mod terminal;
pub mod theme;

pub use app::TuiApp;
pub use events::TerminalEvents;
pub use input::{InputAction, InputEditor, InputHistory};
pub use message::{ActiveTool, ContentBlock, TokenCounts, TuiMessage};
pub use observer::{ToolResultDisplay, TuiObserver, TuiObserverState};
pub use terminal::{TerminalGuard, init_terminal, restore_terminal};
