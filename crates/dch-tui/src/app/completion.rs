//! The composer's Tab completion: the popup's candidate cycle,
//! its key handling, and the theme browsing a highlighted row
//! previews.

use super::TuiApp;
use crate::commands;
use crate::commands::CompletionSources;
use crate::theme::Theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

impl TuiApp {
    /// The suggestion popup's keys, taken before the editor sees
    /// them.
    ///
    /// Up and Down belong to the popup while it stands, and stepping
    /// applies the row it lands on into the composer — the same act a
    /// Tab press performs — so the highlighted row and the drafted
    /// line are always the same choice. Enter needs no arm of its own
    /// here: the row the arrows chose is already the composer's text,
    /// so Enter falls through to the editor and submits what stands.
    /// Everything else falls through to the editor untouched.
    pub(crate) fn completion_key(&mut self, key: KeyEvent) -> Option<bool> {
        if !self.completion_popup_live() {
            return None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Up, KeyModifiers::NONE) => Some(self.step_completion(true)),
            (KeyCode::Down, KeyModifiers::NONE) => Some(self.step_completion(false)),
            (KeyCode::Enter, KeyModifiers::NONE | KeyModifiers::ALT | KeyModifiers::CONTROL)
            | (KeyCode::Char('m'), KeyModifiers::CONTROL)
                if self.previewed_a_theme() =>
            {
                self.apply_highlighted_row();
                None
            }
            _ => None,
        }
    }

    /// Whether the current state shows a theme the browse previewed.
    ///
    /// The popup's Enter arm reads this to decide whether the standing
    /// highlight represents the user's browsing choice — a theme the
    /// palette is currently previewing that differs from the session's
    /// own — so Enter can name it in the composer before submitting.
    fn previewed_a_theme(&self) -> bool {
        self.browse.theme_before_browse.is_some()
            && self
                .composer
                .input
                .text()
                .strip_prefix("/theme ")
                .is_some_and(str::is_empty)
    }

    /// Write the highlighted candidate into the composer, replacing
    /// whatever follows the prefix.
    ///
    /// The popup's Enter act: the user browsed with arrows (or Tab),
    /// the highlight stands on a row, and Enter names it so the
    /// submit commits the choice the screen says is selected. The
    /// prefix is kept — only the value after it is written.
    fn apply_highlighted_row(&mut self) {
        let Some(cycle) = self.completion.completion.as_ref() else {
            return;
        };
        let Some(chosen) = cycle.candidates.get(cycle.index) else {
            return;
        };
        let text = format!("{}{}", cycle.prefix, chosen);
        self.composer.input.set_text(text);
    }

    /// Tab on a command line, in either direction: `BackTab` and
    /// Shift-Tab step backwards through the same list.
    pub(crate) fn tab_completion(&mut self, key: KeyEvent) -> bool {
        let reverse = key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
        self.complete_command_line(reverse)
    }

    /// The candidates the suggestion popup would show for the line as
    /// typed — the popup's own rule, shared with the keys that
    /// navigate it.
    ///
    /// A leading-slash line with more than nothing to offer, minus the
    /// sole candidate already fully spelled, minus a popup the user
    /// dismissed with Esc at this exact draft (any edit re-arms it —
    /// the stamp changes with the text). A cycle the keys are walking
    /// overrides all of that: its own list stands, so stepping through
    /// candidates with Tab or the arrows never collapses the popup to
    /// the word just applied.
    pub(crate) fn popup_candidates(&self) -> Option<(String, Vec<String>)> {
        if self.completion.completion_dismissed == Some(self.composer.input.stamp()) {
            return None;
        }
        let line = self.composer.input.text();
        if !line.starts_with('/') {
            return None;
        }
        if let Some(cycle) = &self.completion.completion
            && line.starts_with(&cycle.prefix)
            && cycle
                .candidates
                .iter()
                .any(|candidate| line == cycle.prefix.clone() + candidate)
        {
            return Some((cycle.prefix.clone(), cycle.candidates.clone()));
        }
        let (prefix, candidates, _) = self.fresh_completion(line);
        if candidates.is_empty() {
            return None;
        }
        if candidates.len() == 1
            && candidates
                .first()
                .is_some_and(|only| line == prefix.clone() + only)
        {
            return None;
        }
        Some((prefix, candidates))
    }

    /// Whether the suggestion popup stands above the composer now.
    ///
    /// The renderer's and the key handlers' shared gate: while it
    /// holds, the popup owns the arrows and Enter owes it a row when a
    /// step has moved the highlight.
    pub(crate) fn completion_popup_live(&self) -> bool {
        self.popup_candidates().is_some()
    }

    /// Preview the theme the popup's highlight stands on, when the
    /// popup is walking `/theme` candidates.
    ///
    /// The palette follows the highlight in every state the popup can
    /// be in — stepping and applying alike — so what the screen says
    /// is selected is what the screen is
    /// painted in. Other completions (models, keys) pass through:
    /// nothing about their rows previews.
    fn preview_highlighted_theme_row(&mut self) {
        if let Some(cycle) = &self.completion.completion
            && cycle.prefix.starts_with("/theme ")
            && let Some(name) = cycle.candidates.get(cycle.index)
            && let Some(theme) = Theme::by_name(name)
        {
            self.preview_theme(theme);
        }
    }

    /// The completion cycle's live half: when the popup is walking
    /// `/theme` candidates — arrows stepping, Tab cycling — each
    /// candidate the text lands on previews immediately, the same
    /// whole-palette repaint a bare `/theme` step performs. The
    /// preview is session-only and stateless: nothing persists until
    /// Enter submits the command, and the candidate showing when the
    /// user stops stepping is the one the screen keeps.
    fn preview_theme_from_completion(&mut self) {
        let line = self.composer.input.text().to_string();
        let Some(name) = line.strip_prefix("/theme ") else {
            return;
        };
        if name.is_empty() || name.contains(char::is_whitespace) {
            return;
        }
        if let Some(theme) = Theme::by_name(name) {
            self.preview_theme(theme);
        }
    }

    /// Move the popup's highlight one row, wrapping at the ends, and
    /// apply the row it lands on.
    ///
    /// An arrow is a selection in every popup: the composer takes the
    /// landed row — the command menu writes the command word, a value
    /// list writes the value — the same text a Tab writes, so the
    /// composer, the highlight, and the palette never disagree about
    /// what is selected; the palette previews the theme the landed
    /// row names. Tab and Shift+Tab continue from the row the arrows
    /// landed on; the keys share one position. Typing replaces the
    /// applied row and restarts the filter from the prefix, and a
    /// stale cycle position is clamped into whatever list the line now
    /// filters to — a paste or an edit that shortened the candidates
    /// can never leave the walk on an index that no longer exists,
    /// the shape that swallowed the key.
    fn step_completion(&mut self, up: bool) -> bool {
        let Some((prefix, candidates)) = self.popup_candidates() else {
            return false;
        };
        let line = self.composer.input.text().to_string();
        let len = candidates.len();
        let current = match &self.completion.completion {
            Some(cycle)
                if cycle.prefix == prefix
                    && line
                        .strip_prefix(&cycle.prefix)
                        .is_some_and(|rest| !rest.contains(char::is_whitespace)) =>
            {
                cycle.index.min(len.saturating_sub(1))
            }
            _ => 0,
        };
        let index = if up {
            current.checked_sub(1).unwrap_or(len.saturating_sub(1))
        } else {
            current
                .checked_add(1)
                .and_then(|next| next.checked_rem(len))
                .unwrap_or(0)
        };
        let total = candidates.len();
        if candidates.get(index).is_none() {
            return true;
        }
        let applied = candidates.get(index).map(|row| format!("{prefix}{row}"));
        let bare = line.strip_prefix(&prefix).is_some_and(str::is_empty);
        self.completion.completion = Some(CompletionCycle {
            prefix,
            candidates,
            index,
            bare,
            window_start: self
                .completion
                .completion
                .as_ref()
                .map_or(0, |cycle| cycle.window_start)
                .clamp(index.saturating_sub(POPUP_SHOWN.saturating_sub(1)), index)
                .min(total.saturating_sub(POPUP_SHOWN)),
        });
        if let Some(text) = applied {
            self.composer.input.set_text(text);
        }
        self.preview_highlighted_theme_row();
        true
    }

    /// Truncate the composer to the cycle's prefix when the next key
    /// is a typed character and the composer spells exactly a row the
    /// popup applied.
    ///
    /// An applied row is a selection, and typing replaces a
    /// selection: an `n` typed over an applied `dracula` filters to
    /// `/theme n`, never `/theme draculan`. A fragment the user
    /// built by hand — a partial filter, a bare prefix — is not an
    /// applied row, and typing appends to it as always. A space is
    /// not filter text: it appends, moving a command word on to its
    /// value position.
    pub(crate) fn drop_applied_row_for_typing(&mut self, key: KeyEvent) {
        let typed = matches!(key.code, KeyCode::Char(ch) if ch != ' ')
            && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT);
        if !typed {
            return;
        }
        let Some(cycle) = &self.completion.completion else {
            return;
        };
        let line = self.composer.input.text();
        let Some(rest) = line.strip_prefix(&cycle.prefix) else {
            return;
        };
        if !rest.is_empty() && cycle.candidates.iter().any(|row| row == rest) {
            let prefix = cycle.prefix.clone();
            self.composer.input.set_text(prefix);
        }
    }

    /// Retire a theme browse the composer left by any path.
    ///
    /// Esc is the deliberate exit and the commit (`/theme <name>` Enter)
    /// is the chosen one, but a browse can also end by the composer
    /// drifting away — a backspace past the space, a cleared draft, a
    /// new command typed over the old one. In every one of those the
    /// previewed theme is a leftover the user walked past, not a
    /// choice; the palette returns to the theme the session was on,
    /// the same restoration Esc performs, so no path out of the browse
    /// leaves the screen painted in a theme nobody picked.
    pub(crate) fn retire_theme_browse_if_gone(&mut self) {
        let Some(theme) = self.browse.theme_before_browse.clone() else {
            return;
        };
        let browse_stands = self
            .popup_candidates()
            .is_some_and(|(prefix, _)| prefix.starts_with("/theme "));
        if !browse_stands {
            self.browse.theme_before_browse = None;
            self.theme = theme;
            self.browse.pending_theme_osc = Some((self.theme.ui.background, self.theme.ui.primary));
            self.invalidate_conversation_renders();
        }
    }

    /// Esc's idle duties, innermost first: a standing popup goes away,
    /// then the draft clears.
    ///
    /// One Esc closes only the popup — the draft it stood over stays,
    /// so dismissing the menu never costs the half-typed line; the
    /// next Esc, with no popup standing, is the one that clears the
    /// draft. While a run holds, Esc keeps its cancel-arm meaning
    /// instead — this runs only on the idle branch. An empty composer
    /// returns the key unhandled rather than claiming it.
    pub(crate) fn esc_clears_or_dismisses(&mut self) -> bool {
        if self.completion_popup_live() {
            self.completion.completion_dismissed = Some(self.composer.input.stamp());
            self.completion.completion = None;
            if let Some(theme) = self.browse.theme_before_browse.take() {
                self.theme = theme;
                self.browse.pending_theme_osc =
                    Some((self.theme.ui.background, self.theme.ui.primary));
                self.invalidate_conversation_renders();
            }
            return true;
        }
        if self.composer.input.is_empty() {
            return false;
        }
        self.composer.input.clear();
        self.completion.completion = None;
        self.completion.completion_dismissed = None;
        true
    }

    /// Complete the command word (or argument) the composer is on,
    /// stepping forward or back.
    ///
    /// On a line starting with `/` the first Tab applies the first
    /// matching candidate (a first Shift+Tab the last) and the popup
    /// keeps standing on the whole list; each further Tab steps to the
    /// next candidate — Shift+Tab or `BackTab` steps back — pre-filling
    /// the composer as it goes; the popup's arrows apply the landed
    /// row the same way, from the same shared position.
    /// The candidate list is frozen at the first press so cycling never
    /// re-completes a completed word, and an edit between presses
    /// re-derives it. Any other line leaves Tab to the editor's indent.
    /// Returns whether the key was used.
    pub(crate) fn complete_command_line(&mut self, reverse: bool) -> bool {
        if !self.composer.input.text().starts_with('/') {
            return false;
        }
        let line = self.composer.input.text().to_string();
        let mut carried_window = self
            .completion
            .completion
            .as_ref()
            .map_or(0, |c| c.window_start);
        let mut carried_bare = false;
        let (mut prefix, mut candidates, mut index) = match self.completion.completion.clone() {
            Some(cycle) => {
                if CompletionState::cycle_holds_the_line(&cycle, &line) {
                    let len = cycle.candidates.len();
                    let next = if reverse {
                        cycle.index.checked_sub(1).unwrap_or(len.saturating_sub(1))
                    } else {
                        cycle
                            .index
                            .checked_add(1)
                            .and_then(|next| next.checked_rem(len))
                            .unwrap_or(0)
                    };
                    carried_bare = cycle.bare;
                    (cycle.prefix, cycle.candidates, next)
                } else {
                    self.fresh_completion(&line)
                }
            }
            None => self.fresh_completion(&line),
        };
        if reverse && self.completion.completion.is_none() {
            index = index.max(candidates.len().saturating_sub(1));
        }
        if !line.ends_with(' ')
            && candidates
                .get(index)
                .is_some_and(|standing| format!("{prefix}{standing}") == line)
        {
            let (arg_prefix, arg_candidates, arg_index) =
                self.fresh_completion(&format!("{line} "));
            if !arg_candidates.is_empty() {
                prefix = arg_prefix;
                candidates = arg_candidates;
                index = arg_index;
                carried_window = 0;
                carried_bare = true;
            }
        }
        let Some(chosen) = candidates.get(index).cloned() else {
            return true;
        };
        let total = candidates.len();
        let window_start = if index < carried_window {
            index
        } else if index >= carried_window.saturating_add(POPUP_SHOWN) {
            index
                .saturating_sub(POPUP_SHOWN.saturating_sub(1))
                .min(total.saturating_sub(POPUP_SHOWN))
        } else {
            carried_window.min(total.saturating_sub(POPUP_SHOWN))
        };
        let bare = carried_bare || line.strip_prefix(&prefix).is_some_and(str::is_empty);
        self.composer.input.set_text(prefix.clone() + &chosen);
        self.completion.completion = Some(CompletionCycle {
            prefix,
            candidates,
            index,
            bare,
            window_start,
        });
        self.preview_theme_from_completion();
        if !self.composer.input.text().starts_with("/theme ") {
            self.post_notice(chosen);
        }
        true
    }

    /// Derive a completion cycle from scratch against the current line.
    ///
    /// Reads the command names plus the configured model and theme values into
    /// a [`CompletionSources`] and asks the parser for candidates. A prefix
    /// nothing starts with yields an empty list, which the caller reads as
    /// "nothing to complete" and leaves the line untouched.
    pub(crate) fn fresh_completion(&self, line: &str) -> (String, Vec<String>, usize) {
        let models: Vec<String> = self
            .config
            .models
            .iter()
            .map(|profile| profile.model.clone())
            .collect();
        let themes: Vec<String> = Theme::command_names()
            .into_iter()
            .map(str::to_string)
            .collect();
        let keys: Vec<String> = commands::config_keys()
            .into_iter()
            .map(str::to_string)
            .collect();
        let sources = CompletionSources {
            models: &models,
            themes: &themes,
            keys: &keys,
        };
        (
            completion_prefix(line),
            commands::complete(line, sources),
            0,
        )
    }
}

