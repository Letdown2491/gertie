//! The composer area as its own module: the input box with its
//! popups (slash commands, @-mentions), queued follow-ups, the
//! rewind bar, attachments, and the send/stop controls. One impl
//! ChatView block, the render body moved whole (issue #16 step 4).

use super::*;

impl ChatView {
    /// What sits between the transcript and the queue rows: steers
    /// still on their way into the running turn, and, at rest, the
    /// held-queue line that says queued messages are waiting and what
    /// can be done about them. `None` when there is nothing to say.
    pub(super) fn render_queue_status(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let muted = cx.theme().muted_foreground;
        if self.streaming && !self.steers_in_flight.is_empty() {
            return Some(
                v_flex()
                    .mx_4()
                    .mt_2()
                    .gap_1()
                    .children(
                        self.steers_in_flight
                            .iter()
                            .enumerate()
                            .map(|(index, text)| {
                                let first = text.lines().next().unwrap_or_default().to_string();
                                h_flex()
                                    .id(("steer-in-flight", index))
                                    .gap_2()
                                    .text_size(px(12.))
                                    .text_color(muted)
                                    .child(div().flex_none().child("steering…"))
                                    .child(div().flex_1().truncate().child(first))
                            }),
                    )
                    .into_any_element(),
            );
        }
        if self.streaming || self.queued.is_empty() {
            return None;
        }
        let count = self.queued.len();
        let words = if count == 1 {
            "1 queued message is waiting. It goes with your next message.".to_string()
        } else {
            format!("{count} queued messages are waiting. They go with your next message.")
        };
        Some(
            h_flex()
                .id("queue-held")
                .mx_4()
                .mt_2()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .text_size(px(12.))
                        .text_color(muted)
                        .child(words),
                )
                .child(
                    Button::new("queue-send-all")
                        .label("send now")
                        .xsmall()
                        .ghost()
                        .tooltip("Send the queue now, as one message")
                        .on_click(cx.listener(|this, _, _, cx| this.send_queue(Vec::new(), cx))),
                )
                .child(
                    Button::new("queue-clear")
                        .label("clear")
                        .xsmall()
                        .ghost()
                        .tooltip("Drop every queued message")
                        .on_click(cx.listener(|this, _, _, cx| this.clear_queue(cx))),
                )
                .into_any_element(),
        )
    }

    /// The composer column: input, popups, queues, the chrome
    /// between transcript and footer.
    ///
    /// One slim row while the text fits on a line: attach and the
    /// permission level, the text, memory and the model, send. Once
    /// the text wraps, it takes a full-width row of its own and the
    /// controls drop under it (#54 follow-up), until it fits the slim
    /// row again.
    pub(super) fn render_composer_area(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let pal = crate::ink::palette(cx);
        let stacked = self.composer_stacked(cx);
        let attach = Button::new("attach-file")
            .icon(IconName::Paperclip)
            .small()
            .ghost()
            // The name stays for the reader that never sees pixels.
            .accessibility_label("attach file")
            .tooltip("Attach images, PDFs, or text files")
            .on_click(cx.listener(|this, _, _, cx| {
                this.attach_from_picker(cx);
            }));
        let posture = self.render_pill_posture(cx);
        // Auto-grown by the state: one row at rest, up to eight with
        // content, then it scrolls. Paste is hooked: images and copied
        // files become chips, text pastes as text.
        let text = Textarea::new(&self.input).appearance(false).on_paste({
            let view = cx.entity();
            move |item, _, cx| view.update(cx, |this, cx| this.attach_paste(item, cx))
        });
        let trailing = self.render_pill_trailing(cx);
        // Send, the same as Enter (which steers a running turn). While
        // a turn runs with nothing typed, the button stops it; typing
        // turns it back into send (a steer).
        let send = {
            let empty = self.input.read(cx).value().trim().is_empty();
            if self.streaming && empty {
                Button::new("stop-turn")
                    .icon(IconName::Square)
                    .small()
                    .danger()
                    .tooltip("Stop the running turn")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.stop_turn(cx);
                    }))
            } else {
                Button::new("send-message")
                    .icon(IconName::ArrowUp)
                    .small()
                    .primary()
                    .disabled(empty)
                    .tooltip(if self.streaming {
                        "Queue for after this turn (Enter) · Ctrl+Enter steers it now"
                    } else {
                        "Send (Enter)"
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.send(false, window, cx);
                    }))
            }
        };
        let pill = div()
            .key_context("Composer")
            .relative()
            .w_full()
            .max_w(px(READING_WIDTH))
            .border_1()
            .border_color(pal.control)
            .bg(pal.paper_raised)
            .font_family(crate::ink::SERIF)
            .text_size(px(16.))
            .children(self.render_memory_popover(cx))
            .children(self.render_model_card(cx));
        let pill = if stacked {
            pill.flex()
                .flex_col()
                .gap_1()
                .pt_2p5()
                .pb_1p5()
                .pl_2()
                .pr_1p5()
                .rounded(px(20.))
                .child(div().id("composer-text").w_full().px_2().child(text))
                .child(
                    h_flex()
                        .id("composer-controls")
                        .test_support()
                        .w_full()
                        .items_center()
                        .gap_1()
                        .child(attach)
                        .child(posture)
                        .child(div().flex_1())
                        .child(trailing)
                        .child(send),
                )
        } else {
            pill.flex()
                .items_center()
                .gap_1()
                .py_1p5()
                .pl_2()
                .pr_1p5()
                .rounded(px(24.))
                .child(attach)
                .child(posture)
                .child(
                    div()
                        .id("composer-text")
                        .flex_1()
                        .min_w_0()
                        .pl_1()
                        .child(text),
                )
                .child(trailing)
                .child(send)
        };
        div()
            .w_full()
            .pl(px(48.))
            .pr(px(56.))
            .pt_3()
            .pb_2()
            .flex()
            .justify_center()
            .child(pill)
            .into_any_element()
    }

    /// Whether the composer shows its stacked form. It stacks once the
    /// text wraps to a second row (or holds a newline) and goes back to
    /// the slim row only when the text would fit there again, measured
    /// against the slim row's width, so it never flickers between the
    /// two at the edge.
    fn composer_stacked(&mut self, cx: &App) -> bool {
        let input = self.input.clone();
        let state = input.read(cx);
        let value = state.value();
        if value.is_empty() {
            self.composer_stacked = false;
            return false;
        }
        let rows = state.display_map().wrap_row_count();
        if !self.composer_stacked {
            if rows > 1 {
                self.composer_stacked = true;
            } else if let Some(bounds) = state.text_bounds() {
                self.composer_inline_width = bounds.size.width;
            }
        } else if rows <= 1 && !value.contains('\n') {
            let len = value.len();
            if let (Some(end), Some(bounds)) =
                (state.range_to_bounds(&(len..len)), state.text_bounds())
            {
                let used = end.origin.x - bounds.origin.x;
                if used < self.composer_inline_width - px(12.) {
                    self.composer_stacked = false;
                }
            }
        }
        self.composer_stacked
    }

    /// Whether the composer is stacked, for tests.
    pub fn composer_stacked_for_test(&self) -> bool {
        self.composer_stacked
    }
}
