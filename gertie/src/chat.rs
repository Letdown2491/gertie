//! The M0 transcript: a bottom-pinned, variable-height message list with a
//! streaming path proven end to end by a timed echo stand-in for a provider.

use std::time::Duration;

use gertie_core::Role;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::{
    App, AnyElement, Context, Entity, IntoElement, ListAlignment, ListState, ParentElement,
    Render, Styled, Window, div, list, prelude::*, px,
};

const GREETING: &str = "gertie M0 — window and stream work. The agent loop and providers \
    arrive in M1; until then this echo proves the transcript renders token-by-token.";

/// One rendered turn in the transcript.
struct ChatMessage {
    role: Role,
    text: String,
}

pub struct ChatView {
    messages: Vec<ChatMessage>,
    input: Entity<InputState>,
    list_state: ListState,
    /// True while the echo stand-in is mid-stream. Real sessions will track
    /// request state in the core, not here.
    streaming: bool,
}

impl ChatView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Message gertie…"));
        cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { .. } => this.send(window, cx),
            _ => {}
        })
        .detach();

        let mut view = Self {
            messages: Vec::new(),
            input,
            list_state: ListState::new(0, ListAlignment::Bottom, px(256.)),
            streaming: false,
        };
        view.messages.push(ChatMessage {
            role: Role::Assistant,
            text: GREETING.into(),
        });
        view.sync_list(cx);
        view
    }

    /// Keeps the list's element count honest and stays pinned to the end —
    /// `ListAlignment::Bottom` follows the tail while the reader is there.
    fn sync_list(&mut self, cx: &mut Context<Self>) {
        self.list_state.reset(self.messages.len());
        self.list_state.scroll_to_end();
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() || self.streaming {
            return;
        }
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.messages.push(ChatMessage {
            role: Role::User,
            text,
        });
        self.stream_reply(cx);
    }

    /// M0's stand-in for a provider: whole tokens landed on a timer, the
    /// same shape `Event::Token` will arrive in over the real stream.
    fn stream_reply(&mut self, cx: &mut Context<Self>) {
        let echoed = self
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| message.text.clone())
            .unwrap_or_default();
        let reply_text = format!("You said: {echoed}");

        self.messages.push(ChatMessage {
            role: Role::Assistant,
            text: String::new(),
        });
        let reply_index = self.messages.len() - 1;
        self.streaming = true;
        self.sync_list(cx);

        let tokens: Vec<String> = reply_text
            .split_inclusive(' ')
            .map(str::to_string)
            .collect();
        cx.spawn(async move |this, cx| {
            for token in tokens {
                cx.background_executor()
                    .timer(Duration::from_millis(35))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if let Some(message) = this.messages.get_mut(reply_index) {
                        message.text.push_str(&token);
                    }
                    this.list_state.scroll_to_end();
                    cx.notify();
                });
            }
            let _ = this.update(cx, |this, cx| {
                this.streaming = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn render_message(&self, index: usize, cx: &App) -> AnyElement {
        let message = &self.messages[index];
        match message.role {
            Role::User => h_flex()
                .w_full()
                .justify_end()
                .child(
                    div()
                        .max_w_3_4()
                        .rounded_lg()
                        .px_3()
                        .py_2()
                        .bg(cx.theme().primary)
                        .text_color(cx.theme().primary_foreground)
                        .child(message.text.clone()),
                )
                .into_any_element(),
            _ => div()
                .w_full()
                .pr_16()
                .text_color(cx.theme().foreground)
                .child(message.text.clone())
                .into_any_element(),
        }
    }
}

impl Render for ChatView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let status = if self.streaming { "streaming…" } else { "ready" };

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().text_size(px(15.)).child("gertie"))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(status),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .px_4()
                    .child(list(self.list_state.clone(), move |index, _window, cx| {
                        view.update(cx, |view, cx| view.render_message(index, cx))
                            .unwrap_or_else(|_| div().into_any_element())
                    })),
            )
            .child(
                h_flex()
                    .px_3()
                    .py_3()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.input)),
                    ),
            )
    }
}
