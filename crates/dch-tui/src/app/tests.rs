//! The whole-app integration pins: frames rendered end to end,
//! commands driven through the panel, and the loop's wake-and-drain
//! rhythms.
use super::composer::relativize;
use super::context::{CONTEXT_BAR_CELLS, context_bar_fill, context_bar_readout};
use super::keys::key_survives_the_sheet;
use super::notices::{format_duration, wrap_prompt_rows, wrap_prompt_rows_last};
use super::permissions::{
    MAX_EXPANDED_OVERLAY_ROWS, MAX_OVERLAY_ROWS, expanded_command_budget, expanded_fixed_rows,
    popup_rect, popup_window,
};
use super::pulse::scaled_color;
use super::queue::{QUEUE_ROWS, queue_prompt_prefix};
use super::run::FrameStats;
use super::selection::reverse_selection;
use super::session_commands::fit_picker_title;
use super::todos::TOOL_DETAIL_CAP;

use super::clipboard::{CopyAnswer, CopyVerdict};
use super::selection::CellPos;
use super::selection::CellSelection;
use super::test_support::*;
use super::todos::ToolDetail;
use super::*;
use crate::cells::display_width;
use crate::cells::for_display;
use crate::commands::{self};
use crate::observer::CompactionFiling;
use crate::observer::TurnPublication;
use crate::queue::{PermissionRule, SessionPick};
use crossterm::event::{KeyCode, KeyModifiers};
use std::collections::HashMap;

#[test]
fn the_worst_draw_survives_the_second() {
    // The lane carries the slowest draw the window saw, not the
    // draw that happened to be standing when it closed.
    let epoch = Instant::now();
    let mut stats = FrameStats::default();
    stats.note_draw(Duration::from_millis(3), epoch);
    stats.note_draw(
        Duration::from_millis(120),
        epoch + Duration::from_millis(200),
    );
    let lane = stats
        .note_draw(
            Duration::from_millis(4),
            epoch + Duration::from_millis(1_000),
        )
        .expect("the second closes");
    assert_eq!(
        lane.worst_draw_ms, 120,
        "the window's maximum, not its last draw"
    );
}

#[test]
fn the_flip_styles_the_retained_opener_by_its_own_span() {
    // The flip's owner lookup keys on the same first retained
    // character as the strip's: a selector opening the line inside
    // its own span once attributed the character after it to the
    // selector's style, so the reversed ink flashed the wrong
    // color for the sweep.
    let mut lines = vec![Line::from(vec![
        Span::styled("\u{fe0f}", Style::default().fg(Color::Red)),
        Span::styled("a b", Style::default().fg(Color::Green)),
    ])];
    reverse_selection(
        &mut lines,
        0,
        &CellSelection {
            anchor: CellPos { line: 0, col: 0 },
            head: CellPos { line: 0, col: 0 },
        },
    );
    let line = lines.first().expect("the fixture line survives the flip");
    let flipped = line
        .spans
        .iter()
        .find(|span| span.content.contains('a'))
        .expect("the retained opener flips");
    assert!(
        flipped
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::REVERSED),
        "the swept cell flips"
    );
    assert_eq!(
        flipped.style.fg,
        Some(Color::Green),
        "the flipped ink carries the character's own span's style"
    );
}

#[test]
fn an_expanded_edit_with_an_undiffable_output_keeps_the_plain_expansion() {
    // The edit family's rows own the diff render only when the
    // parser says diff; an error body or a large-file preview
    // falls back to the plain input/output expansion, and that
    // branch needs its own witness — a claim without a test is
    // how the branch rots.
    let mut subject = app();
    subject.tools.tool_details.insert(
        "call_err".to_string(),
        ToolDetail {
            input_json: "{\"file_path\": \"src/gone.rs\"}".to_string(),
            output: "Edit failed: old_text not found in src/gone.rs".to_string(),
        },
    );
    subject
        .expansion
        .expanded_tools
        .insert("call_err".to_string());
    let block = ContentBlock::Tool {
        name: "Edit".to_string(),
        call_id: "call_err".to_string(),
        input_preview: "src/gone.rs".to_string(),
        output_preview: String::new(),
        retained_input: String::new(),
        success: false,
        elapsed_secs: 0.1,
    };
    let mut lines = Vec::new();
    let mut summaries = HashMap::new();
    subject.push_tool_block_lines(&block, 80, &mut lines, &mut summaries, &mut Vec::new());
    let flat: String = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        flat.contains("input:"),
        "the plain expansion renders for an undiffable edit output: {flat}"
    );
    assert!(
        flat.contains("output:"),
        "the raw output is shown verbatim: {flat}"
    );
    assert!(
        !flat.contains("lines changed"),
        "no stats bar renders for an undiffable output: {flat}"
    );
}

#[test]
fn an_expanded_edit_renders_its_diff_with_real_line_numbers() {
    // The expanded detail of an edit-family call is the parsed
    // diff — banded rows, both gutters — not the raw receipt; the
    // layout follows the config, and an unparseable output keeps
    // the plain input/output expansion.
    let mut config = dch_config::DchConfig::default();
    config.display.diff_layout = dch_config::DiffLayout::Inline;
    let mut subject = TuiApp::new(config);
    let receipt = [
        "Changed: src/lib.rs (modified)",
        "│ 1 line removed, 1 added",
        "@@ -38,3 +38,3 @@",
        "  fn main() {",
        "-     let old = previous();",
        "+     let new = followup();",
        "  }",
    ]
    .join("\n");
    subject.tools.tool_details.insert(
        "call_diff".to_string(),
        ToolDetail {
            input_json: "{\"file_path\": \"src/lib.rs\"}".to_string(),
            output: receipt,
        },
    );
    subject
        .expansion
        .expanded_tools
        .insert("call_diff".to_string());
    let block = ContentBlock::Tool {
        name: "Edit".to_string(),
        call_id: "call_diff".to_string(),
        input_preview: "src/lib.rs".to_string(),
        output_preview: String::new(),
        retained_input: String::new(),
        success: true,
        elapsed_secs: 0.1,
    };
    let mut lines = Vec::new();
    let mut summaries = HashMap::new();
    subject.push_tool_block_lines(&block, 100, &mut lines, &mut summaries, &mut Vec::new());
    let rendered: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.to_string())
                .collect()
        })
        .collect();
    let joined = rendered.join("\n");
    assert!(
        joined.contains("+1") && joined.contains("-1") && joined.contains("2 lines changed"),
        "the stats bar leads the block: {joined}"
    );
    assert!(
        !joined.contains("@@"),
        "no raw hunk header — the rendered rows carry the addresses"
    );
    assert!(
        joined.contains("39") && joined.contains("previous();"),
        "the removal carries its real old-side number: {joined}"
    );
    assert!(
        !joined.contains("input:"),
        "the diff replaces the raw input/output expansion: {joined}"
    );
    assert!(
        subject
            .tools
            .tool_details
            .get("call_diff")
            .is_some_and(|detail| detail.output.contains("let new")),
        "the detail record itself is untouched by rendering"
    );
}