/// The composer's Tab-completion state: the standing cycle and the
/// stamp that keeps a dismissed popup down.
///
/// The cycle exists only while the composer holds a command
/// mid-completion — any other keystroke drops it, so a stale
/// candidate list can never ride an edited word. The dismissal stamp
/// re-arms the popup the moment the draft changes.
#[derive(Default)]
pub(crate) struct CompletionState {
    /// The live `Tab` completion cycle for a `/`-command line, if any.
    ///
    /// Present only while the composer holds a command mid-completion; any
    /// other keystroke edits the line and drops it, so the cycle cannot
    /// carry a stale candidate list into an edited word.
    pub(crate) completion: Option<CompletionCycle>,

    /// The draft stamp at which Esc dismissed the suggestion popup.
    ///
    /// `None` while the popup stands free; `Some(stamp)` while the
    /// user has sent it away and the composer has not been edited —
    /// any edit changes the stamp and re-arms the popup.
    pub(crate) completion_dismissed: Option<u64>,
}

impl CompletionState {
    /// Whether an active cycle still holds the line — the bare prefix
    /// it opened on, or a row it applied — so its candidate list and
    /// highlight stand while the keys walk them.
    ///
    /// Both walkways (the arrows and Tab) apply the row they land on
    /// into the composer, so a cycle holds exactly while the composer
    /// spells one of its own states: the bare prefix, and only for a
    /// cycle that opened bare (a cycle opened on a fragment carries
    /// that fragment's filtered list, and the bare prefix owes a
    /// re-derive), or the prefix plus one of its candidates — the
    /// applied rows. Anything else — a hand-built fragment, an edit,
    /// a different word — retires the cycle and the popup re-derives
    /// from the new line: typing `ligh` after a bare-prefix walk must
    /// filter, never keep cycling the whole palette.
    fn cycle_holds_the_line(cycle: &CompletionCycle, line: &str) -> bool {
        line.strip_prefix(&cycle.prefix).is_some_and(|rest| {
            rest.is_empty() && cycle.bare
                || cycle.candidates.iter().any(|candidate| candidate == rest)
        })
    }
}

