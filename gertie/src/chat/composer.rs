//! The composer area as its own module: the input box with its
//! popups (slash commands, @-mentions), queued follow-ups, the
//! rewind bar, attachments, and the send/stop controls. One impl
//! ChatView block, the render body moved whole (issue #16 step 4).

use super::*;

impl ChatView {
    /// The composer column: input, popups, queues, the chrome
    /// between transcript and footer.
    pub(super) fn render_composer_area(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // The composer: one rounded box,
        // attach on the left, send on the
        // right, the reading width wide.
        div()
            .w_full()
            .px_4()
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
                    .p_1p5()
                    .rounded(px(12.))
                    .border_1()
                    .border_color(cx.theme().input)
                    .bg(cx.theme().secondary.opacity(0.5))
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
                                    "Steer the running turn (Enter)"
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
