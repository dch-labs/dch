//! Markdown parsing and rendering for terminal display.
//!
//! Turns raw markdown into a [`ComponentRoot`] of block-level
//! [`TextComponent`]s laid out at a terminal width, and renders such
//! trees into styled ratatui lines via [`render_markdown`]. The module
//! is deliberately self-contained — it may depend on `ratatui`, `pest`,
//! `unicode-width`, and (behind the `syntax-highlight` feature)
//! tree-sitter, and on nothing else from this crate — so it can lift
//! out whole as a standalone markdown-for-ratatui crate. Three rules
//! keep that possible: the module owns its own theme types (never
//! importing this crate's), it knows nothing about agents or sessions
//! or where its strings come from, and its only attachment is the
//! `pub mod` line that includes it.

#[cfg(feature = "syntax-highlight")]
mod highlight;
mod parser;
pub mod render;
pub mod text_component;
pub mod theme;
pub mod word;

pub use parser::{ComponentRoot, parse_markdown};
pub use render::render_markdown;
pub use text_component::{TextComponent, TextNode};
pub use theme::{MarkdownTheme, SyntaxCapture, SyntaxTheme};
pub use word::{MetaData, Word, WordType};
