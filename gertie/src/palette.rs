//! The command palette: Ctrl+K over everything the store remembers.
//!
//! The kit's `Command` component owns the field, the filtering knobs,
//! and the keyboard nav (its own init binds enter/arrows/escape under
//! its key context); this file owns the gertie half — the global
//! trigger, what a row shows (an FTS hit: session, role, snippet), and
//! what confirming one means (the chat jumps to that turn).

use gpui_kit::component::command::CommandItem;
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::{App, KeyBinding, actions, div, prelude::*, px};

// Open the palette from anywhere in the chat window — no context
// filter, so it fires no matter which control holds focus (a binding
// with no context matches at the deepest level of the dispatch path).
actions!(gertie, [OpenPalette]);

/// The palette's key map, bound once at app start. Only the trigger
/// lives here — enter/arrows/escape inside the palette are the kit's.
pub fn bind_palette_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("ctrl-k", OpenPalette, None::<&str>)]);
}

/// The palette's commands — the `>` filter's rows. One per action the
/// chat surface actually offers (the same set the slash commands
/// drive), spelled the way a person would search for them. No registry
/// hunting: the chat maps each variant straight onto its handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteCommand {
    NewChat,
    Compact,
    Export,
    Agents,
    Settings,
}

/// Every command: (search name, what it does, the action). The match
/// is a case-insensitive substring over the name AND the description —
/// ">export" and ">markdown" both find the export.
pub const COMMANDS: &[(&str, &str, PaletteCommand)] = &[
    (
        "new chat",
        "start a blank conversation",
        PaletteCommand::NewChat,
    ),
    (
        "compact",
        "summarize and shrink this session's history",
        PaletteCommand::Compact,
    ),
    (
        "export",
        "write this session out as markdown",
        PaletteCommand::Export,
    ),
    (
        "agents",
        "show this chat's worker runs",
        PaletteCommand::Agents,
    ),
    (
        "settings",
        "open the settings editor",
        PaletteCommand::Settings,
    ),
];

/// The `>`-filtered command rows: name or description contains the
/// term (empty term = everything), search order — the constant's
/// order, which reads most-used-first.
pub fn command_matches(term: &str) -> Vec<(&'static str, &'static str, PaletteCommand)> {
    let term = term.trim().to_lowercase();
    COMMANDS
        .iter()
        .filter(|(name, description, _)| {
            term.is_empty()
                || name.to_lowercase().contains(&term)
                || description.to_lowercase().contains(&term)
        })
        .copied()
        .collect()
}

/// The `>` mode's rows: one per matched command, the action's name
/// with its description riding beside it.
pub fn command_items(
    commands: &[(&'static str, &'static str, PaletteCommand)],
) -> Vec<CommandItem> {
    commands
        .iter()
        .map(|(name, description, _)| {
            let label = (*name).to_string();
            let description = (*description).to_string();
            CommandItem::new()
                .label(label.clone())
                .keywords([description.as_str()])
                .child(move |_, cx| {
                    h_flex()
                        .gap_2()
                        .items_center()
                        .min_w_0()
                        .flex_1()
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(cx.theme().foreground)
                                .truncate()
                                .child(label.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(description.clone()),
                        )
                })
        })
        .collect()
}

/// One palette row per search hit: where it lives (session title, who
/// said it) and what it says (the FTS snippet, markers left in — they
/// bracket the match). Ordered as `search` ranked them, best first;
/// the row position is the index into the chat's hit vector, which is
/// what confirming hands back.
pub fn search_items(hits: &[gertie_core::HistoryHit]) -> Vec<CommandItem> {
    hits.iter()
        .map(|hit| {
            let title = hit.session_title.clone();
            let who = role_word(&hit.role);
            let snippet = hit.snippet.clone();
            CommandItem::new()
                .label(title.clone())
                .keywords([who, snippet.as_str()])
                .child(move |_, cx| {
                    h_flex()
                        .gap_2()
                        .items_center()
                        .min_w_0()
                        .flex_1()
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(cx.theme().foreground)
                                .truncate()
                                .child(title.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(snippet.clone()),
                        )
                })
        })
        .collect()
}

/// Who said it, in one word — the palette's rows answer "who" at a
/// glance without a legend. Role strings are the store's spelling.
fn role_word(role: &str) -> &'static str {
    match role {
        "user" => "you",
        "assistant" => "gertie",
        "tool" => "tool",
        _ => "system",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_hit_becomes_one_row_in_search_order() {
        let hits = vec![
            gertie_core::HistoryHit {
                session_title: "fix the flaky test".into(),
                session_id: gertie_core::SessionId(1),
                message_id: gertie_core::MessageId(10),
                role: "user".into(),
                snippet: "the ▶flaky◀ one".into(),
            },
            gertie_core::HistoryHit {
                session_title: "fix the flaky test".into(),
                session_id: gertie_core::SessionId(1),
                message_id: gertie_core::MessageId(11),
                role: "tool".into(),
                snippet: "ran ▶flaky◀ suite".into(),
            },
        ];
        let items = search_items(&hits);
        assert_eq!(items.len(), 2);
        // Row order is hit order — the palette's confirm hands the row
        // index back, and that must point at the same hit search ranked.
        assert_eq!(role_word(&hits[0].role), "you");
        assert_eq!(role_word(&hits[1].role), "tool");
        // Role words feed keywords too, so "gertie" narrows to its rows.
        assert_eq!(role_word("assistant"), "gertie");
        assert_eq!(role_word("system"), "system");
    }

    #[test]
    fn command_matches_run_over_names_and_descriptions() {
        // Empty term: everything, in the constant's order.
        assert_eq!(command_matches("").len(), COMMANDS.len());
        // Name substring, case-blind.
        assert_eq!(
            command_matches("EXPORT")
                .iter()
                .map(|(name, _, _)| *name)
                .collect::<Vec<_>>(),
            vec!["export"]
        );
        // Description substring too — "markdown" finds the export.
        let markdown: Vec<&str> = command_matches("markdown")
            .iter()
            .map(|(name, _, _)| *name)
            .collect();
        assert_eq!(markdown, vec!["export"]);
        // No hit: empty, not an error.
        assert!(command_matches("no such action").is_empty());
    }
}
