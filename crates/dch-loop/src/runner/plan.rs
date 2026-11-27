//! Seeding the standing todo plan from the history a resumed session
//! restores.

use std::sync::Arc;

use loopctl::message::Message;
use loopctl::message::MessagePart;

/// Seed the todo store from the history a resume installed.
///
/// Walks the restored conversation's tool results backwards for the
/// last `TodoWrite` render; a list holding unfinished entries becomes
/// the standing plan — the prior session's open work carries, ids and
/// rungs intact — while a completed list, an empty plan notice, or no
/// list at all leaves the store empty: a finished plan retires with
/// its run. Fresh sessions pass an empty history and seed nothing.
pub(super) fn seed_standing_plan(
    context: &Arc<dch_tools::context::RunnerContext>,
    history: &[Message],
) {
    let Some(render) = history.iter().rev().find_map(|message| {
        message.parts.iter().rev().find_map(|part| match part {
            MessagePart::ToolResult {
                name,
                output,
                is_error,
                ..
            } if name == "TodoWrite" && !matches!(is_error, Some(true)) => Some(output.to_string()),
            _ => None,
        })
    }) else {
        return;
    };
    let Some(entries) = dch_tools::todo::parse_task_list_render(&render) else {
        return;
    };
    if entries
        .iter()
        .any(|entry| !matches!(entry.status, dch_tools::TodoStatus::Completed))
    {
        *context
            .todos
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = entries;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::test_support::sample_context;

    #[test]
    fn seed_standing_plan_adopted_iff_undone() {
        // The resume half: a restored history whose last TodoWrite render
        // holds undone entries seeds the store; a completed list (or no
        // list at all) leaves it empty — the finished plan retired with
        // its session, the unfinished one carried across the process
        // boundary.
        let undone_render = "# Task List\n\nSummary: 1 pending, 0 in progress, 0 \
             completed\n\n○ [only] the work\n\n";
        let undone_history = vec![
            loopctl::message::Message::user("do it"),
            loopctl::message::Message::new(
                loopctl::message::Role::User,
                vec![loopctl::message::MessagePart::tool_result(
                    "c1",
                    "TodoWrite",
                    undone_render,
                    false,
                )],
            ),
        ];
        let context = sample_context("/tmp/tx51-seed-undone");
        seed_standing_plan(&context, &undone_history);
        let store = context.todos.lock().expect("todos lock").clone();
        assert_eq!(
            store.len(),
            1,
            "the undone plan seeds the resumed session's store"
        );
        assert_eq!(
            store.first().map(|entry| entry.id.as_str()),
            Some("only"),
            "ids and statuses carry from the render"
        );

        let completed_render = "# Task List\n\nSummary: 0 pending, 0 in progress, 1 \
             completed\n\n● [done] the work\n\n";
        let completed_history = vec![loopctl::message::Message::new(
            loopctl::message::Role::User,
            vec![loopctl::message::MessagePart::tool_result(
                "c2",
                "TodoWrite",
                completed_render,
                false,
            )],
        )];
        let clean = sample_context("/tmp/tx51-seed-complete");
        seed_standing_plan(&clean, &completed_history);
        assert!(
            clean.todos.lock().expect("todos lock").is_empty(),
            "a completed plan retires — nothing seeds"
        );

        let bare = sample_context("/tmp/tx51-seed-bare");
        seed_standing_plan(&bare, &[]);
        assert!(
            bare.todos.lock().expect("todos lock").is_empty(),
            "a history with no plan seeds nothing"
        );
    }
}
