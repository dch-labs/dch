//! The multi-line input editor.
//!
//! [`InputEditor`] is the TUI's single-mode (always insert) text
//! editor: a multi-line buffer with a char-boundary cursor, a
//! readline-style history, and word-wrap geometry for rendering.
//! It owns no frame and no terminal — the app renders
//! [`InputEditor::display_rows`] and positions the terminal
//! cursor from [`InputEditor::cursor_cell`].

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthStr;

/// Spaces inserted by the Tab key.
///
/// Spaces rather than a tab character so the rendered column in the
/// input box matches the buffer's geometry exactly.
const TAB_WIDTH: usize = 4;

/// What a key wants the app to do after the editor mutated itself.
///
/// The editor performs no app-level work of its own — submitting,
/// queueing, and redrawing are the host's decisions, so a key's
/// whole app-facing outcome is one of these values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    /// Enter was pressed with submittable text.
    ///
    /// The editor has already cleared its buffer and recorded the
    /// text on history, so the host receives the text exactly once.
    Submit(String),

    /// Ctrl-L was pressed.
    ///
    /// The app clears and forces one full redraw; the editor's own
    /// state does not change.
    Redraw,

    /// The key was consumed or ignored.
    ///
    /// No app-level action follows — though the caller still redraws,
    /// because the cursor may have moved even when nothing else did.
    None,
}

/// Submitted-input history with Up/Down navigation.
///
/// The cursor sits at "live" (the new-entry position) when `None`
/// and on entry `i` while browsing. A draft typed before browsing is
/// stashed by the editor so returning to live restores it. Entries
/// are capped FIFO and consecutive duplicates collapse, so resubmitting
/// the same text does not flood the list.
#[derive(Debug, Clone)]
pub struct InputHistory {
    /// Submitted entries, oldest first.
    ///
    /// Ordered so the navigation cursor is a plain index and the
    /// oldest entry is the first to leave when the cap evicts.
    entries: Vec<String>,

    /// Index into the entries, or `None` at the live position.
    ///
    /// `None` means the caller is typing a new entry; `Some(i)`
    /// means a browsed entry is loaded and returning to live
    /// restores the stashed draft.
    cursor: Option<usize>,

    /// Maximum entries retained; zero means unbounded.
    ///
    /// Bounds a long-running session's memory; eviction happens
    /// only at the cap, one oldest entry per push past it.
    max: usize,
}

impl InputHistory {
    /// Create an empty history retaining at most `max` entries.
    ///
    /// The navigation cursor starts at the live position; a `max` of
    /// zero keeps every entry.
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            entries: Vec::new(),
            cursor: None,
            max,
        }
    }

    /// Whether no entry is recorded.
    ///
    /// Feeds the editor's arrow-key fallback: with nothing to recall,
    /// Up and Down stay on the transcript.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Record a submitted entry and return to the live position.
    ///
    /// A consecutive duplicate is dropped — re-submitting the same
    /// text should not stack copies. Eviction removes the oldest
    /// entry past the cap.
    pub fn push(&mut self, entry: String) {
        self.cursor = None;
        if self.entries.last().is_some_and(|kept| *kept == entry) {
            return;
        }
        if self.max > 0 && self.entries.len() >= self.max {
            self.entries.remove(0);
        }
        self.entries.push(entry);
    }

    /// Move to the previous (older) entry, returning it.
    ///
    /// Returns `None` when already at the oldest entry, leaving the
    /// position unchanged.
    pub fn older(&mut self) -> Option<&str> {
        match self.cursor {
            None => {
                if self.entries.is_empty() {
                    return None;
                }
                self.cursor = Some(self.entries.len().saturating_sub(1));
            }
            Some(0) => return None,
            Some(index) => self.cursor = Some(index.saturating_sub(1)),
        }
        self.cursor
            .and_then(|index| self.entries.get(index))
            .map(String::as_str)
    }

    /// Move to the next (newer) entry, returning it.
    ///
    /// Returns `None` on arriving back at the live position — the
    /// caller restores the stashed draft.
    pub fn newer(&mut self) -> Option<&str> {
        let index = self.cursor?;
        let newer = index.saturating_add(1);
        if newer >= self.entries.len() {
            self.cursor = None;
            return None;
        }
        self.cursor = Some(newer);
        self.entries.get(newer).map(String::as_str)
    }

    /// Abort navigation; back to the live position.
    ///
    /// Editing and submitting both call this, so the next Up recalls
    /// from the newest entry again.
    pub fn reset(&mut self) {
        self.cursor = None;
    }
}