/// The completion popup's window cap.
///
/// How many rows the popup shows at once; the window scrolls only
/// when the selection steps past its edges.
const POPUP_SHOWN: usize = 6;

/// The in-progress `Tab` completion cycle for a `/`-command line.
///
/// Freezes the candidate list and the position reached so each further
/// Tab advances through the same list rather than re-deriving it against
/// the candidate just applied — which would complete a completed word and
/// strand the cycle on the first match. The `prefix` is the line up to and
/// including the space before the word being completed (just the leading
/// `/` for a command name), so re-applying a candidate keeps the rest of
/// the line fixed and swaps only the completed word.
#[derive(Debug, Clone)]
pub(crate) struct CompletionCycle {
    /// The text before the completed word, held fixed across the cycle.
    ///
    /// Re-applying a candidate re-attaches exactly this text, so only
    /// the completed word ever changes — `/model ` stays put while the
    /// value after it cycles.
    pub(crate) prefix: String,

    /// The candidate values the cycle walks, in completion order.
    ///
    /// Frozen at the first press so cycling cannot re-derive against a
    /// half-applied word; an edit between presses retires the cycle.
    pub(crate) candidates: Vec<String>,

    /// Which candidate currently stands in the composer.
    ///
    /// The next `Tab` steps forward from here, wrapping; the popup's
    /// highlight follows the same index.
    pub(crate) index: usize,

