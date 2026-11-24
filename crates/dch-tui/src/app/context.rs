//! The context window's live figure: the configured and refreshed
//! totals, the bar line the status area paints, and the retirement
//! that follows a model switch.

use super::TuiApp;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::sync::PoisonError;
use std::sync::atomic::Ordering;

impl TuiApp {
    /// Retire the engine-reported context window.
    ///
    /// The live figure in the shared state is a fact about the engine
    /// that last ran — the model a switch leaves behind. Every model
    /// switch retires it: the bar falls to the configured window of
    /// the model now named until the new engine's first report
    /// replaces it with live truth again.
    pub(crate) fn retire_live_context_window(&mut self) {
        self.state
            .context_window
            .store(0, std::sync::atomic::Ordering::SeqCst);
    }

    /// The window the active model's own settings name.
    ///
    /// The same resolution the engine is built on: the selected
    /// profile's `context_window` when it states one, else the base
    /// `[api]` figure, else the built-in fallback. The bar's fallback
    /// leg and a switch's verdict store both read this, so the figure
    /// the bar shows between a switch and the first new turn is the
    /// same one the engine runs.
    fn configured_context_window(&self) -> u64 {
        self.config
            .models
            .iter()
            .find(|profile| profile.model.eq_ignore_ascii_case(&self.config.api.model))
            .and_then(|profile| profile.context_window)
            .or(self.config.api.context_window)
            .unwrap_or(dch_config::FALLBACK_CONTEXT_WINDOW)
    }

    /// The context readout's inputs: the tokens used against the window
    /// the engine actually runs — the live window once the engine has
    /// reported one, the configured window of the model the config
    /// names before that.
    pub(crate) fn context_snapshot(&self) -> (u64, u64) {
        let live_window = self.state.context_window.load(Ordering::SeqCst);
        let window = if live_window > 0 {
            live_window
        } else {
            self.configured_context_window()
        };
        let used = self
            .state
            .tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .context_tokens;
        (used, window)
    }

    /// The context bar's own row: `padding` cells of air, then the
    /// fill over the track — one line the bottom row's numbers and the
    /// row above it share a column count through.
    pub(crate) fn context_bar_line(&self, padding: usize, fill: usize) -> Line<'static> {
        Line::from(vec![
            Span::raw(" ".repeat(padding)),
            Span::styled(
                " ".repeat(fill),
                Style::default().bg(self.theme.ui.scrollbar_thumb),
            ),
            Span::styled(
                " ".repeat(CONTEXT_BAR_CELLS.saturating_sub(fill)),
                Style::default().bg(self.theme.ui.scrollbar_track),
            ),
        ])
    }
}
/// Cells the status bar's context-usage bar fills.
///
/// Sixteen — wide enough that the numbers spelling the bar's fraction
/// never run past its right edge (`200k/200k (100%)`, the widest the
/// compact units spell, is sixteen cells) — while staying a glanceable
/// fraction rather than a second scrollbar.
pub(crate) const CONTEXT_BAR_CELLS: usize = 16;

/// The rows the status bar occupies at any width: three — a row of
/// air under the composer, the bar's row, and the bottom row. The
/// air is the composer's bottom margin and stays unpainted, so the
/// identity row that closes the box — the model, its effort, and the
/// mode — reads as the last thing in the composer and the footer's
/// facts start one clear row under it. The footer carries no prose
/// to wrap: its facts are the numbers leading the bottom row from
/// the bar's left edge and the id closing it at the right, and a
/// frame too narrow for both forgoes the id rather than growing
/// rows.
pub(crate) const STATUS_BAR_ROWS: u16 = 3;

/// The footer's air: the rows of its margin, painted by no pane.
/// They belong to no content — an overlay sheet may stand on them,
/// which is how the margin costs the sheets nothing: their bottom
/// edge keeps the same clear distance above the bar's own rows.
pub(crate) const STATUS_BAR_AIR: u16 = 1;

/// The footer's side margins: one cell of air at each edge. The
/// bar and the numbers beneath it start one cell in — the bar
/// standing exactly over what it measures — and the session id
/// closes its row one cell short of the right edge.
///
/// One cell each side — a breath, not an inset: the readout is
/// still the first thing at the left and the id the last at the
/// right; neither touches the frame's edge.
pub(crate) const STATUS_BAR_MARGIN: usize = 1;

/// The filled-cell count of the status bar's context-usage bar.
///
/// `used` tokens against a `window` budget, rounded to the nearest of
/// `cells` cells and clamped to the bar — a full bar when usage
/// somehow exceeds the window, an empty one when the window is
/// unknown (zero), which is the fresh-session state before the first
/// turn reports.
pub(crate) fn context_bar_fill(used: u64, window: u64, cells: usize) -> usize {
    if window == 0 {
        return 0;
    }
    let cells = u64::try_from(cells).unwrap_or(u64::MAX);
    let filled = used
        .saturating_mul(cells)
        .saturating_add(window.saturating_div(2))
        .checked_div(window)
        .unwrap_or(cells);
    usize::try_from(filled.min(cells)).unwrap_or(usize::MAX)
}

