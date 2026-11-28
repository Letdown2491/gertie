//! The composer pill's own controls (#54 follow-up): the posture on
//! its left, the model and this chat's memory on its right, the room
//! to think as a fill along its bottom edge. Nothing sits under the
//! pill any more. The memory button opens a popover of only what this
//! chat used and learned, each with keep, edit and forget.

use super::*;
use crate::ink;

/// The popover's width.
const MEMORY_POPOVER_WIDTH: f32 = 440.;

/// The room left, in percent, at or under which it reads as tight:
/// where compaction begins.
const ROOM_TIGHT: u64 = 25;

/// Where compaction starts, as a share of the context.
const COMPACT_AT: f32 = 0.75;

/// The endpoint label is "provider · model": the composer shows the
/// model alone, the card names the provider.
fn split_model_label(label: &str) -> (Option<&str>, &str) {
    match label.split_once(" · ") {
        Some((provider, model)) => (Some(provider), model),
        None => (None, label),
    }
}

fn model_name(label: &str) -> &str {
    split_model_label(label).1
}

/// The room to think as a small ring: the faint whole circle, and the
/// used share drawn over it from the top, clockwise.
fn room_ring(
    size: f32,
    used: f32,
    ring: gpui_kit::Hsla,
    track: gpui_kit::Hsla,
) -> impl IntoElement {
    use gpui_kit::{canvas, point};
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let ox: f32 = bounds.origin.x.into();
            let oy: f32 = bounds.origin.y.into();
            let radius = size / 2. - 1.5;
            let at = |fraction: f32| {
                let angle = (fraction * 360. - 90.).to_radians();
                (
                    ox + size / 2. + radius * angle.cos(),
                    oy + size / 2. + radius * angle.sin(),
                )
            };
            let mut arc = |from: f32, to: f32, color| {
                let mut path = super::panes::round_stroke(2.4);
                let steps = (((to - from) * 64.).ceil() as usize).max(2);
                for step in 0..=steps {
                    let (x, y) = at(from + (to - from) * step as f32 / steps as f32);
                    if step == 0 {
                        path.move_to(point(px(x), px(y)));
                    } else {
                        path.line_to(point(px(x), px(y)));
                    }
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            };
            arc(0., 1., track);
            let used = used.clamp(0., 1.);
            if used > 0.01 {
                arc(0., used.min(0.999), ring);
            }
        },
    )
    .size(px(size))
    .flex_shrink_0()
}

/// One memory as the popover shows it.
struct PopRow {
    id: String,
    file: String,
    text: String,
    helpful: u32,
}

impl ChatView {
    /// The posture, inside the pill on the left: one dot and its word.
    /// Click opens the mode menu; full access reads in vermilion.
    pub(super) fn render_pill_posture(&self, cx: &Context<Self>) -> AnyElement {
        let pal = ink::palette(cx);
        let full = self.mode == PermissionMode::FullAccess;
        let tip = mode_tooltip(self.mode);
        let color = if full { pal.vermilion } else { pal.text_muted };
        h_flex()
            .id("mode-chip")
            .relative()
            .flex_shrink_0()
            .gap_1p5()
            .items_center()
            .h(px(32.))
            .px_2()
            .rounded_full()
            .cursor_pointer()
            .font_family(ink::SANS)
            .text_size(px(12.))
            .text_color(color)
            .hover(|style| style.bg(pal.paper).text_color(pal.text))
            .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
            .on_click(cx.listener(|this, _, _, cx| this.cycle_mode(cx)))
            .child(div().size(px(7.)).rounded_full().bg(color))
            .child(posture_words(self.mode))
            .test_support()
            .children(self.mode_menu.then(|| self.render_mode_menu(cx)))
            .into_any_element()
    }