impl Default for InputHistory {
    fn default() -> Self {
        Self::new(1000)
    }
}

/// A multi-line text editor for the TUI input area.
///
/// Single editing mode, always insert. The buffer holds `\n`
/// separated logical lines; the cursor is a byte offset that always
/// sits on a UTF-8 character boundary, from which the display
/// geometry ([`InputEditor::display_rows`],
/// [`InputEditor::cursor_cell`]) is derived per render — nothing
/// position-shaped is stored, so re-wraps on resize are free.
#[derive(Debug, Clone)]
pub struct InputEditor {
    /// The buffer; `\n` separates logical lines.
    ///
    /// Kept as one `String` so every edit is a single insert or
    /// drain at the cursor, whatever the logical line layout.
    text: String,

    /// Cursor as a byte offset into the buffer.
    ///
    /// Always on a UTF-8 character boundary — every public mutator
    /// asserts it — because it feeds `String::insert` and `drain`
    /// directly; display coordinates are derived from it per
    /// render, never stored.
    cursor: usize,

    /// Submitted-entry history with a navigation cursor.
    ///
    /// Owned here so recall survives buffer replacement; the cap
    /// and duplicate collapse live on the history itself.
    history: InputHistory,

    /// The in-progress draft stashed while browsing history.
    ///
    /// Taken when the first recall loads an entry and restored
    /// when navigation returns to the live position, so browsing
    /// never costs the half-typed line.
    draft: Option<String>,

    /// Soft cap in characters; an insert admits only what fits
    /// under it.
    ///
    /// A defense against pathological pastes — the model's own
    /// context limit sits upstream and is not this editor's concern.
    max_chars: usize,
}

