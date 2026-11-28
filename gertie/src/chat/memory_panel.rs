//! The memory panel (#54 follow-up): everything Gertie remembers here,
//! as sentences you can read, with where each came from, what it
//! means, and buttons that say what they do. Grouped by topic file,
//! entries that need a look first, retired ones folded at the end.

use super::*;
use crate::ink;

/// The panel's width beside the work side.
const PANEL_WIDTH: f32 = 440.;

/// A topic file's name as a heading: `build-and-test.md` reads
/// "Build and test".
fn topic_title(file: &str) -> String {
    let stem = file.trim_end_matches(".md").replace(['-', '_'], " ");
    let mut chars = stem.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "Notes".to_string(),
    }
}

/// `2026-11-28` as "Nov 28"; anything else as written.
fn short_date(date: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let parts: Vec<&str> = date.split('-').collect();
    match parts.as_slice() {
        [_, month, day] => match (month.parse::<usize>(), day.parse::<u32>()) {
            (Ok(month), Ok(day)) if (1..=12).contains(&month) => {
                format!("{} {day}", MONTHS[month - 1])
            }
            _ => date.to_string(),
        },
        _ => date.to_string(),
    }
}

/// A memory's text, clipped for a reference inside another's line.
fn clip_words(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max).collect::<String>().trim_end())
    }
}

impl ChatView {
    /// Where an entry came from, in words.
    fn memory_origin(&self, entry: &gertie_core::MemoryEntry) -> String {
        let when = entry.added_at.as_deref().map(short_date);
        let from = match entry.source_session.as_deref() {
            Some(gertie_core::memory::CORRECTION_SOURCE) => {
                Some("a correction you gave".to_string())
            }
            Some(source) => Some(
                source
                    .parse::<u64>()
                    .ok()
                    .and_then(|id| self.sessions.iter().find(|session| session.id.0 == id))
                    .map(|session| format!("learned in \u{201c}{}\u{201d}", session.title))
                    .unwrap_or_else(|| "learned in an earlier chat".to_string()),
            ),
            None => None,
        };
        match (from, when) {
            (Some(from), Some(when)) => format!("{from}, {when}"),
            (Some(from), None) => from,
            (None, Some(when)) => format!("added {when}"),
            (None, None) => String::new(),
        }
    }

