//! The finished run's final answer renders without any input.
//!
//! The incident shape: the run ends after a thinking-then-answer turn
//! and the pane stops at the thinking row; the answer only appears
//! when the user types. A draw is the only thing that lands a
//! graduation, so this pin lets the run finish, waits out several
//! caret-blink frames with no input at all, and then asserts the
//! answer is on the screen — if a wake can be lost, the blink tick
//! must still carry the frame.
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]

mod common;

use common::sse_thinking_text_turn;
use common::sse_tool_call_turn;
use serde_json::json;
use std::time::Duration;

#[test]
fn the_final_answer_renders_with_no_input_after_the_run_ends() {
    let answer = "## Settled report\n\nThe settle-probe answer must appear unaided: \
        the run closed, the blink frames owe the repaint, and no keystroke \
        arrives to rescue it.\n\n- finding one\n- finding two";
    let sb = common::Sandbox::with_server(common::CannedServer::sse(vec![
        sse_tool_call_turn("Read", &json!({"path": "notes.txt"})),
        sse_thinking_text_turn("closing out the turn now", answer),
    ]));
    std::fs::write(sb.workdir().join("notes.txt"), "some notes\n").expect("fixture");

    let session = sb.dch_pty(&[]);
    let mut session = session;
    session.expect("test-model", Duration::from_secs(20));
    session.send(b"run it");
    session.send(b"\r");
    // Wait for the run to close on a marker streaming can never show:
    // the reply's duration line graduates only with the run's end, so
    // answer text alone — which the live stream also paints — proves
    // nothing about the post-run repaint this pin exists for.
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    loop {
        let screen = session.snapshot();
        if screen_shows_graduated_duration(&screen) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the pty never showed the graduated duration line; screen so far:\n{screen}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Now idle long enough for several blink frames (500 ms each) with
    // no input, then snapshot: the answer must already be visible.
    std::thread::sleep(Duration::from_millis(2500));
    let screen = session.snapshot();
    let plain = {
        let mut out = String::new();
        let mut chars = screen.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    };
    // The assertion matches the answer's words in order rather than
    // as a spaced phrase: a diff-driven repaint can position words by
    // cursor move instead of space glyphs, and the stripper cannot
    // recover those — the same matching rule the pty harness's own
    // `expect` uses.
    let haystack = plain.to_lowercase();
    let mut search_from = 0;
    for word in "must appear unaided".split_whitespace() {
        match haystack.get(search_from..).and_then(|rest| rest.find(word)) {
            Some(at) => {
                search_from = search_from.saturating_add(at).saturating_add(word.len());
            }
            None => panic!("the answer renders with zero input after the run ends: {plain:?}"),
        }
    }
    session.quit_and_finish(Duration::from_secs(20));
}

/// Whether the screen shows a reply's graduated duration span.
///
/// The duration line graduates only with the run's end — the live
/// stream never paints it — so it is the one marker that proves the
/// engine finished rather than merely that answer text arrived. The
/// snapshot is a raw cursor-addressed stream, not reconstructed rows,
/// so the scan reads the span's own shape on the wire: an SGR
/// terminator (`m`), the duration glyphs (`<1s`, `4s`, `2m 30s`), and
/// the escape that closes the standalone span — a shape the live
/// thinking row's `· <1s`, preceded by a space, cannot match.
fn screen_shows_graduated_duration(screen: &str) -> bool {
    screen.match_indices('m').any(|(index, _)| {
        let Some(after_style) = index
            .checked_add(1)
            .and_then(|after_style| screen.get(after_style..))
        else {
            return false;
        };
        let rest = after_style.strip_prefix('<').unwrap_or(after_style);
        let measured = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == ' ')
            .count();
        measured > 0
            && rest
                .get(measured..)
                .is_some_and(|suffix| suffix.starts_with('s'))
            && rest
                .get(measured.saturating_add(1)..)
                .is_some_and(|suffix| suffix.starts_with('\u{1b}'))
    })
}