impl InputEditor {
    /// Create an empty editor with the default history cap.
    ///
    /// The buffer starts empty, the cursor at its start, and the
    /// history capped at a thousand entries.
    #[must_use]
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            history: InputHistory::default(),
            draft: None,
            max_chars: 100_000,
        }
    }

    /// The current buffer.
    ///
    /// Empty right after a submit; keystrokes, pastes, and history
    /// recalls replace it wholesale.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the buffer holds nothing.
    ///
    /// One half of the arrow-key fallback gate — see
    /// [`InputEditor::has_history`] for the other.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Whether the buffer spans more than one logical line.
    ///
    /// Decides the Up/Down split: line motion when true, history
    /// recall when false.
    #[must_use]
    pub fn is_multiline(&self) -> bool {
        self.text.contains('\n')
    }

    /// Whether any history entry is recorded.
    ///
    /// Together with [`InputEditor::is_empty`], gates the arrow-key
    /// fallback that leaves Up and Down on the transcript.
    #[must_use]
    pub fn has_history(&self) -> bool {
        !self.history.is_empty()
    }

    /// Replace the whole buffer; the cursor lands at the end.
    ///
    /// History navigation lands browsed entries and stashed drafts
    /// here.
    pub fn set_text(&mut self, text: String) {
        debug_assert!(self.text.is_char_boundary(self.cursor));
        self.cursor = text.len();
        self.text = text;
    }

    /// Clear the buffer and any in-progress history navigation.
    ///
    /// The submit path and the whitespace-only Enter both land here;
    /// a stashed draft is dropped with the buffer, not restored.
    pub fn clear(&mut self) {
        debug_assert!(self.text.is_char_boundary(self.cursor));
        self.text.clear();
        self.cursor = 0;
        self.draft = None;
        self.history.reset();
    }

    /// Insert text at the cursor, respecting the character cap.
    ///
    /// The bracketed-paste path lands whole blocks here as one edit,
    /// so a multi-line paste never partial-submits. A cap-rejected
    /// insert touches nothing — the buffer, the browse position, and
    /// any stashed draft come out exactly as they went in.
    pub fn insert_str(&mut self, pasted: &str) {
        debug_assert!(self.text.is_char_boundary(self.cursor));
        let remaining = self.max_chars.saturating_sub(self.text.chars().count());
        let prefix: String = pasted.chars().take(remaining).collect();
        if prefix.is_empty() {
            return;
        }
        self.history.reset();
        self.draft = None;
        self.text.insert_str(self.cursor, &prefix);
        self.cursor = self.cursor.saturating_add(prefix.len());
    }

    /// Process one key press; the editor mutates itself and reports
    /// what the app should do.
    ///
    /// Enter submits (whitespace-only buffers are cleared, not
    /// submitted); Shift+Enter inserts a newline where the terminal
    /// reports the modifier, and Ctrl+J — the terminal's own
    /// newline key, reported as a control-j on every terminal —
    /// does the same everywhere; `\` immediately before the cursor
    /// turns Enter into a newline on every terminal, consuming the
    /// backslash. Ctrl+M submits too: legacy terminals deliver it
    /// as the same byte as Enter, terminals on the xterm
    /// modifyOtherKeys protocol report the modifier on Enter
    /// itself, and kitty-protocol terminals report it as a
    /// control-m — all three spellings submit. Ctrl-C is
    /// deliberately not bound — quit and cancel own it at the app
    /// level.
    pub fn handle_key(&mut self, key: KeyEvent) -> InputAction {
        debug_assert!(self.text.is_char_boundary(self.cursor));
        match (key.code, key.modifiers) {
            (KeyCode::Enter, KeyModifiers::SHIFT)
            | (KeyCode::Char('\n'), _)
            | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                self.insert_newline();
                InputAction::None
            }
            (KeyCode::Enter, KeyModifiers::NONE | KeyModifiers::ALT | KeyModifiers::CONTROL)
            | (KeyCode::Char('m'), KeyModifiers::CONTROL) => self.enter(),
            (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                self.insert_char(c);
                InputAction::None
            }
            (KeyCode::Tab, _) => {
                self.insert_str(&" ".repeat(TAB_WIDTH));
                InputAction::None
            }
            (KeyCode::Backspace, _) => {
                self.delete_before_cursor();
                InputAction::None
            }
            (KeyCode::Delete, _) => {
                self.delete_at_cursor();
                InputAction::None
            }
            (KeyCode::Left, KeyModifiers::NONE) => {
                self.move_left();
                InputAction::None
            }
            (KeyCode::Right, KeyModifiers::NONE) => {
                self.move_right();
                InputAction::None
            }
            (KeyCode::Left, KeyModifiers::ALT) => {
                self.move_word_left();
                InputAction::None
            }
            (KeyCode::Right, KeyModifiers::ALT) => {
                self.move_word_right();
                InputAction::None
            }
            (KeyCode::Home, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                self.cursor = self.line_start();
                InputAction::None
            }
            (KeyCode::End, _) | (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                self.cursor = self.line_end();
                InputAction::None
            }
            (KeyCode::Up, KeyModifiers::NONE) => {
                self.up();
                InputAction::None
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                self.down();
                InputAction::None
            }
            (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.history_prev();
                InputAction::None
            }
            (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                self.history_next();
                InputAction::None
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                let start = self.line_start();
                self.text.drain(start..self.cursor);
                self.cursor = start;
                InputAction::None
            }
            (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                self.text.drain(self.cursor..self.line_end());
                InputAction::None
            }
            (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                self.delete_word_before();
                InputAction::None
            }
            (KeyCode::Char('l'), KeyModifiers::CONTROL) => InputAction::Redraw,
            _ => InputAction::None,
        }
    }

    /// The buffer's display rows at a wrap width.
    ///
    /// Each logical line wraps to `width` columns and the results
    /// flatten in order; an empty buffer is one empty row, so the
    /// renderer always has a row to draw.
    #[must_use]
    pub fn wrapped_lines(&self, width: u16) -> Vec<String> {
        let cap = usize::from(width.max(1));
        let mut rows = Vec::new();
        for logical in self.text.split('\n') {
            let wrapped = textwrap::wrap(logical, cap);
            if wrapped.is_empty() {
                rows.push(String::new());
            } else {
                rows.extend(wrapped.into_iter().map(std::borrow::Cow::into_owned));
            }
        }
        if rows.is_empty() {
            rows.push(String::new());
        }
        rows
    }

    /// The rows the input box renders at a wrap width.
    ///
    /// [`wrapped_lines`](Self::wrapped_lines) plus, when the caret
    /// sits just past text that exactly fills the buffer's final
    /// row, one empty continuation row — the wrap grid has no cell
    /// there, and the box renders where the next character will
    /// land instead of clamping the caret back onto the full row's
    /// first cell. Sizing and rendering must both use this so the
    /// caret's row always exists in the drawn area.
    #[must_use]
    pub fn display_rows(&self, width: u16) -> Vec<String> {
        let mut rows = self.wrapped_lines(width);
        if self
            .cursor_cell(width)
            .is_some_and(|(row, _)| usize::from(row) == rows.len())
        {
            rows.push(String::new());
        }
        rows
    }

    /// The cursor's `(row, column)` within the grid
    /// [`display_rows`](Self::display_rows) renders, in display
    /// columns (wide glyphs count their width).
    ///
    /// `None` when the buffer is empty — the caller places the
    /// terminal cursor at the box start. The column comes from
    /// wrapping the cursor's line prefix, so it follows the same
    /// breaks the rendered rows use; when that prefix exactly fills
    /// its final row the coordinates name the continuation row
    /// [`display_rows`](Self::display_rows) appends for exactly
    /// this case.
    #[must_use]
    pub fn cursor_cell(&self, width: u16) -> Option<(u16, u16)> {
        if self.text.is_empty() {
            return None;
        }
        let cap = usize::from(width.max(1));
        let before = self.text.get(..self.cursor).unwrap_or("");
        let line_index = before.matches('\n').count();
        let line_col = before.rfind('\n').map_or(self.cursor, |i| {
            self.cursor.saturating_sub(i.saturating_add(1))
        });
        let mut row: usize = 0;
        for (index, logical) in self.text.split('\n').enumerate() {
            let wrapped = textwrap::wrap(logical, cap);
            let rows_here = wrapped.len().max(1);
            if index == line_index {
                let prefix = logical.get(..line_col).unwrap_or("");
                // The sentinel keeps a trailing space attached through
                // the wrap — textwrap right-trims it otherwise and the
                // caret would report a column left of its true cell.
                let guarded = format!("{prefix}x");
                let partial = textwrap::wrap(&guarded, cap);
                let row_in_line = partial.len().saturating_sub(1);
                let column = partial
                    .last()
                    .map_or(0, |row| UnicodeWidthStr::width(row.as_ref()))
                    .saturating_sub(1);
                let cell_row = row.saturating_add(row_in_line);
                return Some((
                    u16::try_from(cell_row).unwrap_or(u16::MAX),
                    u16::try_from(column).unwrap_or(u16::MAX),
                ));
            }
            row = row.saturating_add(rows_here);
        }
        None
    }

    /// The Enter decision.
    ///
    /// A trailing backslash converts the press into a newline; a
    /// whitespace-only buffer clears without submitting; anything
    /// else submits and records on history.
    fn enter(&mut self) -> InputAction {
        if self.char_before() == Some('\\') {
            let cut = self.cursor.saturating_sub(1);
            self.text.drain(cut..self.cursor);
            self.cursor = cut;
            self.insert_newline();
            return InputAction::None;
        }
        if self.text.trim().is_empty() {
            self.clear();
            return InputAction::None;
        }
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.draft = None;
        self.history.push(text.clone());
        InputAction::Submit(text)
    }

    /// Insert one character through the shared insert path.
    ///
    /// Routing typing through [`InputEditor::insert_str`] applies
    /// the character cap and the history-browse reset uniformly.
    fn insert_char(&mut self, c: char) {
        self.insert_str(&c.to_string());
    }

    /// Insert a line break through the shared insert path.
    ///
    /// The Shift+Enter arm, the `\`-escape arm, and the bare newline
    /// character all funnel here.
    fn insert_newline(&mut self) {
        self.insert_str("\n");
    }

    /// The character immediately before the cursor, if any.
    ///
    /// The `\`-Escape check reads it; `None` at the buffer start.
    fn char_before(&self) -> Option<char> {
        self.text
            .get(..self.cursor)
            .and_then(|prefix| prefix.chars().next_back())
    }

    /// The character immediately after the cursor, if any.
    ///
    /// Delete-at-cursor consumes it; `None` at the buffer end.
    fn char_after(&self) -> Option<char> {
        self.text
            .get(self.cursor..)
            .and_then(|rest| rest.chars().next())
    }

    /// Byte offset where the cursor's logical line begins.
    ///
    /// Home and Ctrl-U target it; a line start is always a character
    /// boundary by construction.
    fn line_start(&self) -> usize {
        self.text
            .get(..self.cursor)
            .and_then(|prefix| prefix.rfind('\n'))
            .map_or(0, |i| i.saturating_add(1))
    }

    /// Byte offset where the cursor's logical line ends.
    ///
    /// The position of the line's `\n`, or the buffer end — the
    /// separator itself stays outside the span.
    fn line_end(&self) -> usize {
        self.text
            .get(self.cursor..)
            .and_then(|rest| rest.find('\n'))
            .map_or(self.text.len(), |i| self.cursor.saturating_add(i))
    }

    /// Step one character toward the buffer start.
    ///
    /// A multi-byte character crosses whole — the offset moves by
    /// its UTF-8 length, never into it.
    fn move_left(&mut self) {
        if let Some(c) = self.char_before() {
            self.cursor = self.cursor.saturating_sub(c.len_utf8());
        }
    }

    /// Step one character toward the buffer end.
    ///
    /// A multi-byte character crosses whole — the offset moves by
    /// its UTF-8 length, never into it.
    fn move_right(&mut self) {
        if let Some(c) = self.char_after() {
            self.cursor = self.cursor.saturating_add(c.len_utf8());
        }
    }

    /// Move the cursor up one logical line.
    ///
    /// The byte column carries over clamped to the line above's
    /// length, then rounds down to a character boundary — a short
    /// or multi-byte-bearing line never leaves the cursor
    /// mid-character.
    fn move_up(&mut self) {
        let start = self.line_start();
        let column = self.cursor.saturating_sub(start);
        let Some(preceding) = self.text.get(..start).and_then(|p| p.strip_suffix('\n')) else {
            return;
        };
        let above_start = preceding.rfind('\n').map_or(0, |i| i.saturating_add(1));
        let above_len = preceding.len().saturating_sub(above_start);
        self.cursor = self.clamp_to_boundary(above_start.saturating_add(column.min(above_len)));
    }

    /// Move the cursor down one logical line.
    ///
    /// The byte column carries over clamped to the line below's
    /// length, then rounds down to a character boundary — the same
    /// guarantee [`InputEditor::move_up`] gives in the other
    /// direction.
    fn move_down(&mut self) {
        let end = self.line_end();
        let column = self.cursor.saturating_sub(self.line_start());
        let Some(following) = self.text.get(end..).and_then(|r| r.strip_prefix('\n')) else {
            return;
        };
        let below_len = following.find('\n').unwrap_or(following.len());
        self.cursor =
            self.clamp_to_boundary(end.saturating_add(1).saturating_add(column.min(below_len)));
    }

    /// Walk a byte offset back to the nearest UTF-8 character
    /// boundary.
    ///
    /// Vertical movement clamps a byte column against a byte length,
    /// which can land between the bytes of a multi-byte character on
    /// the target line; the caret rounds down to the character it
    /// lands inside. The buffer start is always a boundary, so the
    /// walk terminates.
    fn clamp_to_boundary(&self, mut target: usize) -> usize {
        while !self.text.is_char_boundary(target) {
            target = target.saturating_sub(1);
        }
        target
    }

    /// The Up key.
    ///
    /// A multi-line buffer moves the cursor up a logical line; a
    /// single-line buffer recalls the previous history entry.
    fn up(&mut self) {
        if self.is_multiline() {
            self.move_up();
        } else {
            self.history_prev();
        }
    }

    /// The Down key.
    ///
    /// A multi-line buffer moves the cursor down a logical line; a
    /// single-line buffer steps to the next history entry.
    fn down(&mut self) {
        if self.is_multiline() {
            self.move_down();
        } else {
            self.history_next();
        }
    }

    /// Load the previous history entry.
    ///
    /// The current text is stashed as the draft on the first step
    /// away from the live position, so the browse can return to it
    /// intact.
    fn history_prev(&mut self) {
        if let Some(entry) = self.history.older() {
            if self.draft.is_none() {
                self.draft = Some(std::mem::take(&mut self.text));
                self.cursor = 0;
            }
            let entry = entry.to_string();
            self.set_text(entry);
        }
    }

    /// Load the next history entry.
    ///
    /// Arriving back at the live position restores the stashed
    /// draft instead of an entry.
    fn history_next(&mut self) {
        match self.history.newer() {
            Some(entry) => {
                let entry = entry.to_string();
                self.set_text(entry);
            }
            None => {
                if let Some(draft) = self.draft.take() {
                    self.set_text(draft);
                }
            }
        }
    }

    /// Move to the start of the previous word.
    ///
    /// Skips any run of whitespace backward, then the word before
    /// it — the readline backward-word landing spot.
    fn move_word_left(&mut self) {
        let prefix = self.text.get(..self.cursor).unwrap_or("");
        let mut saw_word = false;
        for (index, c) in prefix.char_indices().rev() {
            if c.is_whitespace() {
                if saw_word {
                    self.cursor = index.saturating_add(c.len_utf8());
                    return;
                }
            } else {
                saw_word = true;
            }
        }
        self.cursor = 0;
    }

    /// Move to the end of the next word.
    ///
    /// Skips the word at the cursor — or, from whitespace, the run
    /// and the word after it — landing just past that word's last
    /// character: the readline forward-word spot.
    fn move_word_right(&mut self) {
        let rest = self.text.get(self.cursor..).unwrap_or("");
        let mut saw_word = false;
        for (index, c) in rest.char_indices() {
            if c.is_whitespace() {
                if saw_word {
                    self.cursor = self.cursor.saturating_add(index);
                    return;
                }
            } else {
                saw_word = true;
            }
        }
        self.cursor = self.text.len();
    }

    /// Delete the character before the cursor.
    ///
    /// At a line start the previous line break goes instead,
    /// joining the two lines — backspace never stalls at a
    /// boundary.
    fn delete_before_cursor(&mut self) {
        self.history.reset();
        self.draft = None;
        if let Some(c) = self.char_before() {
            let cut = self.cursor.saturating_sub(c.len_utf8());
            self.text.drain(cut..self.cursor);
            self.cursor = cut;
        }
    }

    /// Delete the character after the cursor.
    ///
    /// A no-op at the buffer end; deletes the line separator when it
    /// is next, joining the lines.
    fn delete_at_cursor(&mut self) {
        self.history.reset();
        self.draft = None;
        if let Some(c) = self.char_after() {
            let end = self.cursor.saturating_add(c.len_utf8());
            self.text.drain(self.cursor..end);
        }
    }

    /// Delete back to the start of the previous word.
    ///
    /// The whitespace run behind it goes too, matching where
    /// [`InputEditor::move_word_left`] would land.
    fn delete_word_before(&mut self) {
        self.history.reset();
        self.draft = None;
        let target = {
            let prefix = self.text.get(..self.cursor).unwrap_or("");
            let mut saw_word = false;
            let mut cut = 0;
            for (index, c) in prefix.char_indices().rev() {
                if c.is_whitespace() {
                    if saw_word {
                        cut = index.saturating_add(c.len_utf8());
                        break;
                    }
                } else {
                    saw_word = true;
                }
            }
            cut
        };
        self.text.drain(target..self.cursor);
        self.cursor = target;
    }
}

impl Default for InputEditor {
    fn default() -> Self {
        Self::new()
    }
}
