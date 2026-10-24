//! The M1 chat: session sidebar + transcript + live provider streaming.

use futures_util::StreamExt as _;
use gertie_core::{Event, MessageId, Role, SessionId, SessionStore};
use gertie_providers::{OpenAICompatible, Provider, Request};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, AnyElement, AppContext as _, Context, Entity, Focusable, IntoElement, ListAlignment,
    ListState, ParentElement, Render, SharedString, StatefulInteractiveElement as _, Styled,
    Window, div, list, prelude::*, px,
};
use std::sync::Arc;
use std::time::Duration;

/// One rendered turn in the transcript, with its row in the session store
/// (so streaming updates land in SQLite without inserting a row per token).
struct ChatMessage {
    role: Role,
    text: String,
    db_id: Option<MessageId>,
}

pub struct ChatView {
    store: Arc<SessionStore>,
    provider: Option<Arc<OpenAICompatible>>,
    endpoint_label: String,
    sessions: Vec<gertie_core::SessionSummary>,
    /// None until the first message — empty chats don't exist as rows, so
    /// the sidebar shows only real conversations.
    session_id: Option<SessionId>,
    messages: Vec<ChatMessage>,
    input: Entity<InputState>,
    list_state: ListState,
    streaming: bool,
}

impl ChatView {
    pub fn new(
        store: Arc<SessionStore>,
        provider: Option<Arc<OpenAICompatible>>,
        endpoint_label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Message gertie…"));
        cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { .. } => this.send(window, cx),
            _ => {}
        })
        .detach();

        let sessions = store.list_sessions().unwrap_or_default();
        // Resume the newest session if there is one; otherwise the first
        // message creates the session (lazy, so no empty rows accumulate).
        let session_id = sessions.first().map(|summary| summary.id);

        let mut view = Self {
            store,
            provider,
            endpoint_label,
            sessions,
            session_id,
            messages: Vec::new(),
            input,
            list_state: ListState::new(0, ListAlignment::Bottom, px(256.)),
            streaming: false,
        };
        view.reload_session(cx);
        window.focus(&view.input.read(cx).focus_handle(cx), cx);
        view
    }

    /// The session the transcript is bound to, for tests and tooling.
    pub fn session_id(&self) -> Option<SessionId> {
        self.session_id
    }

    /// Swaps the transcript for a session's stored messages.
    fn reload_session(&mut self, cx: &mut Context<Self>) {
        self.messages = match self.session_id {
            Some(session_id) => self
                .store
                .load_messages(session_id)
                .unwrap_or_default()
                .into_iter()
                .map(|message| ChatMessage {
                    role: message.role,
                    text: message.content,
                    db_id: Some(message.id),
                })
                .collect(),
            None => Vec::new(),
        };
        self.streaming = false;
        self.sync_list(cx);
    }

    fn sync_list(&mut self, cx: &mut Context<Self>) {
        self.list_state.reset(self.messages.len());
        self.list_state.scroll_to_end();
        cx.notify();
    }

    fn new_session(&mut self, _event: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.streaming {
            return;
        }
        // Sessions are created lazily by the first message; "New chat" just
        // blanks the transcript until then.
        self.session_id = None;
        self.messages.clear();
        self.sync_list(cx);
        window.focus(&self.input.read(cx).focus_handle(cx), cx);
    }

    fn switch_to_session(
        &mut self,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.streaming {
            return;
        }
        self.session_id = Some(session_id);
        self.reload_session(cx);
        window.focus(&self.input.read(cx).focus_handle(cx), cx);
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() || self.streaming {
            return;
        }
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        window.focus(&self.input.read(cx).focus_handle(cx), cx);

        // First message of a chat: the session is born here, titled by it.
        let session_id = match self.session_id {
            Some(session_id) => session_id,
            None => {
                let title: String = text.chars().take(48).collect();
                let session_id = self
                    .store
                    .create_session(&title)
                    .expect("create session");
                self.session_id = Some(session_id);
                self.sessions = self.store.list_sessions().unwrap_or_default();
                session_id
            }
        };

        let parent = self.messages.last().and_then(|message| message.db_id);
        let db_id = self
            .store
            .append_message(session_id, parent, Role::User, &text)
            .ok();
        self.messages.push(ChatMessage {
            role: Role::User,
            text,
            db_id,
        });
        self.stream_reply(cx);
    }

    /// Turns the transcript into a provider request and drives the event
    /// stream into the last (assistant) message, persisting as it goes.
    fn stream_reply(&mut self, cx: &mut Context<Self>) {
        let Some(provider) = self.provider.clone() else {
            self.messages.push(ChatMessage {
                role: Role::Assistant,
                text: NO_ENDPOINT_NOTICE.into(),
                db_id: None,
            });
            self.sync_list(cx);
            return;
        };

        let store = self.store.clone();
        let parent = self.messages.last().and_then(|message| message.db_id);
        let assistant_db = self
            .session_id
            .and_then(|session_id| store.append_message(session_id, parent, Role::Assistant, "").ok());

        let request = Request {
            messages: self
                .messages
                .iter()
                .map(|message| (message.role, message.text.clone()))
                .collect(),
            max_output_tokens: None,
        };
        let stream = provider.stream(request);

        self.messages.push(ChatMessage {
            role: Role::Assistant,
            text: String::new(),
            db_id: assistant_db,
        });
        let reply_index = self.messages.len() - 1;
        self.streaming = true;
        self.sync_list(cx);

        cx.spawn(async move |this, cx| {
            let mut stream = stream;
            while let Some(event) = stream.next().await {
                match event {
                    Event::Token { delta } => {
                        let _ = this.update(cx, |this, cx| {
                            if let Some(message) = this.messages.get_mut(reply_index) {
                                message.text.push_str(&delta);
                                if let Some(db_id) = message.db_id {
                                    let _ = store.update_message(db_id, &message.text);
                                }
                            }
                            this.list_state.scroll_to_end();
                            cx.notify();
                        });
                    }
                    Event::Error { message } => {
                        let _ = this.update(cx, |this, cx| {
                            if let Some(message_at) = this.messages.get_mut(reply_index) {
                                if !message_at.text.is_empty() {
                                    message_at.text.push_str("\n\n");
                                }
                                message_at
                                    .text
                                    .push_str(&format!("[error] {message}"));
                                if let Some(db_id) = message_at.db_id {
                                    let _ = store.update_message(db_id, &message_at.text);
                                }
                            }
                            cx.notify();
                        });
                    }
                    Event::Done => break,
                    _ => {}
                }
                cx.background_executor()
                    .timer(Duration::from_millis(1))
                    .await;
            }
            let _ = this.update(cx, |this, cx| {
                this.streaming = false;
                this.sessions = this.store.list_sessions().unwrap_or_default();
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
            _ => {
                let is_error = message.text.contains("[error]");
                div()
                    .w_full()
                    .pr_16()
                    .text_color(if is_error {
                        cx.theme().danger
                    } else {
                        cx.theme().foreground
                    })
                    .child(message.text.clone())
                    .into_any_element()
            }
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
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_size(px(15.)).child("gertie"))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(self.endpoint_label.clone()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(status),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(
                        // Session sidebar.
                        v_flex()
                            .id("session-sidebar")
                            .w(px(220.))
                            .h_full()
                            .flex_shrink_0()
                            .border_r_1()
                            .border_color(cx.theme().border)
                            .p_2()
                            .gap_1()
                            .overflow_y_scroll()
                            .child(
                                Button::new("new-chat")
                                    .label("New chat")
                                    .small()
                                    .on_click(cx.listener(Self::new_session)),
                            )
                            .children(self.sessions.iter().map(|summary| {
                                let session_id = summary.id;
                                Button::new(SharedString::from(format!(
                                    "session-{}",
                                    summary.id.0
                                )))
                                .label(summary.title.clone())
                                .small()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.switch_to_session(session_id, window, cx);
                                }))
                            })),
                    )
                    .child(
                        // The transcript: flex sizing goes directly on the
                        // list element — a wrapper div collapses it.
                        list(self.list_state.clone(), move |index, _window, cx| {
                            view.update(cx, |view, cx| view.render_message(index, cx))
                                .unwrap_or_else(|_| div().into_any_element())
                        })
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .px_4(),
                    ),
            )
            .child(
                h_flex()
                    .px_3()
                    .py_3()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().child(Input::new(&self.input))),
            )
    }
}

const NO_ENDPOINT_NOTICE: &str = "No endpoint configured. Create ~/.config/gertie/config.toml:\n\n\
    [[endpoints]]\n\
    name = \"ppq\"\n\
    base_url = \"https://api.ppq.ai/v1\"\n\
    model = \"glm-5.3-flash\"\n\
    api_key_env = \"PPQ_API_KEY\"\n\n\
    …set PPQ_API_KEY in your environment, then restart gertie.";