    /// The pill's right side: this chat's memory, then the model with
    /// the room left to think drawn as a ring beside it. Once the room
    /// runs tight the ring turns vermilion and the words say what is
    /// left. A click opens the model card.
    pub(super) fn render_pill_trailing(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let pal = ink::palette(cx);
        let count = self.memory_here_count();
        let memory = self.memory_store.is_some().then(|| {
            let open = self.memory_popover;
            h_flex()
                .id("memory-chip")
                .relative()
                .flex_shrink_0()
                .gap_1()
                .items_center()
                .h(px(32.))
                .px_2()
                .rounded_full()
                .cursor_pointer()
                .font_family(ink::SANS)
                .text_size(px(12.))
                .text_color(if open { pal.text } else { pal.text_muted })
                .when(open, |chip| chip.bg(pal.paper))
                .hover(|style| style.bg(pal.paper).text_color(pal.text))
                .tooltip(|window, cx| {
                    Tooltip::new("What this chat used and learned").build(window, cx)
                })
                .on_click(cx.listener(|this, _, _, cx| this.toggle_memory_popover(cx)))
                .child("memory")
                .when(count > 0, |chip| {
                    chip.child(div().text_color(pal.text).child(count.to_string()))
                })
                .test_support()
        });
        let tight = self.room_left().filter(|left| *left <= ROOM_TIGHT);
        let open = self.model_card;
        let words = self.model_words();
        let color = if tight.is_some() {
            pal.vermilion
        } else if open {
            pal.text
        } else {
            pal.text_muted
        };
        let model = h_flex()
            .id("pill-model")
            .flex_shrink(1.)
            .min_w_0()
            .gap(px(7.))
            .items_center()
            .h(px(32.))
            .px_2()
            .rounded_full()
            .cursor_pointer()
            .font_family(ink::SANS)
            .text_size(px(12.))
            .text_color(color)
            .when(open, |chip| chip.bg(pal.paper))
            .hover(|style| style.bg(pal.paper))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_model_card(cx)))
            .children(self.room_used().map(|used| {
                let ring = if tight.is_some() {
                    pal.vermilion
                } else {
                    pal.text
                };
                room_ring(14., used, ring, pal.line).into_any_element()
            }))
            .child(
                div()
                    .min_w_0()
                    .max_w(px(180.))
                    .truncate()
                    .whitespace_nowrap()
                    .child(words),
            )
            .test_support();
        h_flex()
            .flex_shrink_0()
            .gap_1()
            .items_center()
            .children(memory)
            .child(model)
            .into_any_element()
    }

    /// What the model button says: the model's name, or the room left
    /// once it runs tight.
    fn model_words(&self) -> String {
        match self.room_left().filter(|left| *left <= ROOM_TIGHT) {
            Some(left) => format!("{left}% left"),
            None => model_name(&self.endpoint_label).to_string(),
        }
    }

    /// The model button's words, for tests.
    pub fn model_words_for_test(&self) -> String {
        self.model_words()
    }

    /// The share of the context used, 0 to 1, when the window is known.
    fn room_used(&self) -> Option<f32> {
        let window = self.context_window?;
        Some(self.context_used.min(window) as f32 / window.max(1) as f32)
    }

    pub(super) fn toggle_model_card(&mut self, cx: &mut Context<Self>) {
        // The same guard as the memory popover's: the click that closed
        // it on mouse-down is not a request to reopen.
        if !self.model_card
            && self
                .model_card_closed
                .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(300))
        {
            self.model_card_closed = None;
            return;
        }
        self.model_card = !self.model_card;
        if self.model_card {
            self.memory_popover = false;
        }
        cx.notify();
    }

    /// The model card: the model, the room left to think as a bar with
    /// the point where compaction starts, and what this chat has read
    /// and written, drawn.
    pub(super) fn render_model_card(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.model_card {
            return None;
        }
        let pal = ink::palette(cx);
        let (provider, name) = split_model_label(&self.endpoint_label);
        let small = |text: String| {
            div()
                .font_family(ink::SANS)
                .text_size(px(11.5))
                .text_color(pal.text_muted)
                .child(text)
        };
        let label = |text: &'static str| {
            div()
                .flex_1()
                .font_family(ink::SANS)
                .text_size(px(12.))
                .text_color(pal.text_muted)
                .child(text)
        };
        let mut about = Vec::new();
        if let Some(provider) = provider {
            about.push(format!("via {provider}"));
        }
        if let Some(window) = self.context_window {
            about.push(format!("{} to think in", human_tokens(window)));
        }
        let header = h_flex()
            .items_baseline()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(ink::SERIF)
                    .text_size(px(17.))
                    .text_color(pal.text)
                    .child(name.to_string()),
            )
            .when(!about.is_empty(), |row| row.child(small(about.join(" · "))));

        let room = self.room_used().map(|used| {
            let left = 100 - (used * 100.).round() as u64;
            let tight = left <= ROOM_TIGHT;
            let fill = if tight { pal.vermilion } else { pal.text };
            v_flex()
                .id("model-card-room")
                .test_support()
                .gap_2()
                .child(
                    h_flex()
                        .items_baseline()
                        .child(label("room to think"))
                        .child(
                            div()
                                .font_family(ink::SANS)
                                .text_size(px(12.))
                                .text_color(if tight { pal.vermilion } else { pal.text })
                                .child(format!("{left}% left")),
                        )
                        .child(small(format!(" · {} used", human_tokens(self.context_used)))),
                )
                .child(
                    div()
                        .relative()
                        .h(px(8.))
                        .rounded(px(4.))
                        .bg(pal.line)
                        .child(
                            div()
                                .absolute()
                                .left_0()
                                .top_0()
                                .bottom_0()
                                .w(relative(used.max(0.01)))
                                .rounded(px(4.))
                                .bg(fill),
                        )
                        .child(
                            div()
                                .absolute()
                                .left(relative(COMPACT_AT))
                                .top(px(-4.))
                                .bottom(px(-4.))
                                .w(px(1.))
                                .bg(pal.text_muted),
                        ),
                )
                .child(small(format!(
                    "At the mark, Gertie compacts the conversation and keeps the last {} messages whole.",
                    gertie_core::agent_loop::KEEP_TAIL
                )))
        });

        let (input, output, cached) = self.usage;
        let spent = (input + output > 0).then(|| {
            let fresh = input.saturating_sub(cached);
            let total = (input + output).max(1) as f32;
            let parts = [
                (cached, pal.link.opacity(0.55), "read from cache"),
                (fresh, pal.link, "read fresh"),
                (output, pal.text, "wrote"),
            ];
            let bar = h_flex()
                .h(px(8.))
                .gap(px(2.))
                .rounded(px(4.))
                .overflow_hidden()
                .children(parts.iter().filter(|(count, ..)| *count > 0).map(
                    |(count, color, _)| {
                        div().h_full().w(relative(*count as f32 / total)).bg(*color)
                    },
                ));
            let legend = h_flex().flex_wrap().gap_x_3().gap_y_1().children(
                parts
                    .iter()
                    .filter(|(count, ..)| *count > 0)
                    .map(|(count, color, words)| {
                        h_flex()
                            .gap(px(5.))
                            .items_center()
                            .child(div().size(px(8.)).rounded(px(2.)).bg(*color))
                            .child(small(format!("{words} {}", human_tokens(*count))))
                    }),
            );
            v_flex()
                .id("model-card-spent")
                .test_support()
                .gap_2()
                .child(h_flex().child(label("this chat so far")))
                .child(bar)
                .child(legend)
        });
        let nothing = room.is_none() && spent.is_none();

        Some(
            v_flex()
                .id("model-card")
                .test_support()
                .occlude()
                .absolute()
                .bottom_full()
                .right_0()
                .mb_2()
                .w(px(380.))
                .p_4()
                .gap_4()
                .rounded(px(14.))
                .border_1()
                .border_color(pal.control)
                .bg(pal.paper_raised)
                .shadow_lg()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    if this.model_card {
                        this.model_card = false;
                        this.model_card_closed = Some(std::time::Instant::now());
                        cx.notify();
                    }
                }))
                .child(header)
                .children(room)
                .children(spent)
                .when(nothing, |card| {
                    card.child(small("Nothing sent yet in this chat.".to_string()))
                })
                .into_any_element(),
        )
    }

    /// The share of the context still free, when known.
    fn room_left(&self) -> Option<u64> {
        let window = self.context_window.filter(|_| self.context_used > 0)?;
        let used = self.context_used.min(window) * 100 / window.max(1);
        Some(100 - used)
    }

    /// What this chat learned and used, as ids: learned first (entries
    /// whose provenance is this session), then consulted ones.
    fn memory_here(&self) -> (Vec<String>, Vec<String>) {
        let session = self.session_id.map(|id| id.0.to_string());
        let learned: Vec<String> = self
            .memory_rows
            .iter()
            .filter(|entry| {
                entry.valid_until.is_none()
                    && session.is_some()
                    && entry.source_session.as_deref() == session.as_deref()
            })
            .map(|entry| entry.id.clone())
            .collect();
        let used: Vec<String> = self
            .status_facts
            .consulted
            .iter()
            .filter(|id| !learned.contains(id))
            .cloned()
            .collect();
        (learned, used)
    }

    fn memory_here_count(&self) -> usize {
        let (learned, used) = self.memory_here();
        learned.len() + used.len()
    }

    pub(super) fn toggle_memory_popover(&mut self, cx: &mut Context<Self>) {
        // A click on the button while open closes the popover on its
        // mouse-down (outside the popover); the click that follows is
        // the same gesture, not a request to reopen.
        if !self.memory_popover
            && self
                .memory_popover_closed
                .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(300))
        {
            self.memory_popover_closed = None;
            return;
        }
        self.memory_popover = !self.memory_popover;
        self.memory_edit = None;
        if self.memory_popover {
            self.refresh_memory_rows();
        }
        cx.notify();
    }

    /// Forget: the entry is archived, out of recall, its line kept in
    /// the file's history.
    pub(super) fn forget_memory(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(store) = self.memory_store.clone() else {
            return;
        };
        match store.apply(&[gertie_core::Op::Archive { id: id.clone() }], 16) {
            Ok(_) => {
                self.refresh_memory_rows();
                self.refresh_memory_index(cx);
                cx.notify();
            }
            Err(error) => self.push_notice(format!("memory: {id} not forgotten ({error})"), cx),
        }
    }

    /// Save an edit: the entry's words replaced, its id kept.
    pub(super) fn save_memory_edit(&mut self, cx: &mut Context<Self>) {
        let Some((id, input)) = self.memory_edit.take() else {
            return;
        };
        let text = input.read(cx).value().trim().to_string();
        let Some(store) = self.memory_store.clone() else {
            return;
        };
        if text.is_empty() {
            cx.notify();
            return;
        }
        match store.apply(
            &[gertie_core::Op::Update {
                id: id.clone(),
                text,
            }],
            16,
        ) {
            Ok(_) => {
                self.refresh_memory_rows();
                self.refresh_memory_index(cx);
            }
            Err(error) => self.push_notice(format!("memory: {id} not edited ({error})"), cx),
        }
        cx.notify();
    }

    /// The popover over the pill's right end: this chat's memory, and
    /// the way to all of it.
    pub(super) fn render_memory_popover(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.memory_popover {
            return None;
        }
        let pal = ink::palette(cx);
        let (learned, used) = self.memory_here();
        let row_of = |id: &String| {
            self.memory_rows
                .iter()
                .find(|entry| &entry.id == id && entry.valid_until.is_none())
                .map(|entry| PopRow {
                    id: entry.id.clone(),
                    file: entry.file.clone(),
                    text: entry.text.clone(),
                    helpful: entry.helpful,
                })
        };
        let learned: Vec<PopRow> = learned.iter().filter_map(row_of).collect();
        let used: Vec<PopRow> = used.iter().filter_map(row_of).collect();
        let section = |title: &'static str, rows: Vec<PopRow>| {
            (!rows.is_empty()).then(|| {
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .px_3()
                            .pt_2()
                            .font_family(ink::SERIF)
                            .text_size(px(13.))
                            .text_color(pal.text_muted)
                            .child(title),
                    )
                    .children(rows.into_iter().map(|row| self.render_pop_row(row, cx)))
            })
        };
        let empty = learned.is_empty() && used.is_empty();
        let total = self
            .memory_rows
            .iter()
            .filter(|entry| entry.valid_until.is_none())
            .count();
        Some(
            v_flex()
                .id("memory-popover")
                .test_support()
                .occlude()
                .absolute()
                .bottom_full()
                .right_0()
                .mb_2()
                .w(px(MEMORY_POPOVER_WIDTH))
                .max_h(px(460.))
                .rounded(px(16.))
                .border_1()
                .border_color(pal.control)
                .bg(pal.paper_raised)
                .shadow_lg()
                .overflow_hidden()
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    if this.memory_popover && this.memory_edit.is_none() {
                        this.memory_popover = false;
                        this.memory_popover_closed = Some(std::time::Instant::now());
                        cx.notify();
                    }
                }))
                .child(
                    v_flex()
                        .id("memory-popover-rows")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .pb_2()
                        .children(section("learned in this chat", learned))
                        .children(section("used in this chat", used))
                        .when(empty, |list| {
                            list.child(
                                div()
                                    .px_3()
                                    .py_4()
                                    .font_family(ink::SERIF)
                                    .text_size(px(14.))
                                    .text_color(pal.text_muted)
                                    .child("Nothing used or learned in this chat yet."),
                            )
                        }),
                )
                .child(
                    h_flex()
                        .px_3()
                        .py_2()
                        .border_t_1()
                        .border_color(pal.line)
                        .font_family(ink::SANS)
                        .text_size(px(12.))
                        .text_color(pal.text_muted)
                        .child(div().flex_1().child(format!(
                            "Gertie remembers {total} thing{} here",
                            if total == 1 { "" } else { "s" }
                        )))
                        .child(
                            div()
                                .id("memory-all")
                                .test_support()
                                .cursor_pointer()
                                .text_color(pal.link)
                                .hover(|style| style.text_color(pal.text))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.memory_popover = false;
                                    this.open_memory_panel(cx);
                                }))
                                .child("see all"),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_pop_row(&self, row: PopRow, cx: &Context<Self>) -> AnyElement {
        let pal = ink::palette(cx);
        let id = row.id.clone();
        let editing = self
            .memory_edit
            .as_ref()
            .filter(|(editing, _)| *editing == row.id)
            .map(|(_, input)| input.clone());
        let action = |name: &str, label: &'static str, tip: &'static str| {
            div()
                .id(SharedString::from(format!("memory-{name}-{}", row.id)))
                .test_support()
                .cursor_pointer()
                .text_color(pal.text_muted)
                .hover(|style| style.text_color(pal.text))
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                .child(label)
        };
        let body = match editing {
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
                            action("save", "save", "Keep these words")
                                .text_color(pal.text)
                                .on_click(cx.listener(|this, _, _, cx| this.save_memory_edit(cx))),
                        )
                        .child(action("cancel", "cancel", "Leave it as it was").on_click(
                            cx.listener(|this, _, _, cx| {
                                this.memory_edit = None;
                                cx.notify();
                            }),
                        )),
                )
                .into_any_element(),
            None => v_flex()
                .gap_1()
                .child(
                    div()
                        .font_family(ink::SERIF)
                        .text_size(px(14.5))
                        .line_height(relative(1.45))
                        .text_color(pal.text)
                        .child(row.text.clone()),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .font_family(ink::SANS)
                        .text_size(px(11.5))
                        .child(
                            div()
                                .flex_1()
                                .text_color(pal.text_past)
                                .child(format!("{} · {}", row.id, row.file)),
                        )
                        .child(
                            action("keep", "keep", "Still true: counts in its favor at recall")
                                .when(row.helpful > 0, |keep| {
                                    keep.child(format!(" {}", row.helpful))
                                })
                                .on_click(cx.listener({
                                    let id = id.clone();
                                    move |this, _, _, cx| {
                                        this.vote_memory(id.clone(), true, cx);
                                        cx.notify();
                                    }
                                })),
                        )
                        .child(
                            action("edit", "edit", "Change its words").on_click(cx.listener({
                                let id = id.clone();
                                let text = row.text.clone();
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
                        .child(action("forget", "forget", "Stop remembering it").on_click(
                            cx.listener({
                                let id = id.clone();
                                move |this, _, _, cx| this.forget_memory(id.clone(), cx)
                            }),
                        )),
                )
                .into_any_element(),
        };
        div()
            .id(SharedString::from(format!("memory-pop-{}", row.id)))
            .test_support()
            .mx_1p5()
            .px_2()
            .py_2()
            .rounded(px(10.))
            .hover(|style| style.bg(pal.paper))
            .child(body)
            .into_any_element()
    }
}