#[test]
fn the_half_cell_edges_render_beside_the_center() {
    let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
    let mut config = dch_config::DchConfig::default();
    config.display.strip_style = dch_config::StripStyle::Worm;
    let mut app = TuiApp::from_observer_state(config, state.clone());
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    app.run_anchor = Some(
        Instant::now()
            .checked_sub(Duration::from_millis(1850))
            .expect("the monotonic clock is well past its epoch"),
    );
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
    terminal
        .draw(|frame| {
            app.render_pulse_strip(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 1,
                },
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert!(
        buffer
            .cell((33, 0))
            .is_some_and(|cell| cell.symbol() == " "),
        "clear cells sit before the run"
    );
    assert!(
        buffer
            .cell((34, 0))
            .is_some_and(|cell| cell.symbol() == "╶"),
        "the tail's solid half-tick leads the run"
    );
    assert!(
        buffer
            .cell((35, 0))
            .is_some_and(|cell| cell.symbol() == "─"),
        "full cells carry the run between the dashes"
    );
    assert!(
        buffer
            .cell((45, 0))
            .is_some_and(|cell| cell.symbol() == "─"),
        "full cells reach the head's half"
    );
    assert!(
        buffer
            .cell((46, 0))
            .is_some_and(|cell| cell.symbol() == "╴"),
        "the head's solid half-tick ends the run"
    );
    assert!(
        buffer
            .cell((47, 0))
            .is_some_and(|cell| cell.symbol() == " "),
        "clear cells sit beyond the head"
    );
}

#[test]
fn a_stale_turn_publication_never_backfills_a_later_manual_pass() {
    // The interleaving the attribution stamps exist for: an earlier
    // fold's marked head sits in a pending turn-end publication
    // when a later manual pass's row lands in the same drain. The
    // publication answers pass one and the row is pass two, so the
    // row takes its own filing's words and the stale publication
    // settles nothing.
    let mut app = app();
    let stale = std::sync::Arc::new(vec![loopctl::message::Message::user(
        "## Conversation summary (compacted)\n- the first pass's ledger",
    )]);
    app.state
        .turn_histories
        .lock()
        .expect("the turn-history lock")
        .push(TurnPublication {
            passes: 1,
            history: stale,
        });
    let own = std::sync::Arc::new(vec![loopctl::message::Message::user(
        "## Conversation summary (compacted)\n- the second pass's ledger",
    )]);
    app.state
        .compaction_publications
        .lock()
        .expect("the publications lock")
        .push(CompactionFiling {
            pass: 2,
            folded: own,
        });
    app.state
        .compactions
        .lock()
        .expect("the compaction lock")
        .push(CompactionEvent {
            pass: 2,
            record: TuiMessage::Compacted {
                tokens_before: 10,
                tokens_after: 5,
                reason: "manual".to_string(),
                evicted: 3,
                duration_ms: 100,
                compactor: Some("structured".to_string()),
                compression_ratio: Some(2.0),
                summary: None,
                timestamp: chrono::Utc::now(),
            },
        });
    app.drain_shared_state();
    match app.conversation().last() {
        Some(TuiMessage::Compacted { summary, .. }) => assert!(
            summary
                .as_deref()
                .is_some_and(|text| text.contains("the second pass's ledger")),
            "the row takes its own pass's words, never the earlier fold's: {summary:?}"
        ),
        other => panic!("the compacted row landed: {other:?}"),
    }
}

#[test]
fn a_late_filing_never_serves_a_later_pass() {
    // The orphaned-filing interleaving: a row settles through the
    // turn-end channel and its own filing arrives only afterwards,
    // too late to pair. The late filing must linger inert — its
    // pass is spent, its number never reused — while the next
    // manual pass's row takes exactly its own filing's words.
    let mut app = app();
    let settled = std::sync::Arc::new(vec![loopctl::message::Message::user(
        "## Conversation summary (compacted)\n- the first pass's ledger",
    )]);
    app.state
        .turn_histories
        .lock()
        .expect("the turn-history lock")
        .push(TurnPublication {
            passes: 1,
            history: settled,
        });
    app.state
        .compactions
        .lock()
        .expect("the compaction lock")
        .push(CompactionEvent {
            pass: 1,
            record: compacted_row("threshold exceeded"),
        });
    app.drain_shared_state();
    let late = std::sync::Arc::new(vec![loopctl::message::Message::user(
        "## Conversation summary (compacted)\n- the first pass's late filing",
    )]);
    let own = std::sync::Arc::new(vec![loopctl::message::Message::user(
        "## Conversation summary (compacted)\n- the second pass's ledger",
    )]);
    {
        let mut filings = app
            .state
            .compaction_publications
            .lock()
            .expect("the publications lock");
        filings.push(CompactionFiling {
            pass: 1,
            folded: late,
        });
        filings.push(CompactionFiling {
            pass: 2,
            folded: own,
        });
    }
    app.state
        .compactions
        .lock()
        .expect("the compaction lock")
        .push(CompactionEvent {
            pass: 2,
            record: compacted_row("manual"),
        });
    app.drain_shared_state();
    let rows = app.conversation();
    match rows.first() {
        Some(TuiMessage::Compacted { summary, .. }) => assert!(
            summary
                .as_deref()
                .is_some_and(|text| text.contains("the first pass's ledger")),
            "the first row keeps the words its own publication carried: {summary:?}"
        ),
        other => panic!("the first compacted row landed: {other:?}"),
    }
    match rows.last() {
        Some(TuiMessage::Compacted { summary, .. }) => assert!(
            summary
                .as_deref()
                .is_some_and(|text| text.contains("the second pass's ledger")),
            "the later row takes its own filing's words, never the \
                 orphan's: {summary:?}"
        ),
        other => panic!("the second compacted row landed: {other:?}"),
    }
    let lingering: Vec<u64> = app
        .state
        .compaction_publications
        .lock()
        .expect("the publications lock")
        .iter()
        .map(|filing| filing.pass)
        .collect();
    assert_eq!(
        lingering,
        vec![1],
        "the late filing lingers inert — spent passes are never reused"
    );
}

#[test]
fn an_ordinary_message_addition_keeps_a_standing_debt_standing() {
    // The release is the replacement's, not the list's: a conversation
    // that merely grows — a reply landing, a tool result arriving —
    // keeps its debts, for their indexes still name the rows they
    // were stamped against.
    let mut app = app();
    app.state
        .compactions
        .lock()
        .expect("the compaction lock")
        .push(CompactionEvent {
            pass: 1,
            record: compacted_row("threshold exceeded"),
        });
    app.drain_shared_state();
    assert_eq!(
        app.compaction_summary_owed,
        vec![(1, 0)],
        "the unmarked pass leaves its debt"
    );
    app.push_message(TuiMessage::Assistant {
        blocks: vec![ContentBlock::Text {
            text: "work continues".to_string(),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    });
    app.drain_shared_state();
    assert_eq!(
        app.compaction_summary_owed,
        vec![(1, 0)],
        "an ordinary addition never releases a debt"
    );
}

#[test]
fn the_arts_height_never_trims_the_picture() {
    // The whole picture at every resolution: a pane that cannot
    // seat a rung's rows declines to a smaller rung — height
    // narrows the fjord, it never windows its water away.
    let short = rendered_art_columns(120, 24);
    let mid = rendered_art_columns(120, 50);
    let tall = rendered_art_columns(120, 90);
    assert_eq!(mid, tall, "past the fit, the width holds: {mid} vs {tall}");
    assert!(
        short < mid,
        "a shorter pane narrows to a smaller whole rung: {short} vs {mid}"
    );

    let short_rows = rendered_art_rows(120, 24);
    let tall_rows = rendered_art_rows(120, 90);
    assert!(
        short_rows < tall_rows,
        "a taller pane shows a taller rung, whole: {short_rows} vs {tall_rows}"
    );
}

#[test]
fn the_arts_width_follows_the_panes_width() {
    // A wider pane earns a wider rung — the ladder's whole point —
    // up to the ladder's ceiling, beyond which the width holds and
    // only the height can add rows.
    let snug = rendered_art_columns(80, 50);
    let roomy = rendered_art_columns(100, 50);
    let giant = rendered_art_columns(120, 50);
    assert!(
        snug < roomy && roomy < giant,
        "width drives the rung: {snug} < {roomy} < {giant}"
    );
    let beyond = rendered_art_columns(220, 50);
    let ceiling = rendered_art_columns(200, 50);
    assert_eq!(
        ceiling, beyond,
        "past the ladder's ceiling the width holds: {ceiling} vs {beyond}"
    );
}

#[test]
fn a_roomy_pane_paints_the_larger_scale() {
    // Quality follows both axes under the cover-crop: width drives
    // the rung up to the ladder's ceiling, and past it a taller
    // pane adds rows of the same-wide art.
    let snug = rendered_art(50, 40);
    let roomy = rendered_art(140, 30);
    let giant = rendered_art(220, 80);
    assert!(snug < 1200, "the snug pane holds the smallest rung: {snug}");
    assert!(
        snug * 2 < roomy,
        "the roomy pane holds a wider rung than snug: {snug} vs {roomy}"
    );
    assert!(
        giant > roomy,
        "the giant pane shows more rows of the same-wide art: \
             {giant} vs {roomy}"
    );
}

#[test]
fn alt_arrows_walk_the_submitted_prompts() {
    // Three prompts with tall answers between them and a steered
    // echo beside the last: Alt+Up seats each prompt's first row
    // at the viewport's top in turn — the echo is not a stop — the
    // walk past the first prompt answers with a notice instead of
    // a move, and Alt+Down walks back the other way.
    let mut app = app();
    let tall = "an answer paragraph of some length, wrapping across \
             several rows when the pane is narrow\n"
        .repeat(6);
    for prompt in ["first ask", "second ask", "third ask"] {
        app.push_message(TuiMessage::User {
            text: prompt.to_string(),
            timestamp: chrono::Utc::now(),
            steered: false,
            unsent: false,
        });
        app.push_message(TuiMessage::Assistant {
            blocks: vec![ContentBlock::Text { text: tall.clone() }],
            timestamp: chrono::Utc::now(),
            duration_ms: None,
        });
    }
    app.push_message(TuiMessage::User {
        text: "a steered echo beside the tail".to_string(),
        timestamp: chrono::Utc::now(),
        steered: true,
        unsent: false,
    });
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).expect("terminal");
    terminal.draw(|frame| app.render(frame)).expect("draw");
    assert_eq!(
        app.bake.user_prompt_marks.len(),
        3,
        "only submitted prompts are stops: {:?}",
        app.bake.user_prompt_marks
    );
    let total = app.pacing.last_layout_lines;
    let viewport = app.pacing.last_layout_viewport;
    let marks = app.bake.user_prompt_marks.clone();
    let mark = |index: usize| marks.get(index).copied().unwrap_or_default();
    let expected = |mark: usize| total.saturating_sub(viewport).saturating_sub(mark);

    app.handle_key_inner(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    assert_eq!(
        app.scroll_offset(),
        expected(mark(2)),
        "Alt+Up seats the newest submitted prompt at the top"
    );
    app.handle_key_inner(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    assert_eq!(
        app.scroll_offset(),
        expected(mark(1)),
        "the second press walks one prompt older"
    );
    app.handle_key_inner(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    assert_eq!(
        app.scroll_offset(),
        expected(mark(0)),
        "the third press seats the first prompt"
    );
    app.handle_key_inner(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT));
    assert!(
        app.transient_notice
            .as_ref()
            .is_some_and(|(text, _)| text.contains("no earlier prompt")),
        "past the oldest prompt the walk answers with a notice: {:?}",
        app.transient_notice
    );
    app.handle_key_inner(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT));
    assert_eq!(
        app.scroll_offset(),
        expected(mark(1)),
        "Alt+Down walks back toward the newer prompt"
    );
}

#[test]
fn a_finished_run_stamps_its_total_onto_the_final_reply_only() {
    let mut app = app();
    let mut queue = app.state.graduations.lock().expect("the graduation lock");
    queue.push(crate::observer::Graduation::Reply {
        text: "checking".to_string(),
        thinking: None,
    });
    queue.push(crate::observer::Graduation::Reply {
        text: "done".to_string(),
        thinking: None,
    });
    queue.push(crate::observer::Graduation::RunEnded {
        duration_ms: 84_000,
        success: true,
    });
    drop(queue);
    app.drain_shared_state();
    let durations: Vec<Option<u64>> = app
        .conversation()
        .iter()
        .filter_map(|message| match message {
            TuiMessage::Assistant { duration_ms, .. } => Some(*duration_ms),
            _ => None,
        })
        .collect();
    assert_eq!(
        durations,
        vec![None, Some(84_000)],
        "only the run's final reply carries the run's total"
    );
}

#[test]
fn a_run_that_graduated_no_reply_drops_its_total() {
    let mut app = app();
    app.push_message(TuiMessage::User {
        text: "do a thing".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
        unsent: false,
    });
    app.state
        .graduations
        .lock()
        .expect("the graduation lock")
        .push(crate::observer::Graduation::RunEnded {
            duration_ms: 84_000,
            success: true,
        });
    app.drain_shared_state();
    assert!(
        app.conversation()
            .last()
            .is_some_and(|message| matches!(message, TuiMessage::User { .. })),
        "a run with no reply leaves the conversation alone — no stranger is stamped"
    );
}

#[test]
fn an_assistant_reply_renders_its_duration_as_a_trailing_line() {
    let mut app = app();
    app.push_message(TuiMessage::Assistant {
        blocks: vec![ContentBlock::Text {
            text: "the measured answer".to_string(),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: Some(5_424_500),
    });
    app.push_message(TuiMessage::Assistant {
        blocks: vec![ContentBlock::Text {
            text: "the unmeasured answer".to_string(),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    });
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).expect("terminal");
    terminal.draw(|frame| app.render(frame)).expect("draw");
    let showing = buffer_text(terminal.backend().buffer());
    assert!(
        showing.contains("1h 30m 24s"),
        "a measured reply carries its duration on a trailing line: {showing}"
    );
    assert_eq!(
        showing.matches("1h 30m 24s").count(),
        1,
        "an unmeasured reply renders no duration line: {showing}"
    );
}

#[test]
fn durations_render_in_their_largest_fitting_units() {
    assert_eq!(format_duration(0), "<1s");
    assert_eq!(format_duration(999), "<1s");
    assert_eq!(format_duration(1_000), "1s");
    assert_eq!(format_duration(42_500), "42s");
    assert_eq!(format_duration(60_000), "1m 0s");
    assert_eq!(format_duration(84_000), "1m 24s");
    assert_eq!(format_duration(3_600_000), "1h 0m 0s");
    assert_eq!(format_duration(5_424_500), "1h 30m 24s");
}

#[test]
fn the_keys_that_belong_to_the_app_survive_the_sheet() {
    // The keys that read the conversation rather than answer the sheet
    // stay the app's, so a user can page for context with a question up.
    for code in [KeyCode::PageUp, KeyCode::PageDown, KeyCode::F(2)] {
        assert!(
            key_survives_the_sheet(KeyEvent::new(code, KeyModifiers::NONE)),
            "{code:?} reads the conversation, not the sheet"
        );
    }
    assert!(key_survives_the_sheet(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL
    )));
    for key in [
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
    ] {
        assert!(
            !key_survives_the_sheet(key),
            "{key:?} belongs to the sheet in front of the user"
        );
    }
}

#[test]
fn relativize_spells_home_as_tilde_and_deeper_paths_under_it() {
    // The `~/` shape: home itself is the bare tilde, a directory
    // under it keeps its tail, and a directory outside it has no
    // relative spelling at all — the caller shows the full path.
    let home = Path::new("/home/tester");
    assert_eq!(
        relativize(Path::new("/home/tester"), home).as_deref(),
        Some("~"),
        "the home directory itself is the bare tilde"
    );
    assert_eq!(
        relativize(Path::new("/home/tester/repos/dch"), home).as_deref(),
        Some("~/repos/dch"),
        "a directory under home keeps its tail under the tilde"
    );
    assert_eq!(
        relativize(Path::new("/srv/work"), home),
        None,
        "a directory outside home has no relative spelling"
    );
}

#[test]
fn an_expanded_preview_spends_only_the_rows_the_sheet_has() {
    // The budget is the sheet's inner height minus the prompt rows
    // drawn and the fixed rows owed (scopes, reason, spacer,
    // hints — each block measured at its wrapped length), floored
    // at one so a cramped pane still shows the preview's first
    // line, and capped at the standing ceiling so a tall pane does
    // not hand an enormous command the sheet.
    let hints = "y allows · n or esc denies · e expands";
    let blocks = [
        "a remembers: cargo build",
        "d remembers: rm",
        "reason: ship it▏",
    ];
    let fixed = expanded_fixed_rows(&blocks, hints, 80);
    // A tall sheet: everything fits, the ceiling binds.
    assert_eq!(
        expanded_command_budget(100, 3, fixed),
        MAX_EXPANDED_OVERLAY_ROWS,
        "a tall pane spends the standing ceiling, no more"
    );
    // A short sheet: inner height (less borders) minus prompt and
    // fixed rows is exactly the preview's budget — nothing more.
    let budget = expanded_command_budget(20, 3, fixed);
    assert_eq!(
        budget,
        usize::from(20u16.saturating_sub(2))
            .saturating_sub(3)
            .saturating_sub(fixed),
        "the short pane spends its remaining rows, no more"
    );
    assert_eq!(
        3 + fixed + budget,
        usize::from(20u16.saturating_sub(2)),
        "prompt, fixed rows, and preview fill the sheet exactly"
    );
    // A pane with nothing to spare: the floor still shows one row.
    assert_eq!(
        expanded_command_budget(6, 3, fixed),
        1,
        "a cramped pane still shows the preview's first line"
    );
}

#[test]
fn an_overflowing_preview_keeps_its_ellipsis_row_inside_the_budget() {
    // The wrapper's row ceiling includes its own ellipsis row: a
    // command whose wrap runs past the budget returns exactly the
    // budget's rows — the ellipsis and the budget's remaining tail
    // rows — so prompt, preview, and fixed rows fill the sheet
    // exactly and the final hint row is never clipped. A wrap that
    // fits the budget, exactly or with room, returns its own rows
    // unchanged.
    let wrap_at = 10;
    let budget = 3;
    let fits = "aa bb cc";
    assert_eq!(
        wrap_prompt_rows_last(fits, wrap_at, budget).len(),
        1,
        "a short command returns its own single row"
    );
    let exact = "aa bb cc dd ee ff gg hh ii";
    assert_eq!(
        wrap_prompt_rows_last(exact, wrap_at, budget).len(),
        budget,
        "a wrap that exactly spends the budget returns it whole"
    );
    let overflowing = "aa bb cc dd ee ff gg hh ii jj kk ll";
    let rows = wrap_prompt_rows_last(overflowing, wrap_at, budget);
    assert_eq!(
        rows.len(),
        budget,
        "the ellipsis row is counted inside the budget, never added on top of it"
    );
    assert_eq!(
        rows.first().map(String::as_str),
        Some("…"),
        "the cut is named where the reader's eye lands"
    );
    // The sheet-level composition the budget exists for: with the
    // prompt drawn and the fixed rows owed, an overflowing command
    // still leaves the sheet's total at or under its inner height.
    let hints = "y allows · n or esc denies · e expands";
    let blocks = ["a remembers: cargo build", "d remembers: rm"];
    let fixed = expanded_fixed_rows(&blocks, hints, 80);
    let sheet_prompt_rows = 3;
    let budget = expanded_command_budget(20, sheet_prompt_rows, fixed);
    let preview = wrap_prompt_rows_last(overflowing, 80, budget);
    assert!(
        sheet_prompt_rows + fixed + preview.len() <= usize::from(20u16.saturating_sub(2)),
        "prompt, fixed rows, and an overflowing preview stay inside the sheet's inner height"
    );
}

#[test]
fn multi_row_blocks_are_counted_at_their_wrapped_length() {
    // The fixed-rows measurement must count what the renderer
    // appends: a long scope on a narrow pane wraps to the overlay's
    // row ceiling, and a one-row-per-block guess would hand the
    // preview rows the sheet has already spent — pushing scope text
    // and hints past the sheet's bottom edge.
    let hints = "y allows · n or esc denies";
    let narrow = 20;
    let long_scope = "a remembers: cargo build --workspace --quiet · repeated again and again";
    let blocks = [long_scope, "d remembers: rm"];
    let wrapped_scope = wrap_prompt_rows(long_scope, narrow, MAX_OVERLAY_ROWS);
    assert!(
        wrapped_scope.len() > 1,
        "the fixture genuinely wraps — a one-row block could never pin the undercount"
    );
    let measured = expanded_fixed_rows(&blocks, hints, narrow);
    let by_hand = wrapped_scope.len()
        + wrap_prompt_rows("d remembers: rm", narrow, MAX_OVERLAY_ROWS).len()
        + 1
        + wrap_prompt_rows(hints, narrow, MAX_OVERLAY_ROWS).len();
    assert_eq!(
        measured, by_hand,
        "each block counts at the length its wrap will render at"
    );
}

#[test]
fn the_live_case_denies_only_what_was_refused_and_blessings_survive() {
    // The sheet-level pin on the narrow deny derivation: a session
    // that blessed
    // `cargo build` earlier, then pressed `d` on
    // `cargo build … | tail -30`, records nothing — the narrow
    // derivation mints only `cargo build`, and the memory skips
    // what the allow list already carries. The blessing survives;
    // no reader-deny rule is minted by association.
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    let memory = crate::queue::PermissionMemory::default();
    memory.remember_rules(&[PermissionRule::ShellPrefix {
        prefix: "cargo build".to_string(),
    }]);
    app.set_permission_memory(memory.clone());
    app.permission
        .pending_permissions
        .push_back(crate::permission::PermissionRequest {
            tool_name: "Bash".to_string(),
            prompt: "Allow 'Bash' (ShellExecute) in Interactive mode?".to_string(),
            input: serde_json::json!({
                "command": "cargo build --workspace --quiet 2>&1 | tail -30"
            }),
            always: vec![
                PermissionRule::ShellPrefix {
                    prefix: "cargo build".to_string(),
                },
                PermissionRule::ShellPrefix {
                    prefix: "tail".to_string(),
                },
            ],
            never: vec![PermissionRule::ShellPrefix {
                prefix: "cargo build".to_string(),
            }],
            reply: tokio::sync::oneshot::channel().0,
        });
    // An approval answers a rendered ask: the pin draws the sheet
    // once so the press finds it answerable.
    let _ = app.permission.shown_permission();
    app.permission
        .permission_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
    assert!(
        memory.deny_rules().is_empty(),
        "the refusal minted nothing — the verb was already blessed"
    );
    assert_eq!(
        memory.allow_rules(),
        vec![PermissionRule::ShellPrefix {
            prefix: "cargo build".to_string()
        }],
        "the blessing is sticky"
    );
}

#[test]
fn a_forget_matches_the_rule_it_rendered_not_the_position_it_landed_on() {
    // The listing's numbers are a snapshot: the memory can move
    // between the render and the press — a queued `/resume`'s
    // adopt replaces the pair wholesale — so the press removes
    // the rule the user read or nothing, never whatever now
    // occupies the position.
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.permission.permission_memory.adopt(
        vec![crate::queue::PermissionRule::ShellPrefix {
            prefix: "cargo test".to_string(),
        }],
        vec![],
        false,
    );
    let out = command_output(&mut app, "/permissions");
    assert!(
        out.contains("1. every future `cargo test` run"),
        "the render numbers the rule the user read: {out}"
    );
    // The adopt the resume rides in: the rendered rule is gone, a
    // different one holds the position.
    app.permission.permission_memory.adopt(
        vec![crate::queue::PermissionRule::ShellPrefix {
            prefix: "cargo build".to_string(),
        }],
        vec![],
        false,
    );
    let out = command_output(&mut app, "/permissions forget 1");
    assert!(
        out.contains("no longer remembered as rendered"),
        "the mismatch is named, not silently re-keyed: {out}"
    );
    assert_eq!(
        app.permission.permission_memory.allow_rules(),
        vec![crate::queue::PermissionRule::ShellPrefix {
            prefix: "cargo build".to_string(),
        }],
        "the rule holding the position survives the mismatched press"
    );
    // The happy twin: a fresh render re-keys the press, and the
    // forget removes exactly the rule it now names.
    let out = command_output(&mut app, "/permissions");
    assert!(
        out.contains("cargo build"),
        "the fresh render lists what is live: {out}"
    );
    let out = command_output(&mut app, "/permissions forget 1");
    assert!(
        out.contains("Forgot remembered permission"),
        "the re-keyed press removes what it rendered: {out}"
    );
    assert!(
        app.permission.permission_memory.allow_rules().is_empty(),
        "the rendered rule is out of the memory"
    );
}

#[test]
fn a_repair_under_standing_damage_reseeds_and_releases_the_adoption_guard() {
    // The repair's serialization pin: with the driver's adoption
    // guard installed the way production wires it, the recovery
    // runs its two steps — the verdict's clearing, the cell's
    // re-seed — under that guard and gives it back, so a
    // concurrent resume adoption cannot interleave between them
    // and a leaked guard cannot deadlock the next adoption.
    let mut config = dch_config::DchConfig::default();
    config.permissions.rules = vec![dch_config::PermissionRuleConfig {
        id: None,
        outcome: dch_config::PermissionRuleOutcome::Allow,
        tool: Some("Bash".to_string()),
        shell_prefix: None,
        path_glob: None,
    }];
    let mut app = TuiApp::new(config.clone());
    let memory = crate::queue::PermissionMemory::default();
    memory.mark_damaged();
    app.set_permission_memory(memory);
    let rules_cell = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    app.set_permission_config_rules_cell(Some(std::sync::Arc::clone(&rules_cell)));
    let adoption_guard = std::sync::Arc::new(std::sync::Mutex::new(()));
    app.set_permission_adoption_guard(Some(std::sync::Arc::clone(&adoption_guard)));

    // The user walks the door the notices name: the listing reviews
    // the standing damage, and the repair acts on that review.
    app.permissions_command(None);
    app.permissions_command(Some("repair"));

    let rules = rules_cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(
        *rules,
        config.permissions.engine_rules(),
        "the repair re-seeds the config layer at the configuration's own join"
    );
    drop(rules);
    assert!(
        adoption_guard.try_lock().is_ok(),
        "the repair releases the adoption guard — a leak would deadlock the next adoption"
    );
    assert!(
        !app.permission.permission_memory.damage_standing(),
        "the verdict cleared"
    );
}

#[test]
fn returning_to_the_bare_prefix_rederives_not_recycles() {
    // A cycle opened on a fragment freezes that fragment's filtered
    // list; deleting back to the bare prefix must re-derive the
    // whole list, not keep stepping the stale filter. Only a cycle
    // that opened bare holds the bare prefix.
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.handle_event(&crossterm::event::Event::Paste("/theme ligh".to_string()));
    let tab = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Tab,
        KeyModifiers::NONE,
    ));
    assert!(app.handle_event(&tab), "Tab steps the filtered cycle");
    let cycle = app
        .completion
        .completion
        .as_ref()
        .expect("the filtered cycle stands");
    assert_eq!(cycle.candidates.len(), 3, "ligh* filters three themes");
    assert!(!cycle.bare, "the cycle opened on a fragment");
    while app.composer.input.text() != "/theme " {
        app.handle_event(&crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        ));
    }
    assert!(app.handle_event(&tab), "Tab at the bare prefix re-derives");
    let cycle = app
        .completion
        .completion
        .as_ref()
        .expect("a fresh cycle stands");
    assert!(
        cycle.candidates.len() > 3,
        "the bare prefix cycles the whole palette, not the stale filter"
    );
    assert!(cycle.bare, "the fresh cycle opened bare");
}

#[test]
fn a_typed_fragment_tab_walks_only_its_filter() {
    // The user's exact flow, typed character by character: the
    // fragment's filtered list is what Tab walks, press after
    // press — never the whole palette. The stale-path guard: a
    // bare-prefix walk first (the whole list), then typing `ligh`
    // over it must retire that cycle and re-derive the filter.
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    for ch in "/theme ligh".chars() {
        app.handle_event(&crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
        ));
    }
    let tab = crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Tab,
        KeyModifiers::NONE,
    ));
    for _ in 0..5 {
        assert!(app.handle_event(&tab), "Tab always answers");
        let cycle = app.completion.completion.as_ref().expect("a cycle stands");
        assert_eq!(
            cycle.candidates.len(),
            3,
            "the walk stays on the filtered list: {:?}",
            cycle.candidates
        );
        assert!(!cycle.bare, "the walk never reopens bare");
    }

    // The stale path: walk the bare prefix's whole list, then
    // build the fragment by hand over it — the bare cycle must
    // retire the moment the composer spells something it never
    // applied, and Tab walks the fragment's filter instead.
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    for ch in "/theme ".chars() {
        app.handle_event(&crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
        ));
    }
    assert!(app.handle_event(&tab), "the bare walk opens");
    while app.composer.input.text() != "/theme " {
        app.handle_event(&crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        ));
    }
    for ch in "ligh".chars() {
        app.handle_event(&crossterm::event::Event::Key(
            crossterm::event::KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
        ));
    }
    assert!(app.handle_event(&tab), "Tab answers the fragment");
    let cycle = app
        .completion
        .completion
        .as_ref()
        .expect("a fresh cycle stands");
    assert_eq!(
        cycle.candidates.len(),
        3,
        "typing over a bare walk re-derives the filter: {:?}",
        cycle.candidates
    );
}