    /// Whether the cycle opened on the bare prefix — no fragment —
    /// so its candidate list is the unfiltered whole.
    ///
    /// A cycle opened on a fragment (`/theme ligh`) freezes that
    /// fragment's filtered list; returning to the bare prefix must
    /// re-derive, not keep cycling the stale filter. Only a cycle that
    /// opened bare holds the bare prefix.
    pub(crate) bare: bool,

    /// The popup window's start row, scrolled only at the edges.
    ///
    /// The selection moves through a still list: the window shifts
    /// only when the selection would step past its top or bottom
    /// edge, not to pin the selection at any particular row. This is
    /// what makes backwards navigation read as the highlight moving
    /// up, rather than the list sliding under a bottom-pinned
    /// selection.
    pub(crate) window_start: usize,
}

/// The part of a `/`-command line that completion leaves untouched.
///
/// Everything up to and including the last whitespace, so completing a
/// command argument keeps `/model ` or `/theme ` intact; with no whitespace
/// yet the line is on its first word and only the leading `/` is kept, so a
/// command name is completed onto a bare slash.
fn completion_prefix(line: &str) -> String {
    match line.char_indices().rev().find(|(_, ch)| ch.is_whitespace()) {
        Some((last, ch)) => line[..last.saturating_add(ch.len_utf8())].to_string(),
        None => "/".to_string(),
    }
}