/// The numbers that spell the context bar's fraction.
///
/// The percentage rounds from the true ratio — not from the bar's
/// cells, whose steps would make every reading end in zero — and the
/// totals ride in compact units, thousands up to a million and
/// millions beyond (`419k/1M`, never `1000k`), so a bar kept
/// deliberately glanceable still answers "how full, exactly". A
/// session beyond its window shows its honest over-100 percentage:
/// the bar clamps full, the number tells the truth. An unknown window
/// (zero) returns `None`: there is no denominator, and the bar above
/// the readout is already empty.
pub(crate) fn context_bar_readout(used: u64, window: u64) -> Option<(String, String)> {
    if window == 0 {
        return None;
    }
    let percent = used
        .saturating_mul(100)
        .saturating_add(window.saturating_div(2))
        .checked_div(window)
        .unwrap_or(0);
    let compact = |tokens: u64| {
        let thousands = tokens.saturating_add(500).checked_div(1_000).unwrap_or(0);
        if thousands < 1_000 {
            return format!("{thousands}k");
        }
        let tenths = thousands.saturating_add(50).checked_div(100).unwrap_or(0);
        let whole = tenths.checked_div(10).unwrap_or(0);
        let frac = tenths.saturating_sub(whole.saturating_mul(10));
        if frac == 0 {
            format!("{whole}M")
        } else {
            format!("{whole}.{frac}M")
        }
    };
    Some((
        format!("{percent}%"),
        format!("{}/{}", compact(used), compact(window)),
    ))
}

#[cfg(test)]
mod tests {
    use crate::app::TuiApp;
    use crate::app::test_support::*;

    use crate::message::TuiMessage;

    #[test]
    fn the_status_bar_shows_the_context_bar_at_the_configured_fill() {
        let mut app = app();
        // The footer belongs to a conversation with turns — the
        // welcome paints none — so the bar rides a landed prompt.
        app.push_message(TuiMessage::User {
            text: "hello".to_string(),
            timestamp: chrono::Utc::now(),
            steered: false,
            unsent: false,
        });
        app.state
            .tokens
            .lock()
            .expect("the tokens lock")
            .context_tokens = 100_000;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("draw");
        let thumb = app.theme.ui.scrollbar_thumb;
        let buffer = terminal.backend().buffer();
        let mut thumb_cells = 0usize;
        for y in 0..24u16 {
            for x in 0..80u16 {
                if buffer[(x, y)].style().bg == Some(thumb) {
                    thumb_cells += 1;
                }
            }
        }
        assert_eq!(thumb_cells, 8, "half the window fills half of the bar");
        let rows: Vec<String> = (0..24u16)
            .map(|y| {
                (0..80u16)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        let numbers_row = rows
            .iter()
            .position(|row| row.contains("100k/200k (50%)"))
            .expect("the totals and their percent spell below the bar");
        let bar_row = u16::try_from(numbers_row.saturating_sub(1)).unwrap_or(0);
        let bar_cells = (0..80u16)
            .filter(|&x| buffer[(x, bar_row)].style().bg == Some(thumb))
            .count();
        assert_eq!(
            bar_cells, 8,
            "the bar rides the row directly above the numbers that spell it"
        );
    }

    #[test]
    fn cost_command_reports_context_and_tokens() {
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        let out = command_output(&mut app, "/cost");
        assert!(out.contains("Context:"), "got {out:?}");
        assert!(out.contains("tokens used"), "got {out:?}");
    }

    #[test]
    fn a_session_update_lands_in_the_status_bar_fields() {
        // The reverse lane: facts the driver computes and the display cannot.
        let mut app = TuiApp::new(dch_config::DchConfig::default());
        app.apply_session_update(crate::queue::SessionUpdate::SessionId {
            id: "abc-123".to_string(),
        });
        assert_eq!(app.host.session_id.as_deref(), Some("abc-123"));
        app.apply_session_update(crate::queue::SessionUpdate::ModelName {
            name: "gpt-4o".to_string(),
        });
        assert_eq!(app.config.api.model, "gpt-4o");
        // The window follows the accepted model: once the verdict names the
        // wide profile, the retired engine figure gives way to that
        // profile's configured window.
        app.config.models = vec![dch_config::ModelProfile {
            model: "wide".to_string(),
            context_window: Some(400_000),
            ..dch_config::ModelProfile::default()
        }];
        app.retire_live_context_window();
        app.apply_session_update(crate::queue::SessionUpdate::ModelName {
            name: "wide".to_string(),
        });
        let (_, window) = app.context_snapshot();
        assert_eq!(
            window, 400_000,
            "the bar reads the accepted model's configured window"
        );
    }
}