#[test]
fn a_committed_switch_defers_its_terminal_writes_to_the_flush() {
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    for command in ["/theme nord", "/theme", "/set theme nord"] {
        app.browse.pending_theme_osc = None;
        command_output(&mut app, command);
        let (background, primary) = app
            .browse
            .pending_theme_osc
            .expect("{command} parks its terminal writes on the flush");
        assert_eq!(
            background, app.theme.ui.background,
            "{command} parks the landed palette's canvas, not a key-time leftover"
        );
        assert_eq!(
            primary, app.theme.ui.primary,
            "{command} parks the cursor tint beside the canvas it belongs to"
        );
    }
}

#[test]
fn a_preview_parks_its_canvas_and_a_dismissed_browse_re_arms_the_restore() {
    // The browse preview's terminal half: the palette swap parks the
    // previewed canvas and tint on the flush — the slot a commit parks
    // into — and the browse's dismissal parks the restored palette's
    // pair the same way, so the terminal follows the preview and comes
    // back with it. A direct seat without a proven canvas keeps the
    // deferred inks; the parked canvas is still the previewed palette's
    // own, which the flush writes only on a terminal that took one.
    let _verdict = crate::terminal::MULTIPLEXER_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    crate::terminal::set_multiplexed_for_tests(Some(false));
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    let before = app.theme.clone();
    app.preview_theme(Theme::by_name("nord").expect("nord resolves"));
    let previewed = app.theme.clone();
    assert_ne!(
        previewed.ui.background, before.ui.background,
        "fixture premise: the preview moved the canvas"
    );
    assert_eq!(
        app.browse.pending_theme_osc,
        Some((previewed.ui.background, previewed.ui.primary)),
        "the preview parks the previewed canvas and tint for the flush"
    );
    app.composer.input.set_text("/theme ".to_string());
    assert!(
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        "Esc dismisses the standing browse"
    );
    assert_eq!(
        app.theme.name, before.name,
        "the dismissed browse restores the session's theme"
    );
    assert_eq!(
        app.browse.pending_theme_osc,
        Some((before.ui.background, before.ui.primary)),
        "the dismissal re-arms the restored palette's canvas and tint"
    );
    crate::terminal::set_multiplexed_for_tests(None);
}

/// Serializes the tests that read the process-wide window balance.
///
/// The balance is one counter for the whole test binary, so a test
/// snapshotting it — or sampling it from inside a failing backend —
/// holds this lock for its whole body; without it a parallel test's
/// markers could move the counter between the snapshots these pins
/// compare.
static WINDOW_BALANCE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn the_startup_fallback_restores_without_opening_a_burst() {
    // The branch a host's struct-literal theme reaches: the retry's
    // landing cannot un-mask a palette the registry does not carry, so
    // the terminal gets its own background back through the plain
    // captured restore — never a synchronized-output close the
    // fallback did not open, which would leave the markers unpaired.
    let _balance = WINDOW_BALANCE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    let mut literal = Theme::by_name("nord").expect("nord resolves");
    literal.name = "host literal";
    app.theme = literal;
    let before =
        crate::terminal::SYNCHRONIZED_WINDOW_BALANCE.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        !app.land_startup_canvas(),
        "an unregistered name cannot un-mask the palette"
    );
    assert_eq!(
        app.theme.name, "host literal",
        "the deferred palette stays as the host installed it"
    );
    assert_eq!(
        crate::terminal::SYNCHRONIZED_WINDOW_BALANCE.load(std::sync::atomic::Ordering::Relaxed),
        before,
        "the fallback opens and closes no synchronized window of its own"
    );
}

/// A backend whose frame writes always fail — the only arm a dead or
/// closing terminal takes, which [`ratatui::backend::TestBackend`]
/// cannot stand in for.
struct FailingBackend {
    /// The process-wide window balance as the failing draw saw it.
    ///
    /// Sampled from inside `draw`, between the burst's opener and
    /// its close, so the owning test can tell an open window from an
    /// absent pair of markers — the before-and-after balance alone
    /// cannot.
    sampled: std::sync::Arc<std::sync::atomic::AtomicIsize>,
}

impl ratatui::backend::Backend for FailingBackend {
    fn draw<'a, I>(&mut self, _content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.sampled.store(
            crate::terminal::SYNCHRONIZED_WINDOW_BALANCE.load(std::sync::atomic::Ordering::Relaxed),
            std::sync::atomic::Ordering::Relaxed,
        );
        Err(std::io::Error::other("the frame write failed"))
    }

    fn hide_cursor(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn show_cursor(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
        Ok(ratatui::layout::Position::ORIGIN)
    }

    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        _position: P,
    ) -> std::io::Result<()> {
        Ok(())
    }

    fn clear(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        Ok(ratatui::layout::Size {
            width: 80,
            height: 24,
        })
    }

    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
        Ok(ratatui::backend::WindowSize {
            columns_rows: ratatui::layout::Size {
                width: 80,
                height: 24,
            },
            pixels: ratatui::layout::Size {
                width: 0,
                height: 0,
            },
        })
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_failed_draw_closes_the_parked_bursts_window() {
    // The burst's window opens before the frame's bytes; a draw that
    // fails mid-frame still owes the close, or the restore bytes the
    // exit guard writes next land inside a window a mode-2026
    // terminal keeps buffering and never paints. One test walks both
    // arms in sequence under the balance lock — the counter is
    // process-wide, so a parallel test's markers could otherwise move
    // it between these snapshots.
    let _balance = WINDOW_BALANCE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let sampled = std::sync::Arc::new(std::sync::atomic::AtomicIsize::new(0));
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    let mut terminal = ratatui::Terminal::new(FailingBackend {
        sampled: std::sync::Arc::clone(&sampled),
    })
    .expect("terminal");
    let before =
        crate::terminal::SYNCHRONIZED_WINDOW_BALANCE.load(std::sync::atomic::Ordering::Relaxed);
    let drawn = app.draw_frame(
        &mut terminal,
        Some((
            ratatui::style::Color::Rgb(12, 12, 12),
            ratatui::style::Color::Rgb(255, 178, 0),
        )),
    );
    assert!(
        drawn.is_err(),
        "the draw failure propagates to the run loop"
    );
    assert!(
        sampled.load(std::sync::atomic::Ordering::Relaxed) > before,
        "the failing draw ran inside the window the burst opened"
    );
    assert_eq!(
        crate::terminal::SYNCHRONIZED_WINDOW_BALANCE.load(std::sync::atomic::Ordering::Relaxed),
        before,
        "the window the burst opened, the failed draw still closed"
    );
    assert!(
        app.draw_frame(&mut terminal, None).is_err(),
        "the draw failure propagates without a burst"
    );
    assert_eq!(
        sampled.load(std::sync::atomic::Ordering::Relaxed),
        before,
        "no burst stood, so the failing draw ran between no window markers"
    );
    assert_eq!(
        crate::terminal::SYNCHRONIZED_WINDOW_BALANCE.load(std::sync::atomic::Ordering::Relaxed),
        before,
        "no burst stood, so the failure path opens no window"
    );
}

#[test]
fn tab_completes_a_command_name_from_its_prefix() {
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.composer.input.set_text("/them".to_string());
    assert!(app.complete_command_line(false));
    assert!(
        app.composer.input.text() == "/theme" || app.composer.input.text() == "/themes",
        "a command prefix completes to a full command name, got {:?}",
        app.composer.input.text()
    );
}

#[test]
fn tab_completes_past_a_multibyte_separator() {
    // A nonbreaking space separates the command word from its argument
    // as well as an ASCII one does. The prefix must keep the whole
    // separator: slicing at its first byte cut inside the character,
    // dropped the line back to a bare "/", and let the completion
    // replace the command word with the value it offered.
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.composer.input.set_text("/theme\u{a0}dark".to_string());
    assert!(app.complete_command_line(false));
    assert_eq!(
        app.composer.input.text(),
        "/theme\u{a0}dark_plus",
        "the multibyte separator survives into the completed line"
    );
}

#[test]
fn a_step_past_a_stale_cycle_position_still_lands_on_a_row() {
    // A paste can replace the composer under a standing cycle with a
    // fragment whose filtered list is shorter than the position the
    // walk stood on. The step clamps the stale position into the
    // fresh list instead of computing an index that no longer exists
    // — the shape that consumed the key and left the composer
    // untouched.
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.composer.input.set_text("/theme ".to_string());
    assert!(app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)));
    app.composer.input.set_text("/theme d".to_string());
    let (_, fresh, _) = app.fresh_completion("/theme d");
    let clamped_step = fresh
        .get(fresh.len().saturating_sub(2))
        .cloned()
        .unwrap_or_default();
    assert!(
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        "the step lands even from a position the fresh list outgrew"
    );
    assert_eq!(
        app.composer.input.text(),
        format!("/theme {clamped_step}"),
        "the clamped position steps within the fresh list"
    );
}

#[test]
fn tab_cycles_the_suggestions_with_the_popup_standing() {
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.composer.input.set_text("/theme ".to_string());
    let (_, candidates, _) = app.fresh_completion("/theme ");
    assert!(
        candidates.len() >= 2,
        "the probe stem offers at least two themes, got {candidates:?}"
    );
    let first = candidates.first().cloned().unwrap_or_default();
    let second = candidates.get(1).cloned().unwrap_or_default();
    let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
    assert!(app.handle_key(tab));
    assert_eq!(
        app.composer.input.text(),
        format!("/theme {first}"),
        "the first Tab applies the highlighted row — the composer names what the screen previews"
    );
    assert_eq!(
        app.theme.name,
        Theme::by_name(&first).map_or_default(|theme| theme.name),
        "the palette previews the row the first Tab applied"
    );
    assert!(
        app.completion_popup_live(),
        "the popup stands after the applied row"
    );
    assert!(app.handle_key(tab));
    assert_eq!(
        app.composer.input.text(),
        format!("/theme {second}"),
        "the second Tab steps to the second row — the same row a Down lands on"
    );
    // After applying, the popup stands — the cycle's frozen list
    // keeps the candidates visible so the next Tab (or the arrows)
    // continues cycling through them.
    assert!(
        app.completion_popup_live(),
        "the popup stands after an applied candidate — Tab cycles, it does not select"
    );
    // A third Tab steps onward to the third row.
    assert!(app.handle_key(tab));
    assert_eq!(
        app.composer.input.text(),
        format!("/theme {}", candidates.get(2).cloned().unwrap_or_default()),
        "the third Tab steps to the third row — cycling continues"
    );
}

#[test]
fn backtab_from_a_bare_stem_applies_the_last_row() {
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.composer.input.set_text("/theme ".to_string());
    let (_, candidates, _) = app.fresh_completion("/theme ");
    assert!(candidates.len() >= 2, "themes offer more than one");
    let last = candidates.last().cloned().unwrap_or_default();
    assert!(app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)));
    assert_eq!(
        app.composer.input.text(),
        format!("/theme {last}"),
        "a reverse first press applies the last row, not a browse"
    );
    assert!(
        app.completion_popup_live(),
        "the popup stands after the applied row"
    );
}

#[test]
fn backtab_over_a_stale_cycle_applies_the_last_row() {
    // A cycle left standing by a mutation that is not a keystroke is
    // stale, not holding the line: the first reverse press treats it
    // as no cycle at all and lands on the last row — the same
    // contract the clean-path pin states for the bare stem.
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.composer.input.set_text("/theme ".to_string());
    let (_, candidates, _) = app.fresh_completion("/theme ");
    assert!(candidates.len() >= 2, "themes offer more than one");
    let first = candidates.first().cloned().unwrap_or_default();
    let last = candidates.last().cloned().unwrap_or_default();
    assert!(app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
    assert_eq!(
        app.composer.input.text(),
        format!("/theme {first}"),
        "fixture premise: the forward press applied the first row"
    );
    // A paste-shaped mutation — no key wrapper runs — leaves the
    // cycle parked against a line it no longer holds.
    app.composer.input.set_text("/theme ".to_string());
    assert!(app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)));
    assert_eq!(
        app.composer.input.text(),
        format!("/theme {last}"),
        "the stale cycle is treated as none: the first reverse press lands on the last row"
    );
}

#[test]
fn a_paste_that_drifts_the_browse_retires_the_preview() {
    // Paste is the one composer-mutating entry point the key wrapper
    // never sees: a '/theme' browse the pasted text drifts away from
    // must retire on the paste itself, or the screen keeps the
    // previewed palette until some later key press spends the
    // restore.
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    let original = app.theme.clone();
    app.composer.input.set_text("/theme ".to_string());
    assert!(app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
    assert!(
        app.browse.theme_before_browse.is_some(),
        "fixture premise: the browse parked the session's own palette"
    );
    assert_ne!(
        app.theme.name, original.name,
        "fixture premise: the landed row previewed another palette"
    );
    assert!(app.handle_event(&crossterm::event::Event::Paste("x".to_string())));
    assert!(
        app.browse.theme_before_browse.is_none(),
        "the drifted browse retired on the paste itself"
    );
    assert_eq!(
        app.theme.name, original.name,
        "the session's own palette came back with the retirement"
    );
}

#[test]
fn the_cancel_arm_dies_with_the_run_it_was_armed_against() {
    // The queued-claim window: the driver ends one run and starts the
    // next with no yield between the two flag stores, so the running
    // flag is back up before any drain could read it down. The run's
    // end rides the graduation the drain processes regardless of the
    // flag, and the arm retires there — the next run's first Esc arms
    // afresh instead of firing a cancel one press old.
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    let fired = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let count = std::rc::Rc::clone(&fired);
    app.set_run_canceller(Box::new(move || count.set(count.get().saturating_add(1))));
    app.state
        .agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert!(
        app.exit.cancel_arm,
        "fixture premise: the first Esc armed the ladder"
    );
    // Run A ends and B starts back to back: the end rides the
    // graduation lane while the running flag never falls.
    app.state
        .graduations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(crate::observer::Graduation::RunEnded {
            duration_ms: 1,
            success: true,
        });
    app.drain_shared_state();
    assert!(
        !app.exit.cancel_arm,
        "the arm retired with the run whose end the drain processed"
    );
    assert_eq!(fired.get(), 0, "no cancel fired on the way out");
    assert!(app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert_eq!(
        fired.get(),
        0,
        "the next run's first Esc arms, never fires a cancel one press old"
    );
    assert!(
        app.exit.cancel_arm,
        "it arms the ladder afresh against the run in flight"
    );
    assert!(app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert_eq!(fired.get(), 1, "and the second press is the cancel");
}

#[test]
fn any_mouse_press_stands_the_quit_chord_down() {
    // The chord must never fire from stale intent: a right-button
    // press between the presses disarms it the same way a left press
    // or a key does.
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.quit.arm();
    app.handle_mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Right),
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    });
    assert!(!app.quit.armed, "the right press spent the arm");
}

#[test]
fn a_render_invalidation_resets_the_frozen_stream_whole() {
    // The freeze's cached half: rows frozen under the palette that
    // stood when they settled. A render invalidation drops the frozen
    // prefix with the bake — or the cached half would keep serving
    // the old colors until the stream's next delta or its graduation.
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    *app.state
        .streaming_text
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        "settled block one\n\nlive tail".to_string();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    assert!(
        app.freeze.frozen_upto > 0,
        "fixture premise: a block settled into the freeze"
    );
    app.invalidate_conversation_renders();
    assert!(
        app.freeze.frozen_lines.is_empty() && app.freeze.frozen_upto == 0,
        "the invalidation drops the frozen prefix with the bake"
    );
}

