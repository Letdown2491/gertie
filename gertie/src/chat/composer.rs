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
    pub(super) fn render_composer_area(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // The composer: one rounded box,
        // attach on the left, send on the
        // right, the reading width wide.
        // The pill (#54): paper-raised, round send on the right.
        let pal = crate::ink::palette(cx);
        div()
            .w_full()
            .pl(px(48.))
            .pr(px(56.))
            .pt_3()
            .pb_2()
            .flex()
            .justify_center()
            .child(
                h_flex()
                    .key_context("Composer")
                    .w_full()
                    .max_w(px(READING_WIDTH))
                    .items_end()
                    .gap_1()
                    .py_1p5()
                    .pl_2()
                    .pr_1p5()
                    .rounded(px(24.))
                    .border_1()
                    .border_color(pal.control)
                    .bg(pal.paper_raised)
                    .font_family(crate::ink::SERIF)
                    .text_size(px(16.))
                    // The steer/queue hint lives in the
                    // composer's placeholder now, the
                    // sync keeps it true while a turn
                    // runs, and an empty input is the
                    // only state that ever shows it.
                    .child(
                        Button::new("attach-file")
                            .icon(IconName::Paperclip)
                            .small()
                            .ghost()
                            // The name stays for the
                            // reader that never sees
                            // pixels.
                            .accessibility_label("attach file")
                            .tooltip("Attach images, PDFs, or text files")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.attach_from_picker(cx);
                            })),
                    )
                    .child(div().flex_1().min_w_0().child(
                        // Auto-grown by the state: one
                        // row at rest, up to six with
                        // content. Paste is hooked:
                        // images and copied files
                        // become chips, text pastes
                        // as text. The box around it
                        // is the border.
                        Textarea::new(&self.input).appearance(false).on_paste({
                            let view = cx.entity();
                            move |item, _, cx| {
                                view.update(cx, |this, cx| this.attach_paste(item, cx))
                            }
                        }),
                    ))
                    .child({
                        // Send, the same as Enter
                        // (which steers a running
                        // turn): a visible way to
                        // send for the mouse.
                        let empty = self.input.read(cx).value().trim().is_empty();
                        // While a turn runs with
                        // nothing typed, the button
                        // stops it; typing turns it
                        // back into send (a steer).
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
                    }),
            )
            .into_any_element()
    }
}