    /// The memory panel: a header that says what this is, then the
    /// entries.
    pub(super) fn render_memory_panel(&self, cx: &Context<Self>) -> AnyElement {
        let pal = ink::palette(cx);
        let current: Vec<&gertie_core::MemoryEntry> = self
            .memory_rows
            .iter()
            .filter(|entry| entry.valid_until.is_none())
            .collect();
        let retired: Vec<&gertie_core::MemoryEntry> = self
            .memory_rows
            .iter()
            .filter(|entry| entry.valid_until.is_some())
            .collect();
        let flagged = |entry: &&gertie_core::MemoryEntry| {
            !entry.recheck.is_empty() || self.memory_drift.contains_key(&entry.id)
        };
        let needs_look: Vec<&gertie_core::MemoryEntry> =
            current.iter().copied().filter(flagged).collect();
        let about_you = current
            .iter()
            .filter(|entry| entry.id.starts_with('g'))
            .count();
        let about_project = current.len() - about_you;

        // Topic sections, project files first in first-seen order, then
        // the user's own tier.
        let mut topics: Vec<(String, Vec<&gertie_core::MemoryEntry>)> = Vec::new();
        for entry in current.iter().copied().filter(|entry| !flagged(entry)) {
            let key = if entry.id.starts_with('g') {
                "About you".to_string()
            } else {
                topic_title(&entry.file)
            };
            match topics.iter_mut().find(|(title, _)| *title == key) {
                Some((_, entries)) => entries.push(entry),
                None => topics.push((key, vec![entry])),
            }
        }
        topics.sort_by_key(|(title, _)| title == "About you");

        let heading = |title: String, count: usize, color| {
            h_flex()
                .px_5()
                .pt_5()
                .pb_1()
                .gap_2()
                .items_baseline()
                .child(
                    div()
                        .font_family(ink::SERIF)
                        .text_size(px(15.))
                        .text_color(color)
                        .child(title),
                )
                .child(
                    div()
                        .font_family(ink::SANS)
                        .text_size(px(11.5))
                        .text_color(pal.text_past)
                        .child(count.to_string()),
                )
        };

        let mut body = v_flex().pb_6();
        if current.is_empty() {
            body = body.child(
                div()
                    .px_5()
                    .pt_6()
                    .font_family(ink::SERIF)
                    .text_size(px(15.))
                    .line_height(relative(1.6))
                    .text_color(pal.text_muted)
                    .child(
                        "Nothing yet. When a chat ends, Gertie may propose what's worth \
                         keeping; you'll see it in the words, with keep or let it go. You \
                         can also just tell it to remember something.",
                    ),
            );
        }
        if !needs_look.is_empty() {
            body = body
                .child(heading(
                    "Needs a look".into(),
                    needs_look.len(),
                    pal.vermilion,
                ))
                .children(
                    needs_look
                        .iter()
                        .map(|entry| self.render_memory_entry(entry, cx)),
                );
        }
        for (title, entries) in topics {
            body = body
                .child(heading(title, entries.len(), pal.text))
                .children(
                    entries
                        .iter()
                        .map(|entry| self.render_memory_entry(entry, cx)),
                );
        }
        if !retired.is_empty() {
            let open = self.memory_show_retired;
            body = body.child(
                h_flex()
                    .id("memory-retired-toggle")
                    .test_support()
                    .px_5()
                    .pt_5()
                    .pb_1()
                    .gap_2()
                    .cursor_pointer()
                    .font_family(ink::SERIF)
                    .text_size(px(14.))
                    .text_color(pal.text_muted)
                    .hover(|style| style.text_color(pal.text))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.memory_show_retired = !this.memory_show_retired;
                        cx.notify();
                    }))
                    .child(format!(
                        "{} replaced or forgotten ({})",
                        if open { "▾" } else { "▸" },
                        retired.len()
                    )),
            );
            if open {
                body = body.children(
                    retired
                        .iter()
                        .map(|entry| self.render_memory_entry(entry, cx)),
                );
            }
        }

        let summary = match (about_project, about_you) {
            (0, 0) => String::new(),
            (project, 0) => format!("{project} about this project"),
            (0, you) => format!("{you} about you"),
            (project, you) => format!("{project} about this project, {you} about you"),
        };
        let header = v_flex()
            .flex_shrink_0()
            .px_5()
            .pt_5()
            .pb_4()
            .gap_2()
            .border_b_1()
            .border_color(pal.line)
            .child(
                h_flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .font_family(ink::SERIF)
                            .text_size(px(21.))
                            .text_color(pal.text)
                            .child("What Gertie remembers"),
                    )
                    .child(
                        Button::new("memory-close")
                            .icon(IconName::Close)
                            .small()
                            .ghost()
                            .tooltip("Close")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.panel = Panel::None;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .font_family(ink::SANS)
                    .text_size(px(13.))
                    .line_height(relative(1.55))
                    .text_color(pal.text_muted)
                    .child(
                        "Gertie reads these before it works here, and recalls the ones that \
                         fit as it goes. Mark what's still true, fix what's wrong, forget \
                         what's stale.",
                    ),
            )
            .when(!summary.is_empty(), |header| {
                header.child(
                    div()
                        .font_family(ink::SANS)
                        .text_size(px(12.))
                        .text_color(pal.text_past)
                        .child(summary),
                )
            });
        let folder = self
            .memory_store
            .as_ref()
            .map(|store| store.root(gertie_core::Tier::Project).display().to_string());
        let footer = folder.map(|folder| {
            div()
                .flex_shrink_0()
                .px_5()
                .py_3()
                .border_t_1()
                .border_color(pal.line)
                .font_family(ink::SANS)
                .text_size(px(11.5))
                .text_color(pal.text_past)
                .child(format!("Kept as plain files in {folder}"))
        });
        v_flex()
            .id("memory-panel")
            .test_support()
            .w(px(PANEL_WIDTH))
            .h_full()
            .flex_shrink_0()
            .bg(pal.paper)
            .border_l_1()
            .border_color(pal.line)
            .child(header)
            .child(
                div()
                    .id("memory-panel-rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(body),
            )
            .children(footer)
            .into_any_element()
    }

    /// One memory: its words, where it came from, what's going on with
    /// it, and what you can do.
    fn render_memory_entry(
        &self,
        entry: &gertie_core::MemoryEntry,
        cx: &Context<Self>,
    ) -> AnyElement {
        let pal = ink::palette(cx);
        let id = entry.id.clone();
        let retired = entry.valid_until.is_some();
        let flagged = !entry.recheck.is_empty() && !retired;
        let text_of = |other: &str| {
            self.memory_rows
                .iter()
                .find(|row| row.id == other)
                .map(|row| clip_words(&row.text, 48))
                .unwrap_or_else(|| other.to_string())
        };

        // Where it came from, and what you've said about it.
        let mut meta = vec![self.memory_origin(entry)];
        if entry.helpful > 0 {
            meta.push(format!("marked true {}×", entry.helpful));
        }
        if entry.harmful > 0 {
            meta.push(format!("marked wrong {}×", entry.harmful));
        }
        if let Some(until) = entry.valid_until.as_deref() {
            meta.push(format!("retired {}", short_date(until)));
        }
        let meta = meta
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");

        // How it relates to others, in their words, not their ids.
        let mut structure = Vec::new();
        let chain = memory_chain(&self.memory_rows, &entry.id);
        if let Some(last) = chain.last() {
            structure.push(format!("Replaced by: {}", text_of(last)));
        }
        for link in &entry.links {
            structure.push(format!(
                "{}: {}",
                link.relation.replace('_', " "),
                text_of(&link.to)
            ));
        }
        let drift = self.memory_drift.get(&entry.id).map(|changed| {
            format!(
                "The files it was based on changed since: {}. Is it still true?",
                changed.join(", ")
            )
        });

        let action = |name: String, label: &'static str, tip: &'static str| {
            div()
                .id(SharedString::from(name))
                .test_support()
                .cursor_pointer()
                .text_color(pal.text_muted)
                .hover(|style| style.text_color(pal.text))
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                .child(label)
        };
        let editing = self
            .memory_edit
            .as_ref()
            .filter(|(editing, _)| *editing == entry.id)
            .map(|(_, input)| input.clone());

        let words = match editing {
            Some(input) => v_flex()
                .gap_2()
                .on_action(
                    cx.listener(|this, _: &gpui_kit::component::input::Enter, _, cx| {
                        this.save_memory_edit(cx);
                    }),
                )
                .child(Input::new(&input).small().w_full())
                .child(
                    h_flex()
                        .gap_3()
                        .font_family(ink::SANS)
                        .text_size(px(12.))
                        .child(
                            action(
                                format!("memory-panel-save-{id}"),
                                "save",
                                "Keep these words",
                            )
                            .text_color(pal.text)
                            .on_click(cx.listener(|this, _, _, cx| this.save_memory_edit(cx))),
                        )
                        .child(
                            action(
                                format!("memory-panel-cancel-{id}"),
                                "cancel",
                                "Leave it as it was",
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.memory_edit = None;
                                cx.notify();
                            })),
                        ),
                )
                .into_any_element(),
            None => div()
                .font_family(ink::SERIF)
                .text_size(px(15.))
                .line_height(relative(1.5))
                .text_color(if retired { pal.text_past } else { pal.text })
                .when(retired, |text| text.line_through())
                .child(entry.text.clone())
                .into_any_element(),
        };

        let flag = flagged.then(|| {
            let walk = gertie_core::correction_walk(&self.memory_rows, &entry.id)
                .iter()
                .map(|step| step.sentence())
                .collect::<Vec<_>>()
                .join(" ");
            v_flex()
                .gap_1p5()
                .px_3()
                .py_2()
                .rounded(px(10.))
                .border_1()
                .border_color(pal.vermilion)
                .font_family(ink::SANS)
                .text_size(px(12.))
                .line_height(relative(1.5))
                .child(
                    div()
                        .text_color(pal.text)
                        .child(format!("Something it rests on was corrected. {walk}")),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .child(
                            action(
                                format!("memory-keep-{id}"),
                                "still holds",
                                "It's still true after the correction: clear the flag",
                            )
                            .text_color(pal.text)
                            .on_click(cx.listener({
                                let id = id.clone();
                                move |this, _, _, cx| this.settle_recheck(id.clone(), true, cx)
                            })),
                        )
                        .child(
                            action(
                                format!("memory-archive-{id}"),
                                "forget it",
                                "It no longer holds: stop recalling it (the line stays in the file's history)",
                            )
                            .on_click(cx.listener({
                                let id = id.clone();
                                move |this, _, _, cx| this.settle_recheck(id.clone(), false, cx)
                            })),
                        ),
                )
        });

        let actions = (!retired && !flagged && self.memory_edit.is_none()).then(|| {
            h_flex()
                .gap_4()
                .font_family(ink::SANS)
                .text_size(px(12.))
                .child(
                    action(
                        format!("memory-vote-up-{id}"),
                        "still true",
                        "Counts in its favor: recalled more readily, and kept by the weekly cleanup",
                    )
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            this.vote_memory(id.clone(), true, cx);
                            cx.notify();
                        }
                    })),
                )
                .child(
                    action(
                        format!("memory-vote-down-{id}"),
                        "wrong",
                        "Counts against it: recalled less, and enough of these and the cleanup drops it",
                    )
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            this.vote_memory(id.clone(), false, cx);
                            cx.notify();
                        }
                    })),
                )
                .child(
                    action(format!("memory-panel-edit-{id}"), "edit", "Change its words")
                        .on_click(cx.listener({
                            let id = id.clone();
                            let text = entry.text.clone();
                            move |this, _, window, cx| {
                                let text = text.clone();
                                let input = cx
                                    .new(|cx| InputState::new(window, cx).default_value(text));
                                input.update(cx, |state, cx| state.focus(window, cx));
                                this.memory_edit = Some((id.clone(), input));
                                cx.notify();
                            }
                        })),
                )
                .child(
                    action(
                        format!("memory-panel-forget-{id}"),
                        "forget",
                        "Stop recalling it now (the line stays in the file's history)",
                    )
                    .on_click(cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| this.forget_memory(id.clone(), cx)
                    })),
                )
        });

        v_flex()
            .id(SharedString::from(format!("memory-row-{id}")))
            .test_support()
            .mx_2()
            .px_3()
            .py_2p5()
            .gap_1p5()
            .rounded(px(12.))
            .hover(|style| style.bg(pal.paper_raised))
            .child(words)
            .when(!meta.is_empty(), |row| {
                row.child(
                    div()
                        .font_family(ink::SANS)
                        .text_size(px(11.5))
                        .text_color(pal.text_past)
                        .child(meta),
                )
            })
            .when(!structure.is_empty(), |row| {
                row.child(
                    div()
                        .id(SharedString::from(format!("memory-structure-{id}")))
                        .test_support()
                        .font_family(ink::SANS)
                        .text_size(px(12.))
                        .line_height(relative(1.5))
                        .text_color(pal.text_muted)
                        .child(structure.join(" · ")),
                )
            })
            .when_some(drift, |row, drift| {
                row.child(
                    div()
                        .id(SharedString::from(format!("memory-drift-{id}")))
                        .test_support()
                        .font_family(ink::SANS)
                        .text_size(px(12.))
                        .line_height(relative(1.5))
                        .text_color(pal.vermilion)
                        .child(drift),
                )
            })
            .children(flag)
            .children(actions)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_files_read_as_headings_and_dates_as_words() {
        assert_eq!(topic_title("build-and-test.md"), "Build and test");
        assert_eq!(topic_title("gotchas.md"), "Gotchas");
        assert_eq!(short_date("2026-11-28"), "Nov 28");
        assert_eq!(short_date("someday"), "someday");
    }
}