#[test]
fn enter_over_a_bare_theme_browse_names_the_highlighted_row() {
    // The browse's commit path: Enter over a bare '/theme ' stem —
    // the popup standing, a row highlighted by the walk — names that
    // row in the composer and submits it, so the session lands on
    // the browsed choice rather than whatever the draft spelled.
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    let original = app.theme.clone();
    app.composer.input.set_text("/theme ".to_string());
    let (_, candidates, _) = app.fresh_completion("/theme ");
    let second = candidates.get(1).cloned().unwrap_or_default();
    assert!(
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        "the walk applies and previews the landed row"
    );
    assert_ne!(
        app.theme.name, original.name,
        "fixture premise: the walk previewed another palette"
    );
    // Back to the bare stem the Enter arm reads, browse still standing.
    app.composer.input.set_text("/theme ".to_string());
    assert!(app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert_eq!(
        app.theme.name,
        Theme::by_name(&second).map_or(second.clone(), |theme| theme.name.to_string()),
        "the highlighted row's palette is the one the session committed"
    );
    assert!(
        app.browse.theme_before_browse.is_none(),
        "the commit retired the browse"
    );
}

#[test]
fn a_stood_down_strip_erases_the_row_it_last_painted() {
    // The strip's exit discipline: a frame that paints nothing must
    // still spend the frame clearing its row, because the frame
    // buffer is cumulative — without the clear the last painted bar
    // lingers into the frames that follow. The glow style paints one
    // unambiguous full-width rule, so its row's fate is the pin. The
    // dedicated first row is the honest seat for it: at the default
    // placement the composer's hairline repaints the spacer the moment
    // the strip stands down, which would mask a lingering bar, while
    // nothing else paints the top row.
    let config = dch_config::DchConfig {
        display: dch_config::DisplayConfig {
            strip_style: StripStyle::Glow,
            strip_position: StripPosition::Top,
            ..dch_config::DisplayConfig::default()
        },
        ..dch_config::DchConfig::default()
    };
    let mut app = TuiApp::new(config);
    app.state
        .agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let strip_row = (0..buffer.area().height).find(|row| {
        let cells: Vec<&str> = (0..buffer.area().width)
            .filter_map(|column| {
                buffer
                    .cell(ratatui::layout::Position::new(column, *row))
                    .map(ratatui::buffer::Cell::symbol)
            })
            .collect();
        cells.len() == usize::from(buffer.area().width)
            && cells
                .iter()
                .all(|symbol| *symbol == "\u{2500}" || *symbol == " ")
            && cells.iter().filter(|symbol| **symbol == "\u{2500}").count() >= 40
    });
    let Some(row) = strip_row else {
        panic!("the running strip painted its bar");
    };
    app.state
        .agent_running
        .store(false, std::sync::atomic::Ordering::SeqCst);
    terminal.draw(|frame| app.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let cleared = (0..buffer.area().width).all(|column| {
        buffer
            .cell(ratatui::layout::Position::new(column, row))
            .is_some_and(|cell| cell.symbol() == " ")
    });
    assert!(
        cleared,
        "the stood-down strip erased the bar its last painted frame left"
    );
}

#[test]
fn arrows_walk_the_suggestion_popup_and_preview_the_landed_row() {
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.composer.input.set_text("/theme ".to_string());
    let (_, candidates, _) = app.fresh_completion("/theme ");
    assert!(
        candidates.len() >= 2,
        "the probe stem offers at least two themes, got {candidates:?}"
    );
    let second = candidates.get(1).cloned().unwrap_or_default();
    assert!(
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        "Down with the popup standing is the popup's"
    );
    assert_eq!(
        app.composer.input.text(),
        format!("/theme {second}"),
        "the composer takes the landed row — selection, preview, and text agree"
    );
    assert_eq!(
        app.theme.name,
        Theme::by_name(&second).map_or_default(|theme| theme.name),
        "the palette previews the landed row"
    );
    assert!(
        app.completion_popup_live(),
        "the popup stands while the arrows browse"
    );
}

#[test]
fn tab_on_a_fully_typed_command_applies_the_first_argument() {
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.composer.input.set_text("/theme".to_string());
    let (_, candidates, _) = app.fresh_completion("/theme ");
    let first = candidates.first().cloned().unwrap_or_default();
    assert!(app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
    assert_eq!(
        app.composer.input.text(),
        format!("/theme {first}"),
        "Tab on a fully typed command lands on the first value — space added, row applied"
    );
    assert!(
        app.completion_popup_live(),
        "the popup stands with the argument candidates"
    );
    assert_eq!(
        app.theme.name,
        Theme::by_name(&first).map_or_default(|theme| theme.name),
        "the palette previews the row the composer names"
    );
}

#[test]
fn a_committed_switch_after_browsing_keeps_the_choice() {
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.composer.input.set_text("/theme ".to_string());
    assert!(app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
    // Type the rest of a theme name — the browse narrows, not ends.
    for glyph in "nord".chars() {
        assert!(app.handle_key(KeyEvent::new(KeyCode::Char(glyph), KeyModifiers::NONE)));
    }
    // The text still names a theme; the browse hasn't ended.
    assert_eq!(
        app.composer.input.text(),
        "/theme nord",
        "the typed name stands"
    );
    // Enter commits — the choice keeps, no restore.
    assert!(app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert_eq!(
        app.theme.name, "Nord",
        "the committed switch stands — the browse's restore does not undo it"
    );
}

#[test]
fn arrows_preview_and_tab_applies_from_the_arrows_row() {
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.composer.input.set_text("/theme ".to_string());
    let (_, candidates, _) = app.fresh_completion("/theme ");
    assert!(
        candidates.len() >= 3,
        "the themes offer at least three rows, got {candidates:?}"
    );
    let third = candidates.get(2).cloned().unwrap_or_default();
    // Two arrow presses walk to the third row; the palette and the
    // composer both follow.
    assert!(app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
    assert!(app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
    assert_eq!(
        app.composer.input.text(),
        format!("/theme {third}"),
        "the composer names the arrow-landed row — Enter submits the choice on screen"
    );
    assert_eq!(
        app.theme.name,
        Theme::by_name(&third).map_or_default(|theme| theme.name),
        "the preview follows the arrow-landed row"
    );
    // Tab continues from the arrow's position — the shared cycle —
    // and applies the next row into the composer.
    assert!(app.complete_command_line(false));
    assert_eq!(
        app.composer.input.text(),
        format!("/theme {}", candidates.get(3).cloned().unwrap_or_default()),
        "Tab continues from the row the arrows landed on — one position, two keys"
    );
}

#[test]
fn the_popup_arrows_wrap_at_both_ends() {
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.composer.input.set_text("/theme ".to_string());
    let (_, candidates, _) = app.fresh_completion("/theme ");
    let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
    let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    let last = candidates.last().cloned().unwrap_or_default();
    let first = candidates.first().cloned().unwrap_or_default();
    assert!(app.handle_key(up));
    assert_eq!(
        app.theme.name,
        Theme::by_name(&last).map_or_default(|theme| theme.name),
        "Up from the fresh highlight wraps to the last row"
    );
    assert_eq!(
        app.composer.input.text(),
        format!("/theme {last}"),
        "the composer takes the wrapped row"
    );
    assert!(app.handle_key(up));
    assert!(app.handle_key(down));
    assert_eq!(
        app.theme.name,
        Theme::by_name(&last).map_or_default(|theme| theme.name),
        "Up then Down returns to the last row (Up wrapped, Down stepped back)"
    );
    assert!(
        app.completion_popup_live(),
        "the popup stands through the walk"
    );
    let _ = first;
}

#[test]
fn tab_continues_from_the_row_the_arrows_landed_on() {
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.composer.input.set_text("/theme ".to_string());
    let (_, candidates, _) = app.fresh_completion("/theme ");
    assert!(
        candidates.len() >= 3,
        "the themes offer at least three rows, got {candidates:?}"
    );
    let second = candidates.get(1).cloned().unwrap_or_default();
    let third = candidates.get(2).cloned().unwrap_or_default();
    assert!(app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
    assert_eq!(
        app.theme.name,
        Theme::by_name(&second).map_or_default(|theme| theme.name),
        "the arrow lands on the second row — the preview proves the position"
    );
    assert!(app.complete_command_line(false));
    assert_eq!(
        app.composer.input.text(),
        format!("/theme {third}"),
        "Tab continues from the arrow-landed row, applying the third"
    );
}

#[test]
fn an_edit_after_stepping_keeps_the_cycles_position() {
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.composer.input.set_text("/theme ".to_string());
    assert!(app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
    // Type after the browse: the text narrows, the position persists.
    app.composer.input.set_text("/theme d".to_string());
    assert!(
        app.completion_popup_live(),
        "the popup re-arms for the edited draft"
    );
    let (_, candidates, _) = app.fresh_completion("/theme d");
    // The cycle was at index 1 (from the first arrow); the next
    // arrow continues from there, stepping to index 2 of the
    // fresh "d"-filtered list.
    let third = candidates.get(2).cloned().unwrap_or_default();
    assert!(app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
    assert_eq!(
        app.theme.name,
        Theme::by_name(&third).map_or_default(|theme| theme.name),
        "the step continues from the cycle's position in the fresh list"
    );
}

#[test]
fn a_value_row_in_the_popup_stands_alone() {
    let mut subject = TuiApp::new(dch_config::DchConfig::default());
    subject.composer.input.set_text("/theme ".to_string());
    let rows = screen_rows(&mut subject);
    let (_, candidates, _) = subject.fresh_completion("/theme ");
    let value = candidates.first().cloned().unwrap_or_default();
    let row = rows
        .iter()
        .find(|row| row.trim_start().starts_with(&value))
        .unwrap_or_else(|| panic!("the popup lists {value}, rows: {rows:?}"));
    let summary = commands::summary_for("theme").unwrap_or_default();
    assert!(
        !row.contains(summary),
        "a theme row is the value alone, not the /theme summary, got {row:?}"
    );
}

#[test]
fn a_wrong_shape_known_command_points_at_the_usage() {
    // /model with no name is a usage mistake, not an unknown
    // command: the notice carries the shape the registry spells,
    // never the command-list pointer a name nothing carries earns.
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.handle_command(commands::parse("/model"));
    let (notice, _) = app.transient_notice.as_ref().expect("a notice stands");
    assert!(
        notice.contains("/model <name>"),
        "the notice names the shape: {notice}"
    );
    assert!(
        !notice.contains("unknown command"),
        "a registered command is never called unknown: {notice}"
    );
}

#[test]
fn fit_picker_title_pads_and_truncates_by_display_cells() {
    // The picker's title column is a cell budget: a wide-script
    // title pads to the same column edge an ASCII one does, and
    // an over-long one keeps a head that fits the budget with the
    // ellipsis inside it. A character count would let a wide
    // title push the model column sideways.
    let padded = fit_picker_title(Some("端末のセッション"));
    assert_eq!(
        str_width(&padded),
        24,
        "a wide-script title pads to the column's cell width: {padded:?}"
    );
    let over_long = "題".repeat(40);
    let capped = fit_picker_title(Some(&over_long));
    assert!(
        str_width(&capped) <= 24,
        "the cap is a cell budget, never a character count: {capped:?}"
    );
    assert!(
        capped.contains('…'),
        "the cap keeps its head visible: {capped:?}"
    );
    assert!(
        str_width(&capped) == 24,
        "a truncated title pads to the same column edge as a short one: {capped:?} is {} cells",
        str_width(&capped)
    );
    let joined = format!(
        "前\u{2714}\u{fe0f}\u{1f468}\u{200d}\u{1f469}{}",
        "題".repeat(30)
    );
    let policy_capped = fit_picker_title(Some(&joined));
    assert!(
        str_width(&policy_capped) <= 24,
        "a joiner cluster spends what it paints: {policy_capped:?}"
    );
    assert!(
        !policy_capped.contains('\u{fe0f}') && !policy_capped.contains('\u{200d}'),
        "the title carries the display policy before either measure: {policy_capped:?}"
    );
    assert_eq!(
        str_width(&fit_picker_title(None)),
        24,
        "the untitled dash pads to the same column edge"
    );
}

#[test]
fn a_settled_expansion_folds_rebake_off_the_baked_rows() {
    // The same retirement from the frame's head: a tool starting to
    // run folds the plan block, and the rows baked in its expanded
    // shape go with the flag rather than outliving it.
    let mut subject = app();
    let receipt = [
        "Changed: src/lib.rs (modified)",
        "│ 1 line removed, 1 added",
        "@@ -1,2 +1,2 @@",
        "  fn main() {",
        "-     old();",
        "+     new();",
        "  }",
    ]
    .join("\n");
    subject.retain_tool_detail("call_todo", "{\"k\":1}", &receipt);
    subject.push_message(TuiMessage::Assistant {
        blocks: vec![ContentBlock::Tool {
            name: "TodoWrite".to_string(),
            call_id: "call_todo".to_string(),
            input_preview: "src/lib.rs".to_string(),
            output_preview: String::new(),
            retained_input: String::new(),
            success: true,
            elapsed_secs: 0.1,
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    });
    subject.push_message(TuiMessage::User {
        text: "after the block".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
        unsent: false,
    });
    subject
        .expansion
        .expanded_tools
        .insert("call_todo".to_string());
    subject.expansion.auto_expanded_todo = Some("call_todo".to_string());
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).expect("terminal");
    terminal
        .draw(|frame| subject.render(frame))
        .expect("first draw");
    let expanded = buffer_text(terminal.backend().buffer());
    assert!(
        expanded.contains("old();"),
        "the armed block draws its expanded detail: {expanded}"
    );
    subject
        .active_tools()
        .lock()
        .expect("the tools lock")
        .push(ActiveTool {
            call_id: "call_next".to_string(),
            name: "Bash".to_string(),
            input_summary: String::new(),
            start: Instant::now(),
        });
    subject.settle_todo_expansion();
    assert!(
        !subject.expansion.expanded_tools.contains("call_todo"),
        "the tool starting to run folds it"
    );
    terminal
        .draw(|frame| subject.render(frame))
        .expect("redraw");
    let collapsed = buffer_text(terminal.backend().buffer());
    assert!(
        !collapsed.contains("old();"),
        "the fold rebakes — the baked detail rows do not outlive the \
             flag they rode: {collapsed}"
    );
    assert!(
        collapsed.contains("src/lib.rs"),
        "the block's own summary row stands: {collapsed}"
    );
}

#[test]
fn the_flipped_highlight_covers_exactly_the_swept_cells() {
    // reverse_selection walks the same projection the copy walks:
    // the reversed ink spans exactly the swept columns — four
    // cells for a four-cell sweep — whatever the raw bytes
    // underneath measure.
    let mut lines = vec![Line::from("done \u{2714}\u{fe0f} shipped")];
    reverse_selection(
        &mut lines,
        0,
        &CellSelection {
            anchor: CellPos { line: 0, col: 5 },
            head: CellPos { line: 0, col: 8 },
        },
    );
    let line = lines.first().expect("the fixture line survives the flip");
    let flipped: Vec<&ratatui::text::Span<'_>> = line
        .spans
        .iter()
        .filter(|span| {
            span.style
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED)
        })
        .collect();
    let ink: String = flipped
        .iter()
        .map(|span| span.content.to_string())
        .collect();
    assert_eq!(ink, "\u{2714}\u{fe0f} sh");
    let cells: usize = flipped
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum();
    assert_eq!(
        cells, 4,
        "the flipped ink paints the four swept cells and no more"
    );
}

#[test]
fn the_identity_row_picker_and_completion_paint_the_stripped_projection() {
    // Three surfaces on one welcome draw: the composer
    // identity row (a free-form config model rides it), the
    // session picker's model column, and the completion popup's
    // candidate values. All paint the projection; the picker's
    // wide columns also pad by cells, so a CJK workdir stops its
    // pad at its own painted width and the row's trailing
    // columns stay on their grid.
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
    let mut config = dch_config::DchConfig::default();
    config.api.model = family.to_string();
    let mut subject = TuiApp::new(config);
    // A standing frame, not the welcome: the splash box bounds
    // the popup at the art's width there, and the honest row
    // clips at that bound whatever the pad discipline — here the
    // composer spans the pane and the row fits whole.
    subject.push_message(TuiMessage::Assistant {
        blocks: vec![ContentBlock::Text {
            text: "hello".to_string(),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: Some(10),
    });
    subject.apply_session_update(SessionUpdate::SessionCatalog {
        rows: vec![SessionPick {
            id: "11111111-1111-1111-1111-111111111111".to_string(),
            model: family.to_string(),
            saved_at: chrono::Utc::now(),
            prompts: 3,
            title: None,
            workdir: Some(format!("/x/{}", "长".repeat(10))),
        }],
    });
    subject.completion.completion = Some(CompletionCycle {
        prefix: "/model ".to_string(),
        candidates: vec![family.to_string()],
        index: 0,
        bare: true,
        window_start: 0,
    });
    // A 120-column pane: the picker's widest honest row (24-cell
    // title, 20-cell dir, 12-cell model) fits whole; at 80 the
    // row clips at the pane regardless of the pad discipline.
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
    terminal.draw(|frame| subject.render(frame)).unwrap();
    for cell in terminal.backend().buffer().content.clone() {
        let symbol = cell.symbol();
        let carries_input = symbol.chars().any(|ch| {
            matches!(
                ch,
                '\u{fe0f}' | '\u{fe0e}'
                    | '\u{200d}'
                    | '\u{20e3}'
                    | '\u{1f3fb}'..='\u{1f3ff}'
                    | '\u{e0020}'..='\u{e007f}'
            )
        });
        assert!(
            !carries_input,
            "the composition input {symbol:?} reached the identity, picker, or completion rows"
        );
    }
    let screen = buffer_text(terminal.backend().buffer());
    assert!(
        screen.contains('\u{1f468}'),
        "the family's members paint stripped, not vanished: {screen:?}"
    );
    assert!(
        screen.contains("3p"),
        "the picker's trailing columns stay on the grid: {screen:?}"
    );
}

#[test]
fn a_seam_cluster_keeps_its_columns_before_and_during_selection() {
    // The frame paints in one order — the flip on the raw clone,
    // then the display strip (paint/conversation.rs) — and a
    // cluster split across spans must sit on the same cells in
    // both states. The screen column here is measured the way the
    // buffer advances: per-span widths of the stripped contents.
    // A strip that projected each span alone kept a span-opening
    // modifier as its own two cells until a selection covering
    // the cluster flipped it into one span — and only then
    // stripped it — so the text after the seam jumped two columns
    // the moment the selection landed, and the unselected row
    // never matched the cells the selection math budgeted.
    fn strip(lines: &mut Vec<Line<'_>>) {
        super::paint::conversation::for_display_lines(lines);
    }
    fn screen_column(lines: &[Line<'_>], needle: char) -> usize {
        let line = lines.first().expect("the fixture line survives the strip");
        let mut column: usize = 0;
        for span in &line.spans {
            let content = span.content.as_ref();
            if let Some(at) = content.find(needle) {
                let prefix = content.get(..at).expect("find lands on a boundary");
                return column.saturating_add(str_width(prefix));
            }
            column = column.saturating_add(str_width(content));
        }
        panic!("the needle never paints");
    }
    fn screen_width(lines: &[Line<'_>]) -> usize {
        lines
            .first()
            .expect("the fixture line survives the strip")
            .spans
            .iter()
            .map(|span| str_width(span.content.as_ref()))
            .sum()
    }
    let seam = || {
        vec![Line::from(vec![
            Span::raw("a\u{1f44d}"),
            Span::raw("\u{1f3fd} b"),
        ])]
    };

    let mut before = seam();
    strip(&mut before);
    let mut during = seam();
    reverse_selection(
        &mut during,
        0,
        &CellSelection {
            anchor: CellPos { line: 0, col: 0 },
            head: CellPos { line: 0, col: 3 },
        },
    );
    strip(&mut during);

    assert_eq!(
        screen_column(&before, 'b'),
        screen_column(&during, 'b'),
        "covering the seam cluster moved the text after it"
    );
    assert_eq!(
        screen_column(&before, 'b'),
        4,
        "the row paints the flattened cells: a, the emoji, the space, then b"
    );
    assert_eq!(
        screen_width(&before),
        display_width("a\u{1f44d}\u{1f3fd} b"),
        "the unselected row paints the width the selection math budgets"
    );
    assert_eq!(
        screen_width(&during),
        display_width("a\u{1f44d}\u{1f3fd} b"),
        "the selected row paints the width the selection math budgets"
    );
}

#[test]
fn the_keyboard_wraps_at_the_painted_line_not_the_raw_spans() {
    // line_width feeds Left/Right their wrap boundary and Down its
    // floor, so its measure must be the same flattened projection
    // the flip, the copy, and the screen's strip walk — a per-span
    // sum counts a span-opening modifier as its own two cells and
    // walks the keyboard two cells past everything the selection
    // can cover.
    let mut subject = TuiApp::new(dch_config::DchConfig::default());
    subject.bake.conversation_cache = vec![Line::from(vec![
        Span::raw("a\u{1f44d}"),
        Span::raw("\u{1f3fd} b"),
    ])];
    assert_eq!(
        subject.line_width(0),
        Some(5),
        "the wrap boundary is the flattened five cells: a, the cluster, a space, b"
    );
}

#[test]
fn an_evicted_expansion_rebakes_off_the_baked_rows() {
    // Past the cap the oldest call's expansion retires with its
    // detail, and the rows it baked in the expanded shape must
    // not outlive the flag that backed them: the retirement
    // rebakes, so the next draw renders the block collapsed —
    // mid-conversation, where the tail re-render cannot reach it.
    let mut subject = app();
    let receipt = [
        "Changed: src/lib.rs (modified)",
        "│ 1 line removed, 1 added",
        "@@ -1,2 +1,2 @@",
        "  fn main() {",
        "-     old();",
        "+     new();",
        "  }",
    ]
    .join("\n");
    subject.retain_tool_detail("call_old", "{\"k\":1}", &receipt);
    subject
        .expansion
        .expanded_tools
        .insert("call_old".to_string());
    subject.push_message(TuiMessage::Assistant {
        blocks: vec![ContentBlock::Tool {
            name: "Edit".to_string(),
            call_id: "call_old".to_string(),
            input_preview: "src/lib.rs".to_string(),
            output_preview: String::new(),
            retained_input: String::new(),
            success: true,
            elapsed_secs: 0.1,
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    });
    subject.push_message(TuiMessage::User {
        text: "after the block".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
        unsent: false,
    });
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).expect("terminal");
    terminal.draw(|frame| subject.render(frame)).expect("draw");
    let expanded = buffer_text(terminal.backend().buffer());
    assert!(
        expanded.contains("old();"),
        "the expanded block draws its diff while the flag stands: {expanded}"
    );
    // TOOL_DETAIL_CAP later graduations retire the oldest call.
    for index in 0..TOOL_DETAIL_CAP {
        subject.retain_tool_detail(&format!("call_{index}"), "{\"k\":1}", "plain output");
    }
    assert!(
        !subject.expansion.expanded_tools.contains("call_old"),
        "the cap retired the oldest call's toggle state"
    );
    terminal
        .draw(|frame| subject.render(frame))
        .expect("redraw");
    let collapsed = buffer_text(terminal.backend().buffer());
    assert!(
        !collapsed.contains("old();"),
        "the retirement rebakes — the baked diff rows do not outlive \
             the flag they rode: {collapsed}"
    );
    assert!(
        collapsed.contains("src/lib.rs"),
        "the block's own summary row stands: {collapsed}"
    );
}

#[test]
fn copy_verdict_wording_covers_all_three_states() {
    assert_eq!(
        copy_verdict_notice(CopyVerdict {
            attempt: 1,
            escape_written: false,
            verified: true,
        }),
        "selection copied",
        "a verified transport earns the plain claim"
    );
    assert_eq!(
        copy_verdict_notice(CopyVerdict {
            attempt: 1,
            escape_written: true,
            verified: false,
        }),
        "",
        "an escape nothing confirmed posts no row at all"
    );
    assert_eq!(
        copy_verdict_notice(CopyVerdict {
            attempt: 1,
            escape_written: false,
            verified: false,
        }),
        "copy failed — set display.copy_command",
        "nothing at all names the escape hatch"
    );
}

#[test]
fn the_copy_answer_wording_matches_the_verdict_vocabulary() {
    assert_eq!(
        copy_answer_notice(CopyAnswer::Verified),
        "selection copied",
        "verified delivery earns the plain claim"
    );
    assert_eq!(
        copy_answer_notice(CopyAnswer::Unconfirmed),
        "",
        "a pending delivery posts nothing — the verdict is the only wording"
    );
    assert_eq!(
        copy_answer_notice(CopyAnswer::Failed),
        "copy failed — set display.copy_command",
        "nothing at all names the escape hatch"
    );
}

#[test]
fn copy_command_hands_the_last_answer_to_the_copier() {
    // The copy leg's real path: an injected copier records exactly what it
    // was handed, standing in for the clipboard so the test never shells
    // out. A turn carrying prose plus tool chrome must yield just the prose.
    use std::sync::atomic::AtomicU64;
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    let now = chrono::Utc::now();
    app.push_message(TuiMessage::User {
        text: "explain it".to_string(),
        timestamp: now,
        steered: false,
        unsent: false,
    });
    app.push_message(TuiMessage::Assistant {
        blocks: vec![
            ContentBlock::Text {
                text: "the answer".to_string(),
            },
            ContentBlock::Tool {
                name: "Read".to_string(),
                call_id: "c1".to_string(),
                input_preview: "a.rs".to_string(),
                success: true,
                elapsed_secs: 0.1,
                output_preview: "…".to_string(),
                retained_input: String::new(),
            },
        ],
        timestamp: now,
        duration_ms: Some(0),
    });
    let captured: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    let calls = Arc::new(AtomicU64::new(0));
    let sink = Arc::clone(&captured);
    let counted = Arc::clone(&calls);
    app.set_selection_copier(Box::new(move |_attempt, text| {
        *sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(text.to_string());
        counted.fetch_add(1, Ordering::SeqCst);
        CopyAnswer::Verified
    }));

    let out = command_output(&mut app, "/copy");

    assert_eq!(
        captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_deref(),
        Some("the answer"),
        "the copier receives the answer text, tool chrome excluded"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "exactly one copy attempted"
    );
    assert!(
        !out.contains("no answer"),
        "an answer was present, got {out:?}"
    );
}

#[test]
fn the_popup_spans_the_composer_s_width() {
    let area = Rect {
        x: 2,
        y: 10,
        width: 40,
        height: 3,
    };
    let popup = popup_rect(area, 4, true);
    assert_eq!(popup.width, 40, "the popup is as wide as the composer");
    assert_eq!(popup.x, 2, "it shares the composer's left edge");
    assert_eq!(popup.y, 6, "it floats immediately above the composer");
    assert_eq!(popup.height, 4, "it is as tall as its content needs");
}

#[test]
fn context_bar_fill_rounds_usage_into_cells() {
    assert_eq!(context_bar_fill(0, 200_000, 10), 0);
    assert_eq!(context_bar_fill(100_000, 200_000, 10), 5);
    assert_eq!(context_bar_fill(20_000, 200_000, 10), 1);
    assert_eq!(context_bar_fill(200_000, 200_000, 10), 10);
    assert_eq!(
        context_bar_fill(300_000, 200_000, 10),
        10,
        "usage past the window clamps at full"
    );
    assert_eq!(
        context_bar_fill(100_000, 0, 10),
        0,
        "an unknown window draws the bar empty"
    );
}

#[test]
fn the_status_bar_reads_the_live_window() {
    let mut app = app();
    // The footer belongs to a conversation with turns — the
    // welcome paints none — so the bar rides a landed prompt.
    app.push_message(TuiMessage::User {
        text: "hello".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
        unsent: false,
    });
    app.config.api.context_window = Some(200_000);
    app.state
        .context_window
        .store(100_000, std::sync::atomic::Ordering::SeqCst);
    app.state
        .tokens
        .lock()
        .expect("the tokens lock")
        .context_tokens = 50_000;
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
    let expected = context_bar_fill(50_000, 100_000, CONTEXT_BAR_CELLS);
    assert_eq!(
        thumb_cells, expected,
        "the bar divides by the live window, not the config field"
    );
}

#[test]
fn context_bar_readout_rounds_from_the_true_ratio() {
    assert_eq!(
        context_bar_readout(55_000, 100_000),
        Some(("55%".to_string(), "55k/100k".to_string())),
        "the readout spells the same fraction the bar paints"
    );
    assert_eq!(
        context_bar_readout(419_000, 1_000_000),
        Some(("42%".to_string(), "419k/1M".to_string())),
        "a million-strong window spells its unit: 1M, not 1000k"
    );
    assert_eq!(
        context_bar_readout(1_234_000, 2_000_000),
        Some(("62%".to_string(), "1.2M/2M".to_string())),
        "millions in between render in tenths"
    );
    assert_eq!(
        context_bar_readout(999_999, 1_000_000),
        Some(("100%".to_string(), "1M/1M".to_string())),
        "rounding into a million promotes the unit rather than printing 1000k"
    );
    assert_eq!(
        context_bar_readout(2_499, 200_000),
        Some(("1%".to_string(), "2k/200k".to_string())),
        "small usage rounds to its own numbers, not to zero"
    );
    assert_eq!(
        context_bar_readout(250_000, 200_000),
        Some(("125%".to_string(), "250k/200k".to_string())),
        "an over-window session reads its honest over-100 percent"
    );
    assert_eq!(
        context_bar_readout(30_000, 0),
        None,
        "an unknown window spells nothing — no denominator"
    );
}

#[test]
fn an_unconfirmed_drained_verdict_leaves_the_posted_wording_standing() {
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.post_notice("selection copied".to_string());
    let mut slot = app.copy.copy_outcomes.lock().expect("the outcomes slot");
    slot.push(CopyVerdict {
        attempt: 1,
        escape_written: true,
        verified: false,
    });
    drop(slot);
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
    terminal.draw(|frame| app.render(frame)).expect("draw");
    assert_eq!(
        app.transient_notice.as_ref().map(|(text, _)| text.as_str()),
        Some("selection copied"),
        "an unconfirmed verdict clears no posted wording — the row a \
             verified claim posted stays"
    );
}

#[test]
fn an_off_strip_paints_nothing_but_the_wake_stays_up() {
    let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut config = dch_config::DchConfig::default();
    config.display.strip_style = dch_config::StripStyle::Off;
    let app = TuiApp::from_observer_state(config, state);
    assert!(
        app.pulse_animation_living(),
        "an off strip still owes the elapsed row its cadence, mid-run"
    );
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
    let drew = terminal
        .draw(|frame| {
            app.render_pulse_strip(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 1,
                },
            );
        })
        .unwrap();
    let _ = drew;
    let buffer = terminal.backend().buffer();
    for column in 0..80u16 {
        assert!(
            buffer
                .cell((column, 0))
                .is_some_and(|cell| cell.symbol() == " "),
            "the off strip leaves the row empty at {column}"
        );
    }
}

#[test]
fn a_glow_breathes_its_color_while_staying_put() {
    let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut config = dch_config::DchConfig::default();
    config.display.theme = "dracula".to_string();
    config.display.strip_style = dch_config::StripStyle::Glow;
    let mut app = TuiApp::from_observer_state(config, state);
    let base = app.theme.ui.composer_border.unwrap_or(app.theme.ui.primary);
    assert!(
        matches!(base, ratatui::prelude::Color::Rgb(..)),
        "the dracula strip color is RGB, so the breath is visible"
    );
    let render = |app: &TuiApp| -> (usize, ratatui::prelude::Color) {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|frame| {
                app.render_pulse_strip(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 1,
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let bar = (0..80u16)
            .filter(|&column| {
                buffer
                    .cell((column, 0))
                    .is_some_and(|cell| cell.symbol() == "─")
            })
            .count();
        let color = buffer
            .cell((0, 0))
            .map_or(ratatui::prelude::Color::Reset, |cell| cell.fg);
        (bar, color)
    };
    // The pinned clock makes each phase exact: the anchor and the
    // frame's sample are the same instants the glow math derives its
    // eighths from, so a slow runner cannot round the start below
    // full brightness.
    let start = Instant::now();
    app.run_anchor = Some(start);
    app.pacing.pulse_clock.set(Some(start));
    let (bar, full) = render(&app);
    assert_eq!(bar, 80, "the glow bar holds the full row");
    assert_eq!(full, base, "the breath starts at full brightness");
    app.pacing.pulse_clock.set(Some(
        start
            .checked_add(Duration::from_millis(1800))
            .expect("the half-breath instant is computable"),
    ));
    let (bar, dim) = render(&app);
    assert_eq!(bar, 80, "the bar never moves");
    assert_eq!(
        dim,
        scaled_color(base, 4),
        "the half-breath sits at half brightness"
    );
    app.pacing.pulse_clock.set(Some(
        start
            .checked_add(Duration::from_millis(3600))
            .expect("the wrap instant is computable"),
    ));
    let (bar, wrapped) = render(&app);
    assert_eq!(bar, 80, "the bar stays full through the wrap");
    assert_eq!(wrapped, base, "the wrap returns to full brightness");
}

#[test]
fn a_named_strip_color_glows_without_breathing() {
    let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut config = dch_config::DchConfig::default();
    config.display.strip_style = dch_config::StripStyle::Glow;
    let mut app = TuiApp::from_observer_state(config, state);
    let named = ratatui::prelude::Color::Blue;
    app.theme.ui.composer_border = Some(named);
    let render = |app: &TuiApp| -> ratatui::prelude::Color {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
        terminal
            .draw(|frame| {
                app.render_pulse_strip(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 1,
                    },
                );
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .cell((0, 0))
            .map_or(ratatui::prelude::Color::Reset, |cell| cell.fg)
    };
    app.run_anchor = Some(Instant::now());
    assert_eq!(
        render(&app),
        named,
        "the named color draws as itself at full breath"
    );
    app.run_anchor = Some(
        Instant::now()
            .checked_sub(Duration::from_millis(1800))
            .expect("now is past the epoch"),
    );
    assert_eq!(
        render(&app),
        named,
        "a named color cannot scale, so the glow stays static through the half breath"
    );
}

#[test]
fn the_bounce_renders_right_anchored_on_its_reverse_pass() {
    let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
    let mut config = dch_config::DchConfig::default();
    config.display.strip_style = dch_config::StripStyle::Bounce;
    let mut app = TuiApp::from_observer_state(config, state.clone());
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    app.run_anchor = Some(
        Instant::now()
            .checked_sub(Duration::from_millis(4500))
            .expect("the monotonic clock is well past its epoch"),
    );
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
    terminal
        .draw(|frame| {
            app.render_pulse_strip(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 1,
                },
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(
        buffer.cell((79, 0)).expect("the last cell").symbol(),
        "─",
        "the reverse pass anchors its run at the right edge"
    );
    assert!(
        buffer
            .cell((40, 0))
            .is_some_and(|cell| cell.symbol() == "─"),
        "the reverse fill reached its halfway extent"
    );
    assert!(
        buffer
            .cell((35, 0))
            .is_some_and(|cell| cell.symbol() == " "),
        "before the fill the row stays clear"
    );
}

#[test]
fn the_travel_renders_right_anchored_on_its_reverse_pass() {
    let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
    let mut config = dch_config::DchConfig::default();
    config.display.strip_style = dch_config::StripStyle::Travel;
    let mut app = TuiApp::from_observer_state(config, state.clone());
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    app.run_anchor = Some(
        Instant::now()
            .checked_sub(Duration::from_millis(4500))
            .expect("the monotonic clock is well past its epoch"),
    );
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
    terminal
        .draw(|frame| {
            app.render_pulse_strip(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 1,
                },
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert!(
        buffer
            .cell((45, 0))
            .is_some_and(|cell| cell.symbol() == "─"),
        "the reverse pass carries the segment past mid-row"
    );
    assert!(
        buffer
            .cell((64, 0))
            .is_some_and(|cell| cell.symbol() == "─"),
        "the segment's right end trails it"
    );
    assert!(
        buffer
            .cell((70, 0))
            .is_some_and(|cell| cell.symbol() == " "),
        "clear cells sit beyond the segment"
    );
    assert!(
        buffer
            .cell((20, 0))
            .is_some_and(|cell| cell.symbol() == " "),
        "and before it, where the forward pass would have begun at column 0"
    );
}

#[test]
fn the_snake_renders_split_at_the_wrap() {
    let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
    let mut config = dch_config::DchConfig::default();
    config.display.strip_style = dch_config::StripStyle::Snake;
    let mut app = TuiApp::from_observer_state(config, state.clone());
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    app.run_anchor = Some(
        Instant::now()
            .checked_sub(Duration::from_millis(3150))
            .expect("the monotonic clock is well past its epoch"),
    );
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
    terminal
        .draw(|frame| {
            app.render_pulse_strip(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 1,
                },
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert!(
        buffer.cell((0, 0)).is_some_and(|cell| cell.symbol() == "─"),
        "the entering head run claims the left edge"
    );
    assert!(
        buffer
            .cell((10, 0))
            .is_some_and(|cell| cell.symbol() == " "),
        "clear cells follow the head run"
    );
    assert!(
        buffer
            .cell((69, 0))
            .is_some_and(|cell| cell.symbol() == " "),
        "and precede the exiting tail run"
    );
    assert!(
        buffer
            .cell((79, 0))
            .is_some_and(|cell| cell.symbol() == "─"),
        "the exiting tail run holds the right edge"
    );
}

#[test]
fn the_sheet_is_measured_against_the_rows_left_after_the_strip() {
    // The rows the question sheet may occupy: the frame's height less
    // the status bar, less the pulse strip's row when it is configured
    // to the top. The sheet's reservation and its box are both measured
    // from here, which is what keeps the layout's fixed rows from ever
    // summing past the frame the solver was handed.
    use ratatui::layout::Rect;

    let mut app = app();
    let area = Rect::new(0, 0, 40, 12);
    assert_eq!(
        app.question_sheet_area(area).height,
        12u16
            .saturating_sub(STATUS_BAR_ROWS)
            .saturating_add(STATUS_BAR_AIR),
        "only the bar's content rows are spoken for before the sheet sees the frame — its air is margin the sheet stands on"
    );

    app.display.strip_position = dch_config::StripPosition::Top;
    assert_eq!(
        app.question_sheet_area(area).height,
        12u16
            .saturating_sub(STATUS_BAR_ROWS)
            .saturating_add(STATUS_BAR_AIR)
            .saturating_sub(1),
        "a strip at the top spends a row the sheet is then not offered"
    );
}

#[test]
fn a_family_bearing_queued_prompt_reserves_the_rows_it_paints() {
    // The height fn once wrapped the raw prompt (the family
    // scoring two cells) while the painter
    // wrapped the stripped one (six), so the section reserved
    // fewer rows than it painted and the wording truncated
    // behind the queue's ellipsis. Both wrap the same projection
    // now, and the reserved height is exactly the rows the paint
    // spends plus the section's furniture.
    let mut subject = app();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    subject.host.submit_tx = Some(tx);
    subject.submit_text("first".to_string());
    subject
        .remove_queued(0)
        .expect("the first push is the driver's claim");
    subject.settle_handed();
    subject
        .state
        .live_claim
        .store(7, std::sync::atomic::Ordering::SeqCst);
    let width = 60u16;
    let prefix = queue_prompt_prefix(1, 0);
    let limit = usize::from(width).saturating_sub(str_width(&prefix)).max(1);
    let cap = usize::from(QUEUE_ROWS).saturating_sub(3);
    // Sized against the computed limit: the raw text fits one
    // row, the stripped render needs two.
    let filler = "a".repeat(limit.saturating_sub(3));
    let prompt = format!("{filler} \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}");
    subject.submit_text(prompt.clone());
    let painted = wrap_prompt_rows(&for_display(&prompt), limit, cap).len();
    assert_eq!(
        painted, 2,
        "the fixture genuinely crosses a row boundary only after the strip"
    );
    assert_eq!(
        usize::from(subject.queue_section_height(width)),
        painted.saturating_add(3),
        "the section reserves exactly the rows it paints"
    );
}

#[test]
fn the_steering_chord_survives_the_sheets_and_the_section() {
    // The hint row offers Ctrl+S the moment a draft meets a running
    // agent, and the queue's focus swallows everything the section
    // does not answer — the survival set is what keeps an advertised
    // key from becoming advertised noise.
    assert!(key_survives_the_sheet(KeyEvent::new(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL
    )));
}

#[test]
fn the_frame_window_opens_on_the_first_draw() {
    // The per-second lane measures from the first draw it counts,
    // not from the draw standing at its close — the difference
    // between a second that can pass and one that never does. So
    // the opening draw seeds the window and says nothing, and the
    // counters go out whole when the window falls.
    let epoch = Instant::now();
    let mut stats = FrameStats {
        anim_wakes: 4,
        ..FrameStats::default()
    };
    assert!(
        stats.note_draw(Duration::from_millis(5), epoch).is_none(),
        "the opening draw seeds the window and reports nothing"
    );
    assert_eq!(stats.draws, 1, "and is counted itself");
    assert!(
        stats
            .note_draw(Duration::from_millis(5), epoch + Duration::from_millis(900))
            .is_none(),
        "a draw inside the second leaves it standing"
    );
    let lane = stats
        .note_draw(
            Duration::from_millis(5),
            epoch + Duration::from_millis(1_100),
        )
        .expect("a draw past the second closes the window");
    assert_eq!(
        (lane.anim_wakes, lane.draws),
        (4, 3),
        "the closed window reports everything it counted"
    );
    assert_eq!(stats.draws, 0, "the reopened window starts empty");
    assert_eq!(stats.anim_wakes, 0, "in every counter");
    assert!(stats.worst_draw.is_none(), "including the worst draw");
    assert_eq!(
        stats.second_start,
        Some(epoch + Duration::from_millis(1_100)),
        "the next window opens at the draw that closed the last"
    );
}

#[test]
fn the_tick_redraws_only_while_tools_run() {
    let app = app();
    assert!(!app.any_tools_running(), "no tools in flight yet");

    app.active_tools()
        .lock()
        .expect("the tools lock")
        .push(ActiveTool {
            call_id: String::new(),
            name: "Grep".to_string(),
            input_summary: "\"needle\"".to_string(),
            start: std::time::Instant::now(),
        });
    assert!(app.any_tools_running(), "a running tool asks for redraws");

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = app.active_tools().lock().expect("the tools lock");
        panic!("poison the tools lock");
    }));
    assert!(panicked.is_err(), "the poisoning panic must unwind");
    assert!(
        !app.any_tools_running(),
        "a poisoned lock reads as no tools in flight"
    );
}

#[test]
fn stop_cancels_a_run_in_flight_without_quitting() {
    // The stop leg's live path: with a run marked in flight, the shared
    // canceller fires and a notice lands, yet the session is asked to stay —
    // the distinction from `/exit` that the whole command turns on.
    use std::sync::atomic::AtomicBool;
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    let cancelled = Arc::new(AtomicBool::new(false));
    let fired = Arc::clone(&cancelled);
    app.set_run_canceller(Box::new(move || fired.store(true, Ordering::SeqCst)));
    app.state.agent_running.store(true, Ordering::SeqCst);

    let out = command_output(&mut app, "/stop");

    assert!(out.contains("stopped the current run"), "got {out:?}");
    assert!(
        cancelled.load(Ordering::SeqCst),
        "the run was actually cancelled"
    );
    assert!(
        !app.quit.requested,
        "stopping a run must not leave the session"
    );
}

#[test]
fn the_popup_window_follows_the_highlight() {
    assert_eq!(
        popup_window(3, 12, 6),
        (0, 3),
        "near the head the window sits at the top"
    );
    assert_eq!(
        popup_window(7, 12, 6),
        (2, 5),
        "past the last row the highlight pins to the window's bottom"
    );
    assert_eq!(
        popup_window(11, 12, 6),
        (6, 5),
        "at the very end the window rides the list's tail"
    );
    assert_eq!(
        popup_window(0, 1, 6),
        (0, 0),
        "a list shorter than the cap shows whole"
    );
}

#[test]
fn the_sweep_renders_left_anchored_when_configured() {
    let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
    let mut config = dch_config::DchConfig::default();
    config.display.strip_style = dch_config::StripStyle::Sweep;
    let mut app = TuiApp::from_observer_state(config, state.clone());
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    app.run_anchor = Some(
        Instant::now()
            .checked_sub(Duration::from_millis(900))
            .expect("the monotonic clock is well past its epoch"),
    );
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
    terminal
        .draw(|frame| {
            app.render_pulse_strip(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 1,
                },
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(
        buffer.cell((0, 0)).expect("the first cell").symbol(),
        "─",
        "the sweep run starts at column 0, where the breath would pad first"
    );
    assert!(
        buffer
            .cell((39, 0))
            .is_some_and(|cell| cell.symbol() == "─"),
        "the fill reached its 900 ms extent"
    );
    assert!(
        buffer
            .cell((45, 0))
            .is_some_and(|cell| cell.symbol() == " "),
        "beyond the fill the row stays clear"
    );
}

#[test]
fn a_canvas_repainting_theme_keeps_the_running_titles_visible() {
    // The reported bug, pinned at the scene: github_light repaints
    // the canvas white, and a terminal launched under a dark scheme
    // keeps a light default foreground — any register that leans on
    // the terminal default renders white on white. The running
    // title must carry the theme's own standing foreground, which
    // on this canvas is a dark register.
    let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
    let mut config = dch_config::DchConfig::default();
    config.display.theme = "github_light".to_string();
    let app = TuiApp::from_observer_state(config, state.clone());
    assert_eq!(
        app.theme.ui.background,
        ratatui::style::Color::Rgb(255, 255, 255),
        "fixture premise: the theme repaints a white canvas"
    );
    assert_eq!(
        app.theme.ui.foreground,
        ratatui::style::Color::Rgb(36, 41, 47),
        "fixture premise: the standing register is dark on that canvas"
    );
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    *state.thinking.lock().expect("the thinking lock") = crate::observer::ThinkingStream {
        text: "reasoning in flight".to_string(),
        began: Some(Instant::now()),
        ..Default::default()
    };
    let (lines, _) = app.live_thinking_lines(80);
    let title = lines
        .first()
        .expect("the marker row")
        .spans
        .iter()
        .find(|span| span.content.contains("Thinking"))
        .expect("the title span");
    assert_eq!(
        title.style.fg,
        Some(ratatui::style::Color::Rgb(36, 41, 47)),
        "the title paints in the theme's dark standing register — never the \
             terminal default a dark-scheme terminal would betray"
    );
}

#[test]
fn a_browse_left_by_backspace_restores_the_theme() {
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.composer.input.set_text("/theme ".to_string());
    let original = app.theme.name;
    let (_, candidates, _) = app.fresh_completion("/theme ");
    let third = candidates.get(2).cloned().unwrap_or_default();
    // Two arrows land on the third theme; the preview follows.
    assert!(app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
    assert!(app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
    assert_eq!(
        app.theme.name,
        Theme::by_name(&third).map_or_default(|theme| theme.name),
        "the arrows previewed the third theme"
    );
    // Backspace out of the applied row and past the space: the
    // text leaves the browse, and the theme returns to the
    // session's own — not the preview.
    while app.composer.input.text().starts_with("/theme ") {
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            "the editor takes the backspace"
        );
    }
    assert_eq!(
        app.theme.name, original,
        "the browse the backspace ended restores the session's theme"
    );
}

#[test]
fn the_browse_previews_the_theme_it_highlights() {
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.composer.input.set_text("/theme ".to_string());
    let original = app.theme.name;
    let (_, candidates, _) = app.fresh_completion("/theme ");
    let second = candidates.get(1).cloned().unwrap_or_default();
    assert!(app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
    assert_eq!(
        app.composer.input.text(),
        format!("/theme {second}"),
        "the composer names the row the highlight stands on"
    );
    assert!(
        app.completion_popup_live(),
        "the browse leaves the popup standing"
    );
    assert_eq!(
        app.theme.name,
        Theme::by_name(&second).map_or_default(|theme| theme.name),
        "the palette previews the highlighted row, not the theme the browse started from"
    );
    assert!(
        app.theme.name != original || original.eq_ignore_ascii_case(&second),
        "the preview moved to the highlighted row (or that row was the original)"
    );
    assert!(app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert_eq!(
        app.theme.name, original,
        "Esc after the browse restores the theme the session was on"
    );
}

#[test]
fn the_exchange_splits_a_direct_terminal_from_a_multiplexed_pane() {
    // The two seats a session can stand in, pinned in one walk because
    // the verdict is process-wide: a direct terminal leans on the
    // OSC 11 exchange — an unanswered startup owes one question, and
    // a preview defers its grounds until the canvas is proven — while
    // a pane inside a multiplexer runs no exchange at all: the frame
    // paints its own canvas, the pane keeps the client's default, and
    // no erase the multiplexer performs on its own — a resize reflow,
    // an overlay's clear, the alternate-screen handoff — can ever show
    // the theme's color where the frame did not put it.
    let _verdict = crate::terminal::MULTIPLEXER_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let canvas = ratatui::style::Color::Rgb(12, 12, 12);
    crate::terminal::set_multiplexed_for_tests(Some(false));
    let mut direct = TuiApp::new(dch_config::DchConfig::default());
    assert!(
        direct.startup_background_ask_warranted().is_some(),
        "a direct session the terminal never answered owes the question"
    );
    let browsed = Theme::by_name("dracula").expect("dracula resolves");
    let dracula_foreground = browsed.ui.foreground;
    direct.preview_theme(browsed);
    assert_eq!(
        direct.theme.ui.foreground,
        ratatui::style::Color::Reset,
        "a direct terminal without a proven canvas previews the deferred register"
    );
    assert_ne!(
        dracula_foreground,
        ratatui::style::Color::Reset,
        "fixture premise: the full palette carries an anchored foreground"
    );
    assert_eq!(
        direct.theme.ui.surface,
        crate::theme::Theme::by_name("dracula")
            .map_or(ratatui::style::Color::Reset, |theme| theme.ui.surface),
        "the previewed painted pairs stand — deferral touches the inks alone"
    );
    crate::terminal::set_multiplexed_for_tests(Some(true));
    let mut pane = TuiApp::new(dch_config::DchConfig::default());
    assert!(
        pane.startup_background_ask_warranted().is_none(),
        "a pane owes no exchange — its canvas is the frame's own to paint"
    );
    assert!(
        crate::terminal::sync_default_background(canvas),
        "the pane's canvas stands without a byte of the exchange"
    );
    assert!(
        !crate::terminal::ask_default_background(canvas).installed,
        "no mid-session ask from a pane"
    );
    assert!(
        crate::terminal::write_only_warranted(canvas),
        "an Rgb palette is always paintable in a pane"
    );
    assert!(
        !crate::terminal::write_only_warranted(ratatui::style::Color::Reset),
        "the paintless palette stays deferred even in a pane"
    );
    let browsed = Theme::by_name("dracula").expect("dracula resolves");
    let browsed_foreground = browsed.ui.foreground;
    pane.preview_theme(browsed);
    assert_ne!(
        pane.theme.ui.surface,
        ratatui::style::Color::Reset,
        "a pane previews the full palette — the grounding paints the canvas"
    );
    assert_eq!(
        pane.theme.ui.foreground, browsed_foreground,
        "a pane previews the full palette, its anchored foreground included"
    );
    crate::terminal::set_multiplexed_for_tests(None);
}

#[test]
fn the_startup_retry_lands_the_registry_palette_over_the_deferral() {
    // The typed-ahead startup defers the palette and leaves the
    // terminal record at never-asked; the retry that lands re-resolves
    // the registry palette under the deferred palette's own name — the
    // grounds and inks come back with it, the same palette the session
    // would have opened under had the query run.
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    let canvas = app.theme.ui.background;
    let surface = app.theme.ui.surface;
    let foreground = app.theme.ui.foreground;
    app.theme = app.theme.deferred_to_terminal();
    assert_eq!(
        app.theme.ui.foreground,
        ratatui::style::Color::Reset,
        "fixture premise: the deferral masked the anchored inks"
    );
    assert_eq!(
        app.theme.ui.surface, surface,
        "fixture premise: the deferral kept the painted pairs"
    );
    assert!(
        app.land_startup_canvas(),
        "a registry palette answers the deferred palette's name"
    );
    assert_eq!(
        app.theme.ui.surface, surface,
        "the grounds un-mask with the palette"
    );
    assert_eq!(app.theme.ui.foreground, foreground);
    assert_eq!(app.theme.ui.background, canvas);
    assert_eq!(
        app.theme.name, "Dch",
        "the landed palette is the registry default"
    );
}

#[test]
fn a_paintless_palette_owes_no_startup_ask() {
    // The transparent palette defers by construction — its canvas is
    // the terminal's own — so the startup retry has nothing to ask and
    // nothing to install: no handshake, no write, the palette stands,
    // in a direct terminal or a pane alike.
    let _verdict = crate::terminal::MULTIPLEXER_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    crate::terminal::set_multiplexed_for_tests(Some(false));
    let mut config = dch_config::DchConfig::default();
    config.display.theme = "transparent".to_string();
    let app = TuiApp::new(config);
    assert_eq!(
        app.theme.ui.background,
        ratatui::style::Color::Reset,
        "fixture premise: the paintless canvas is the terminal's own"
    );
    assert!(
        app.startup_background_ask_warranted().is_none(),
        "no Rgb canvas, no question owed"
    );
    crate::terminal::set_multiplexed_for_tests(None);
}

#[test]
fn a_stated_seat_grounds_a_deferred_palette_the_seat_doc_names_out_of_contract() {
    // The seat seam's one impossible environment: a palette that
    // deferred (a direct terminal that never answered) behind a
    // stated pane seat. The grounding walk reads the canvas alone, so
    // the deferred palette's Rgb canvas is painted anyway — pinned
    // here as the boundary the seat's doc names out of contract, so
    // widening the warrant to consult the palette's deferral is a
    // deliberate change this pin greets, not a silent drift.
    let _verdict = crate::terminal::MULTIPLEXER_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    crate::terminal::set_multiplexed_for_tests(Some(false));
    let mut subject = TuiApp::new(dch_config::DchConfig::default());
    subject.theme = Theme::by_name("dracula")
        .expect("dracula resolves")
        .deferred_to_terminal();
    let canvas = subject.theme.ui.background;
    assert!(
        matches!(canvas, ratatui::style::Color::Rgb(..)),
        "fixture premise: deferral keeps the canvas"
    );
    assert_eq!(
        subject.theme.ui.foreground,
        ratatui::style::Color::Reset,
        "fixture premise: the palette deferred its anchored ink"
    );
    subject.set_terminal_seat(true);
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).expect("terminal");
    terminal.draw(|frame| subject.render(frame)).expect("draw");
    let buffer = terminal.backend().buffer();
    assert!(
        !buffer
            .content
            .iter()
            .any(|cell| cell.bg == ratatui::style::Color::Reset),
        "the seated walk paints the deferred palette's canvas on every default cell — the boundary the doc names"
    );
    crate::terminal::set_multiplexed_for_tests(None);
}

#[test]
fn the_canvas_ground_needs_the_canvas_in_force() {
    // The warrant's three questions: is the canvas in force — a pane
    // inside a multiplexer, or a direct terminal the OSC 11 exchange
    // landed on and still carries — is the canvas installed, and is
    // it an Rgb canvas the frame may paint. A direct terminal the
    // exchange never landed on keeps its own scheme (the deferred
    // palette paints no canvas), an answered terminal whose canvas
    // was restored keeps the restored scheme, and the transparent
    // palette defers to the terminal's own default in any seat.
    let canvas = ratatui::style::Color::Rgb(12, 12, 12);
    let answered = crate::terminal::BackgroundSupport::Answered;
    let refused = crate::terminal::BackgroundSupport::Refused;
    let never_asked = crate::terminal::BackgroundSupport::NotAsked;
    assert!(
        TuiApp::canvas_ground_warranted(true, never_asked, canvas, false),
        "a pane paints its canvas without an exchange"
    );
    assert!(
        TuiApp::canvas_ground_warranted(false, answered, canvas, true),
        "a direct terminal the set landed on gets the canvas painted too"
    );
    assert!(
        !TuiApp::canvas_ground_warranted(false, answered, canvas, false),
        "an answered terminal whose canvas was restored keeps the restored scheme"
    );
    assert!(
        !TuiApp::canvas_ground_warranted(false, refused, canvas, true),
        "a refused terminal keeps its own scheme: nothing to paint"
    );
    assert!(
        !TuiApp::canvas_ground_warranted(false, never_asked, canvas, true),
        "an unsettled terminal has proven nothing: nothing to paint"
    );
    assert!(
        !TuiApp::canvas_ground_warranted(true, answered, ratatui::style::Color::Reset, true),
        "the paintless palette defers to the terminal's own default"
    );
}

#[test]
fn a_grounding_walk_covers_the_blanks_and_keeps_the_surfaces() {
    // The walk behind a multiplexed frame: every cell a widget left on
    // the terminal default takes the canvas — the blank rows, the row
    // tails, the top of the pane — while the grounds the widgets
    // painted themselves (the composer's surface, the splash's art)
    // stand exactly as they were. The pane then shows one canvas
    // whatever the client terminal keeps as its own default.
    let _verdict = crate::terminal::MULTIPLEXER_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    crate::terminal::set_multiplexed_for_tests(Some(false));
    let mut subject = app();
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).expect("terminal");
    terminal.draw(|frame| subject.render(frame)).expect("draw");
    let canvas = subject.theme.ui.background;
    let surface = subject.theme.ui.surface;
    let before = terminal.backend().buffer().clone();
    assert!(
        before
            .content
            .iter()
            .any(|cell| cell.bg == ratatui::style::Color::Reset),
        "fixture premise: an ungrounded frame leaves blanks on the terminal default"
    );
    let mut grounded = before;
    super::ground_default_cells(&mut grounded, canvas);
    assert!(
        grounded
            .content
            .iter()
            .all(|cell| cell.bg != ratatui::style::Color::Reset),
        "the walk leaves no cell on the terminal default — the client's own \
             background can no longer show through"
    );
    assert!(
        grounded.content.iter().any(|cell| cell.bg == surface),
        "the composer's painted surface stands untouched by the walk"
    );
    assert!(
        grounded.content.iter().any(|cell| cell.bg == canvas),
        "the canvas lands on the blanks the widgets left"
    );
    crate::terminal::set_multiplexed_for_tests(None);
}

#[test]
fn a_replaced_live_tail_rebuilds_its_rows() {
    // The live stamp covers the whole unfrozen suffix: a buffer
    // cleared and refilled to a comparable length — the documented
    // cleared-and-refilled case the stamp exists for — must not match
    // the old tail's stamp, or the pane keeps serving the replaced
    // turn's rows. The two tails share their length and their last 64
    // characters; only the head byte differs.
    let theme = Theme::default();
    let md = markdown::MarkdownTheme::from(&theme);
    let syntax = markdown::SyntaxTheme::from(&theme);
    let mut freeze = FreezeCache::default();
    let fill = "x".repeat(64);
    assert!(freeze.refresh(
        &format!("A{fill}"),
        80,
        &md,
        &syntax,
        theme.ui.assistant_message_fg
    ));
    assert!(
        freeze.refresh(
            &format!("B{fill}"),
            80,
            &md,
            &syntax,
            theme.ui.assistant_message_fg
        ),
        "the replaced tail rebuilds"
    );
    let text: String = freeze
        .live_lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.to_string())
        .collect();
    assert!(
        text.contains('B') && !text.contains('A'),
        "the live rows carry the replacement: {text:?}"
    );
}

#[test]
fn the_live_lane_opens_with_the_speaker_change_row() {
    // The running turn owes the same blank row under the prompt
    // that the batch render puts between a user message and the
    // assistant content answering it: without it the first running
    // row rides one row tighter than the same content sits after
    // graduating, and the gap opens as a jump the moment it does.
    let mut subject = app();
    subject.push_message(TuiMessage::User {
        text: "go check the files".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
        unsent: false,
    });
    subject
        .active_tools()
        .lock()
        .expect("the tools lock")
        .push(ActiveTool {
            call_id: "call_seam".to_string(),
            name: "Grep".to_string(),
            input_summary: "\"needle\"".to_string(),
            start: std::time::Instant::now(),
        });
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
    terminal.draw(|frame| subject.render(frame)).expect("draw");
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..24u16)
        .map(|y| {
            (0..80u16)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect();
    let prompt = rows
        .iter()
        .position(|row| row.contains("go check the files"))
        .expect("the submitted prompt renders");
    let tool = rows
        .iter()
        .position(|row| row.contains("Grep"))
        .expect("the running tool renders");
    assert_eq!(
        tool.saturating_sub(prompt),
        3,
        "the prompt's closing blank, the speaker-change row, then the \
             running tool — the same air the graduated turn sits in: {rows:?}"
    );
}

#[test]
fn the_pulse_bar_runs_on_the_runs_clock_not_the_streams() {
    let (_observer, state) = crate::observer::TuiObserverState::new().into_observer();
    let mut app = TuiApp::from_observer_state(dch_config::DchConfig::default(), state.clone());
    state
        .agent_running
        .store(true, std::sync::atomic::Ordering::Relaxed);
    // 900 ms into the run, a stream that began this instant: the
    // bar must follow the run's clock, not the stream's.
    app.run_anchor = Some(
        Instant::now()
            .checked_sub(Duration::from_millis(900))
            .expect("the monotonic clock is well past its epoch"),
    );
    *state.thinking.lock().expect("the thinking lock") = crate::observer::ThinkingStream {
        text: "a stream that just began".to_string(),
        began: Some(Instant::now()),
        ..Default::default()
    };
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 1)).unwrap();
    terminal
        .draw(|frame| {
            app.render_pulse_strip(
                frame,
                Rect {
                    x: 0,
                    y: 0,
                    width: 80,
                    height: 1,
                },
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let bar = (0..80u16)
        .filter(|&column| {
            buffer
                .cell((column, 0))
                .is_some_and(|cell| cell.symbol() == "─")
        })
        .count();
    assert!(
        bar > 8,
        "a run 900 ms old grows the bar wide ({bar} cells), not the one-cell seed \
             a fresh stream's clock would draw"
    );
}

#[test]
fn a_confirm_spends_the_prompt_it_armed_not_the_one_that_slid_into_view() {
    // The review's interleaving, pinned: three prompts wait, the view
    // shows the middle one, the drop ask arms against its id — and a
    // claim then takes the front prompt. The view still points in
    // range, now at the third prompt. `y` must spend the prompt the
    // ask named, never the stranger that slid into the view's
    // position: resolving the act by display position destroys a
    // prompt the user never aimed at.
    use super::queue::QueueAct;
    let mut subject = app();
    for text in ["first", "second", "third"] {
        subject.push_queued(text.to_string());
    }
    subject.queue.queue_mode = QueueMode::Focused;
    subject.queue.queue_view = 1;
    assert!(
        subject.queue_focused_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
        "the arming press is swallowed by the section"
    );
    let QueueMode::Confirming { act, id } = subject.queue.queue_mode else {
        panic!("the drop ask armed");
    };
    assert!(matches!(act, QueueAct::Drop), "the ask is a drop");
    assert_eq!(id, 1, "the ask armed against the shown prompt's id");
    // The driver claims the front prompt — every claim takes the
    // oldest — sliding `third` into the view's position.
    let claimed = subject.remove_queued(0);
    assert_eq!(
        claimed.as_deref(),
        Some("first"),
        "the claim took the front"
    );
    assert_eq!(
        subject.queued_at(1).map(|(_, text)| text),
        Some("third".to_string()),
        "the stale view now points at the stranger"
    );
    assert!(
        subject.queue_confirm_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            act,
            id
        ),
        "the confirm is swallowed by the section"
    );
    let remaining: Vec<String> = (0..subject.queued_len())
        .filter_map(|index| subject.queued_at(index).map(|(_, text)| text))
        .collect();
    assert_eq!(
        remaining,
        vec!["third".to_string()],
        "the armed prompt is spent and the stranger survives"
    );
}

#[test]
fn a_confirm_stands_down_when_the_armed_prompt_was_claimed() {
    // The ask's own gate: when the claim takes the prompt the ask
    // armed against — not a neighbour, the prompt itself — `y` spends
    // nothing. The notice says the confirmation stood down, and the
    // queue keeps every prompt that survived the claim.
    let mut subject = app();
    for text in ["first", "second"] {
        subject.push_queued(text.to_string());
    }
    subject.queue.queue_mode = QueueMode::Focused;
    subject.queue.queue_view = 1;
    assert!(subject.queue_focused_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    let QueueMode::Confirming { act, id } = subject.queue.queue_mode else {
        panic!("the drop ask armed");
    };
    assert_eq!(id, 1, "the ask armed against the shown prompt");
    let _claimed = subject.remove_queued(1);
    assert!(subject.queue_confirm_key(
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        act,
        id
    ));
    assert_eq!(
        subject.notice_text().as_deref(),
        Some("the prompt left the queue — the confirmation stood down"),
        "the confirm says it stood down rather than spending silently"
    );
    assert_eq!(
        subject.queued_len(),
        1,
        "the surviving prompt keeps its place"
    );
    assert_eq!(
        subject.queued_at(0).map(|(_, text)| text),
        Some("first".to_string()),
        "the survivor is the claim's neighbour, untouched"
    );
}

#[test]
fn a_confirmations_preview_follows_the_armed_prompt() {
    // The ask's preview, reconciled by the frame: a claim slides the
    // rows while the confirmation stands, and a view left at its old
    // position would render the ask beside a prompt it will not
    // spend. The frame points the view at the prompt the ask armed,
    // so what the user reads is what `y` answers.
    let mut subject = app();
    for text in ["first", "second", "third"] {
        subject.push_queued(text.to_string());
    }
    subject.queue.queue_mode = QueueMode::Focused;
    subject.queue.queue_view = 1;
    assert!(subject.queue_focused_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    // The driver claims the front prompt — every claim takes the
    // oldest — sliding `third` into the view's stale position.
    let claimed = subject.remove_queued(0);
    assert_eq!(claimed.as_deref(), Some("first"));
    subject.sync_queue_view();
    assert_eq!(
        subject.queue.queue_view, 0,
        "the preview follows the armed prompt, not the stale position"
    );
    assert_eq!(
        subject
            .queued_at(subject.queue.queue_view)
            .map(|(_, text)| text),
        Some("second".to_string()),
        "the ask renders beside the prompt it will spend"
    );
}

#[test]
fn a_confirmation_stands_down_when_the_frame_finds_the_prompt_gone() {
    // The claim takes the armed prompt itself between frames. The
    // frame finds the ask aimed at nothing and stands it down — the
    // section returns to browsing, and the notice says the
    // confirmation went with the prompt.
    let mut subject = app();
    for text in ["first", "second"] {
        subject.push_queued(text.to_string());
    }
    subject.queue.queue_mode = QueueMode::Focused;
    subject.queue.queue_view = 1;
    assert!(subject.queue_focused_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)));
    let gone = subject.remove_queued(1);
    assert_eq!(gone.as_deref(), Some("second"));
    subject.sync_queue_view();
    assert!(
        matches!(subject.queue.queue_mode, QueueMode::Focused),
        "the ask stands itself down"
    );
    assert_eq!(
        subject.notice_text().as_deref(),
        Some("the prompt left the queue — the confirmation stood down"),
        "the frame's dismissal says what it did"
    );
    assert_eq!(
        subject.queued_at(0).map(|(_, text)| text),
        Some("first".to_string()),
        "the survivor keeps its place"
    );
}

#[test]
fn a_slash_prompt_refused_as_guidance_is_delivered_literally() {
    // A prompt saved in the queue may spell a command: `/clear` as
    // literal words, kept by an edit or typed at a moment that was
    // never a dispatch. Guiding it into a run that refuses guidance
    // falls back to its own turn — and the fallback delivers the
    // words, never the command they spell: a queued prompt that
    // happens to read `/clear` must not clear the session.
    let mut subject = app();
    subject
        .conversation
        .push(crate::message::TuiMessage::System {
            text: "a row the session keeps".to_string(),
            timestamp: chrono::Utc::now(),
        });
    subject
        .state
        .agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let id = subject.push_queued("/clear".to_string());
    subject.steer_queued(id);
    assert!(
        subject
            .conversation
            .iter()
            .any(|message| matches!(message, crate::message::TuiMessage::System { text, .. } if text == "a row the session keeps")),
        "a literal delivery clears nothing"
    );
    assert!(
        subject
            .conversation
            .iter()
            .any(|message| matches!(message, crate::message::TuiMessage::User { text, .. } if text == "/clear")),
        "the slash prompt lands as the user's own words"
    );
    assert_eq!(
        subject.queued_len(),
        0,
        "the guided prompt left the queue once, delivered once"
    );
}

#[test]
fn a_steer_act_refuses_to_duplicate_a_claimed_prompt() {
    // The steer arm under the same claim window: the ask armed against
    // a prompt the running turn has since claimed. `y` must not steer
    // whatever slid into the view — the guide refuses, the notice
    // names the refusal, and the queue keeps its survivors whole.
    use super::queue::QueueAct;
    let mut subject = app();
    for text in ["first", "second", "third"] {
        subject.push_queued(text.to_string());
    }
    subject
        .state
        .agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    subject.queue.queue_mode = QueueMode::Focused;
    subject.queue.queue_view = 1;
    assert!(subject.queue_focused_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)));
    let QueueMode::Confirming { act, id } = subject.queue.queue_mode else {
        panic!("the steer ask armed");
    };
    assert!(matches!(act, QueueAct::Steer), "the ask is a steer");
    // The narrow race, modeled head-on: the confirm's gate saw the
    // prompt stand, and the claim takes it in the window between that
    // gate and the act's own removal — so the act is driven with the
    // armed id after the removal, exactly the state the window leaves.
    let _claimed = subject.remove_queued(1);
    subject.steer_queued(id);
    assert_eq!(
        subject.notice_text().as_deref(),
        Some("the prompt left the queue before the guide landed"),
        "the guide names its refusal rather than duplicating the claim"
    );
    let remaining: Vec<String> = (0..subject.queued_len())
        .filter_map(|index| subject.queued_at(index).map(|(_, text)| text))
        .collect();
    assert_eq!(
        remaining,
        vec!["first".to_string(), "third".to_string()],
        "the survivors keep their order, nothing steered"
    );
}

#[test]
fn an_ask_never_arms_against_a_stale_view() {
    // The arming rule: an ask needs a prompt on screen. A view left
    // pointing past the end — a claim landed since the last frame
    // clamped it — arms nothing; the press stands down, the view
    // clamps, and no confirmation mode carries a defaulted id that
    // could name a real prompt.
    let mut subject = app();
    subject.push_queued("only".to_string());
    subject.queue.queue_mode = QueueMode::Focused;
    subject.queue.queue_view = 3;
    assert!(
        subject.queue_focused_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
        "the press is still swallowed by the section"
    );
    assert_eq!(
        subject.queue.queue_mode,
        QueueMode::Focused,
        "no ask armed against the stale view"
    );
    assert_eq!(subject.queued_len(), 1, "the waiting prompt is untouched");
}

#[test]
fn a_refused_guide_delivers_a_slash_spelled_prompt_as_words() {
    // The literal-delivery half of the steer act: guidance refused —
    // nothing to steer into — the armed prompt's words become their
    // own turn exactly as written. A prompt spelling `/clear` reaches
    // the conversation as the user's words, not as the command it
    // happens to spell: the session's history survives the delivery.
    use crate::message::{ContentBlock, TuiMessage};
    let mut subject = app();
    subject.push_message(TuiMessage::Assistant {
        blocks: vec![ContentBlock::Text {
            text: "history that must survive".to_string(),
        }],
        timestamp: chrono::Utc::now(),
        duration_ms: None,
    });
    subject.push_queued("/clear".to_string());
    subject
        .state
        .agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    subject.queue.queue_mode = QueueMode::Focused;
    subject.queue.queue_view = 0;
    assert!(subject.queue_focused_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)));
    let QueueMode::Confirming { act, id } = subject.queue.queue_mode else {
        panic!("the steer ask armed");
    };
    assert!(subject.queue_confirm_key(
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        act,
        id
    ));
    assert_eq!(subject.queued_len(), 0, "the guided prompt left the queue");
    let delivered = subject.conversation.iter().any(|message| match message {
        TuiMessage::User { text, .. } => text == "/clear",
        _ => false,
    });
    assert!(
        delivered,
        "the slash-spelled prompt landed in the conversation as literal words"
    );
    assert!(
        subject.conversation.iter().any(|message| match message {
            TuiMessage::Assistant { blocks, .. } => blocks.iter().any(
                |block| matches!(block, ContentBlock::Text { text } if text.contains("survive"))
            ),
            _ => false,
        }),
        "the command it spelled never ran — the history stands"
    );
}

#[test]
fn a_steer_of_an_open_edit_delivers_the_saved_words_literally() {
    // The steer chord over an edit: the composer holds a saved
    // prompt's body, so retiring its slot does not make the words the
    // composer's own. A steer that finds nothing to steer into — the
    // run ended before a boundary could read the words — falls back to
    // the prompt's own turn, and that fallback delivers literally: a
    // body spelling `/clear` reaches the conversation as the user's
    // words, never as the command it spells.
    use crate::message::TuiMessage;
    let mut subject = app();
    subject.push_message(TuiMessage::System {
        text: "a row the session keeps".to_string(),
        timestamp: chrono::Utc::now(),
    });
    subject.push_queued("/clear".to_string());
    subject.queue.queue_mode = QueueMode::Focused;
    subject.queue.queue_view = 0;
    assert!(subject.queue_focused_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)));
    assert!(
        matches!(subject.queue.queue_mode, QueueMode::Editing { .. }),
        "fixture premise: the edit stands open on the composer"
    );
    subject
        .state
        .agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(subject.steer_now(), "the steer chord takes the press");
    assert_eq!(
        subject.queued_len(),
        0,
        "the retired slot's words were delivered once"
    );
    assert!(
        subject.conversation.iter().any(|message| match message {
            TuiMessage::User { text, .. } => text == "/clear",
            _ => false,
        }),
        "the retired prompt's words land as the user's own"
    );
    assert!(
        subject.conversation.iter().any(|message| match message {
            TuiMessage::System { text, .. } => text == "a row the session keeps",
            _ => false,
        }),
        "the command the words spelled never ran — the history stands"
    );
}

#[test]
fn a_correction_with_no_driver_is_delivered_literally() {
    // The gone-arm of an edit commit on a host with no driver: the
    // run claimed the prompt while its body stood in the composer,
    // and nothing is left to take the correction. The corrected
    // wording is still a saved prompt's body, so the driver-less
    // fallback delivers the words rather than parsing them: a body
    // spelling `/clear` cannot clear the session in the very act of
    // being corrected.
    use crate::message::TuiMessage;
    let mut subject = app();
    subject.push_message(TuiMessage::System {
        text: "a row the session keeps".to_string(),
        timestamp: chrono::Utc::now(),
    });
    let id = subject.push_queued("/clear".to_string());
    subject.queue.queue_mode = QueueMode::Focused;
    subject.queue.queue_view = 0;
    assert!(subject.queue_focused_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)));
    let _claimed = subject.remove_queued(id);
    assert_eq!(
        subject.queue_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Some(true),
        "the save chord reaches the open edit"
    );
    assert_eq!(
        subject.queued_len(),
        0,
        "the claimed slot's correction was delivered, not re-queued"
    );
    assert!(
        subject.conversation.iter().any(|message| match message {
            TuiMessage::User { text, .. } => text == "/clear",
            _ => false,
        }),
        "the corrected words land as the user's own"
    );
    assert!(
        subject.conversation.iter().any(|message| match message {
            TuiMessage::System { text, .. } => text == "a row the session keeps",
            _ => false,
        }),
        "the command the words spelled never ran — the history stands"
    );
}

#[test]
fn a_correction_the_refused_channel_hands_back_to_the_composer() {
    // The refused-send arm: a driver whose channel has gone silent
    // takes nothing, so the corrected words come back the way every
    // refused submit returns them — a notice naming the dead driver,
    // the words in the composer, and no transcript row for a turn
    // that never started.
    use crate::message::TuiMessage;
    let mut subject = app();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    subject.host.submit_tx = Some(tx);
    drop(rx);
    subject.push_message(TuiMessage::System {
        text: "a row the session keeps".to_string(),
        timestamp: chrono::Utc::now(),
    });
    let id = subject.push_queued("the prompt that went out".to_string());
    subject.queue.queue_mode = QueueMode::Focused;
    subject.queue.queue_view = 0;
    assert!(subject.queue_focused_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)));
    subject
        .composer
        .input
        .set_text("the corrected wording".to_string());
    let _claimed = subject.remove_queued(id);
    assert_eq!(
        subject.queue_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Some(true),
        "the save chord reaches the open edit"
    );
    assert_eq!(
        subject.queued_len(),
        0,
        "the refused send re-queued nothing"
    );
    assert!(
        subject.conversation.iter().all(|message| !matches!(
            message,
            TuiMessage::User { text, .. } if text == "the corrected wording"
        )),
        "no transcript row for words that never left"
    );
    assert_eq!(
        subject.composer.input.text(),
        "the corrected wording",
        "the words come back to the composer"
    );
    assert!(
        subject
            .notice_text()
            .as_deref()
            .is_some_and(|notice| notice.contains("the driver has gone")),
        "the notice names the dead driver"
    );
}

#[test]
fn the_cost_command_agrees_with_the_bar_at_an_odd_window() {
    // `/cost` reads the same snapshot the context bar draws from and
    // rounds it the same way, so the two figures a user can see side by
    // side never disagree.
    // The window is odd on purpose — an even split needs no rounding
    // and would hide the half-up the two computations must share — and
    // the usage crosses the window, the overshoot case where the old
    // clamped bar and the command's unclamped number parted ways.
    let mut subject = app();
    subject
        .state
        .context_window
        .store(3, std::sync::atomic::Ordering::SeqCst);
    subject
        .state
        .tokens
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .context_tokens = 7;
    subject.cost_command();
    let report = subject
        .conversation
        .iter()
        .rev()
        .find_map(|message| match message {
            TuiMessage::System { text, .. } if text.starts_with("Context: ") => Some(text.clone()),
            _ => None,
        })
        .expect("the cost report landed");
    let (bar_percent, bar_counts) = context_bar_readout(7, 3).expect("the bar reads a live window");
    let command_percent = report
        .strip_prefix("Context: 7 of 3 tokens used (")
        .and_then(|rest| rest.strip_suffix("% full)."))
        .unwrap_or_default();
    assert_eq!(
        command_percent,
        bar_percent.trim_end_matches('%'),
        "the command's percent and the bar's percent are the same figure: {report} vs \
         {bar_percent}/{bar_counts}"
    );
}

#[test]
fn a_live_model_switch_that_fails_its_save_says_it_stands_live() {
    // A model switch is live the moment the loop takes it, so a failed
    // config save must not claim the value "was never applied" — the
    // restart-class phrasing every other key's failure carries. The save
    // is made to fail against a path
    // a file already occupies, and the notice names the truth: the
    // switch is live, the save is what failed.
    let mut subject = app();
    let occupied = tempfile::NamedTempFile::new().expect("a file where the dir should be");
    subject.config_writes.config_persist_dir = Some(occupied.path().to_path_buf());
    subject.persist_config("model");
    let notice = subject.notice_text().unwrap_or_default();
    assert!(
        notice.starts_with("the switch is live but not saved — the config save failed: "),
        "the failure names the live switch, not a value never applied: {notice}"
    );
}

#[test]
fn an_empty_session_catalog_answers_with_a_notice() {
    // `/sessions` against a catalog with nothing in it says so on the
    // notice row — the user asked and deserves an answer, not a blank
    // screen — and seats no picker there is nothing to seat.
    let mut subject = app();
    subject.open_session_picker(Vec::new());
    assert_eq!(
        subject.notice_text().as_deref(),
        Some("no saved sessions yet — one is saved on exit"),
        "the empty catalog answers aloud"
    );
    assert!(
        subject.pickers.session_picker.is_none(),
        "no picker opens over an empty catalog"
    );
}

#[test]
fn the_queued_and_todo_sections_share_their_left_margin() {
    // The two standing panels above the composer are one family of
    // furniture: the QUEUED badge and the TODO badge sit at the same
    // column, and the queued prompt's rows start where the plan's rows
    // do. A section painted flush against its pane's edge while its
    // sibling insets reads as a stray strip, not a named region.
    use crate::todo_view::{TodoRow, TodoStatus};
    let mut subject = app();
    subject.plan.latest_todos = vec![TodoRow {
        id: "t1".to_string(),
        subject: "align the standing panels".to_string(),
        status: TodoStatus::InProgress,
    }];
    for text in ["first queued", "second queued"] {
        subject.push_queued(text.to_string());
    }
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| subject.render(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let first_marked_column = |row: u16| {
        (0..buffer.area.width).find(|&x| {
            buffer
                .cell((x, row))
                .is_some_and(|cell| cell.symbol() != " " && cell.symbol() != "")
        })
    };
    let todo_row = (0..buffer.area.height)
        .find(|&y| {
            (0..buffer.area.width).any(|x| {
                buffer.cell((x, y)).is_some_and(|cell| cell.symbol() == "T")
                    && buffer
                        .cell((x.saturating_add(1), y))
                        .is_some_and(|cell| cell.symbol() == "O")
            })
        })
        .expect("the TODO badge renders");
    let queued_row = (0..buffer.area.height)
        .find(|&y| {
            (0..buffer.area.width).any(|x| {
                buffer.cell((x, y)).is_some_and(|cell| cell.symbol() == "Q")
                    && buffer
                        .cell((x.saturating_add(1), y))
                        .is_some_and(|cell| cell.symbol() == "U")
            })
        })
        .expect("the QUEUED badge renders");
    assert_ne!(todo_row, queued_row, "the two badges are separate rows");
    assert_eq!(
        first_marked_column(todo_row),
        first_marked_column(queued_row),
        "the TODO and QUEUED badges sit at the same column"
    );
}

#[test]
fn a_strip_that_stands_down_clears_its_row_before_the_composer() {
    // The frame buffer is cumulative: a cell no widget repaints keeps
    // the previous frame's content. The pulse strip owns the spacer row
    // above the composer while the run lives, painting its bar as
    // foreground glyphs in the theme's strip color; the frame the run
    // ends it must clear the row, not merely stop painting it — the
    // last bar (or, after a theme switch, the previous palette's
    // glyphs) would otherwise stand beside the composer forever, a
    // leak every light theme wears visibly. Pinned on a direct
    // terminal, where no canvas walk exists to refill the row.
    let mut subject = app();
    subject.theme = Theme::by_name("github_light").unwrap();
    subject.push_message(TuiMessage::User {
        text: "hi".to_string(),
        timestamp: chrono::Utc::now(),
        steered: false,
        unsent: false,
    });
    subject
        .state
        .agent_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).expect("terminal");
    terminal.draw(|frame| subject.render(frame)).expect("draw");
    let strip_color = subject.theme.ui.primary;
    let strip_glyph = |symbol: &str| matches!(symbol, "\u{2500}" | "\u{2574}" | "\u{2576}");
    let strip_row = (0..24u16).find(|&y| {
        (0..80u16).any(|x| {
            terminal
                .backend()
                .buffer()
                .cell((x, y))
                .is_some_and(|cell| strip_glyph(cell.symbol()) && cell.fg == strip_color)
        })
    });
    let strip_row = strip_row.expect("the running strip paints its row's glyphs");
    subject
        .state
        .agent_running
        .store(false, std::sync::atomic::Ordering::SeqCst);
    terminal.draw(|frame| subject.render(frame)).expect("draw");
    let leftover: Vec<u16> = (0..80u16)
        .filter(|&x| {
            terminal
                .backend()
                .buffer()
                .cell((x, strip_row))
                .is_some_and(|cell| strip_glyph(cell.symbol()) && cell.fg == strip_color)
        })
        .collect();
    assert!(
        leftover.is_empty(),
        "the idle frame clears the strip's row {strip_row} — no bar lingers beside the composer: {leftover:?}"
    );
    let cleared = (0..80u16).all(|x| {
        terminal
            .backend()
            .buffer()
            .cell((x, strip_row))
            .is_some_and(|cell| cell.symbol() == " " && cell.fg == ratatui::style::Color::Reset)
    });
    assert!(
        cleared,
        "the stood-down row {strip_row} returns blank on a direct terminal"
    );
}

#[test]
fn a_boundary_mode_update_moves_the_display_without_the_file() {
    // A `/clear`'s fresh session falls back to the configured
    // default, and the display hears it over the update lane: the
    // live config moves, the file copy never does.
    let mut app = TuiApp::new(dch_config::DchConfig::default());
    app.apply_session_update(crate::queue::SessionUpdate::PermissionMode {
        mode: dch_config::PermissionMode::ReadOnly,
    });
    assert_eq!(
        app.config.runner.permission_mode,
        dch_config::PermissionMode::ReadOnly,
        "the display answers /mode for the mode actually in force"
    );
}
