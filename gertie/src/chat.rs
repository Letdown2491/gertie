//! The M2 chat: session sidebar + transcript + the agent loop, with tool
//! cards that gate mutating calls behind approve/deny.

use futures_util::StreamExt as _;
use gertie_core::{
    LoopEvent, MessageId, PermissionBroker, PermissionDecision, Role, SessionId, SessionStore,
    SharedHistory, SnapshotStore, ToolCall, ToolRegistry, TurnSpec, run_turn,
};
use gertie_providers::OpenAICompatible;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable, IntoElement, ListAlignment,
    ListState, ParentElement, Render, SharedString, StatefulInteractiveElement as _, Styled,
    Window, div, list, prelude::*, px,
};
use std::sync::Arc;
use std::time::Duration;

/// Where the transcript is in a tool call's life: waiting on the human,
/// executing, or finished (with the result the model will also see).
enum ToolStatus {
    /// Waiting on the human; `broker.respond(id, …)` settles it.
    Pending(u64),
    Running,
    Done(String),
}

/// One rendered turn in the transcript. Plain messages keep their row in
/// the session store (streaming updates land in SQLite without a row per
/// token); tool turns are rendered as cards keyed by call id.
enum Turn {
    Message {
        role: Role,
        text: String,
        db_id: Option<MessageId>,
    },
    Tool {
        call: ToolCall,
        status: ToolStatus,
        /// Read-only tools never prompted; the card says so instead of
        /// showing buttons the human never needed to click.
        auto_approved: bool,
        /// Subagent asks get labeled — same card, provenance visible.
        worker: bool,
    },
}

pub struct ChatView {
    store: Arc<SessionStore>,
    provider: Option<Arc<OpenAICompatible>>,
    snapshots: Option<Arc<SnapshotStore>>,
    tools: ToolRegistry,
    endpoint_label: String,
    /// Per-turn request framing (identity, policy, skill index). Not
    /// part of history: the DB stays canonical.
    system_prompt: Option<String>,
    sessions: Vec<gertie_core::SessionSummary>,
    /// None until the first message — empty chats don't exist as rows, so
    /// the sidebar shows only real conversations.
    session_id: Option<SessionId>,
    turns: Vec<Turn>,
    /// Canonical conversation the loop reads and extends across turns.
    history: SharedHistory,
    /// Index of the assistant turn currently receiving deltas; None until
    /// the first delta of a round (tool rounds each get their own row).
    streaming_assistant: Option<usize>,
    /// Last persisted row — the parent for whatever is appended next.
    last_db_id: Option<MessageId>,
    /// What the agent is up to right now, for the status line — the loop
    /// goes quiet between streaming rounds, and silence reads as a hang.
    activity: String,
    /// Session token totals from the provider's usage events (in, out).
    /// In-memory for now; a persisted per-session ledger is M3 scope.
    usage: (u64, u64),
    /// Shared approval registry: leader and worker asks, one answer path.
    broker: Arc<PermissionBroker>,
    input: Entity<InputState>,
    list_state: ListState,
    streaming: bool,
}

impl ChatView {
    /// The constructor is UI-wiring, not an abstraction boundary; every
    /// argument is a genuine dependency of the view.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<SessionStore>,
        provider: Option<Arc<OpenAICompatible>>,
        snapshots: Option<Arc<SnapshotStore>>,
        tools: ToolRegistry,
        endpoint_label: String,
        system_prompt: Option<String>,
        broker: Arc<PermissionBroker>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Message gertie…"));
        cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.send(window, cx)
            }
        })
        .detach();

        let sessions = store.list_sessions().unwrap_or_default();
        // Resume the newest session if there is one; otherwise the first
        // message creates the session (lazy, so no empty rows accumulate).
        let session_id = sessions.first().map(|summary| summary.id);

        let mut view = Self {
            store,
            provider,
            snapshots,
            tools,
            endpoint_label,
            system_prompt,
            sessions,
            session_id,
            turns: Vec::new(),
            history: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            streaming_assistant: None,
            last_db_id: None,
            activity: "starting…".into(),
            usage: (0, 0),
            broker,
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

    /// Swaps the transcript for a session's stored messages and rebuilds
    /// the canonical history the loop will extend.
    fn reload_session(&mut self, cx: &mut Context<Self>) {
        let (turns, history, last_db_id) = match self.session_id {
            Some(session_id) => {
                let rows = self.store.load_messages(session_id).unwrap_or_default();
                let turns = rows
                    .iter()
                    .map(|row| match row.role {
                        Role::Tool => Turn::Tool {
                            call: ToolCall {
                                id: row.tool_call_id.clone().unwrap_or_default(),
                                name: row.tool_name.clone().unwrap_or_default(),
                                arguments: row.tool_arguments.clone().unwrap_or_default(),
                            },
                            status: ToolStatus::Done(row.content.clone()),
                            auto_approved: false,
                            worker: false,
                        },
                        role => Turn::Message {
                            role,
                            text: row.content.clone(),
                            db_id: Some(row.id),
                        },
                    })
                    .collect();
                let history = self.store.load_wire_history(session_id).unwrap_or_default();
                let last_db_id = rows.last().map(|row| row.id);
                (turns, history, last_db_id)
            }
            None => (Vec::new(), Vec::new(), None),
        };
        self.turns = turns;
        self.history = Arc::new(tokio::sync::Mutex::new(history));
        self.last_db_id = last_db_id;
        self.streaming = false;
        self.streaming_assistant = None;
        self.sync_list(cx);
    }

    fn sync_list(&mut self, cx: &mut Context<Self>) {
        self.list_state.reset(self.turns.len());
        self.list_state.scroll_to_end();
        cx.notify();
    }

    fn new_session(
        &mut self,
        _event: &gpui_kit::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.streaming {
            return;
        }
        // Sessions are created lazily by the first message; "New chat" just
        // blanks the transcript until then.
        self.session_id = None;
        self.turns.clear();
        self.last_db_id = None;
        self.streaming_assistant = None;
        self.history.blocking_lock().clear();
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
                let session_id = self.store.create_session(&title).expect("create session");
                self.session_id = Some(session_id);
                self.sessions = self.store.list_sessions().unwrap_or_default();
                session_id
            }
        };

        let db_id = self
            .store
            .append_message(session_id, self.last_db_id, Role::User, &text)
            .ok();
        if db_id.is_some() {
            self.last_db_id = db_id;
        }
        self.turns.push(Turn::Message {
            role: Role::User,
            text: text.clone(),
            db_id,
        });
        self.drive_turn(text, cx);
    }

    /// Hands the turn to the agent loop and renders its event stream:
    /// deltas stream into an assistant row, tool calls become cards, and
    /// results persist as tool rows the model also sees on reload.
    fn drive_turn(&mut self, user_text: String, cx: &mut Context<Self>) {
        let Some(provider) = self.provider.clone() else {
            self.turns.push(Turn::Message {
                role: Role::Assistant,
                text: NO_ENDPOINT_NOTICE.into(),
                db_id: None,
            });
            self.sync_list(cx);
            return;
        };

        let context_window = provider.context_window();
        let provider: Arc<dyn gertie_core::Provider> = provider;
        let stream = run_turn(TurnSpec {
            provider,
            tools: self.tools.clone(),
            history: self.history.clone(),
            user_message: user_text,
            snapshots: self.snapshots.clone(),
            context_window,
            permission_broker: Some(self.broker.clone()),
            system_prompt: self.system_prompt.clone(),
        });
        // Subagent asks arrive here, not on the leader's stream — a
        // worker's loop runs inside a tool, so nobody drains its events.
        let mut worker_asks = self.broker.take_subscription();

        self.streaming = true;
        self.streaming_assistant = None;
        self.sync_list(cx);

        cx.spawn(async move |this, cx| {
            let mut stream = stream;
            // The leader stream parks inside agent tools while a worker
            // runs — its asks would be invisible. Selecting over both
            // keeps every ask answerable, whoever asked.
            loop {
                tokio::select! {
                    event = stream.next() => {
                        let Some(event) = event else { break };
                        match event {
                    LoopEvent::Delta { delta } => {
                        let _ = this.update(cx, |this, cx| this.on_delta(&delta, cx));
                    }
                    LoopEvent::ToolCallRequested { id, call } => {
                        let _ = this.update(cx, |this, cx| {
                            this.on_tool_requested(id, call, cx)
                        });
                    }
                    LoopEvent::ToolCallStarted { call } => {
                        let _ = this.update(cx, |this, cx| this.on_tool_started(call, cx));
                    }
                    LoopEvent::ToolResult {
                        call_id,
                        name,
                        result,
                    } => {
                        let _ = this.update(cx, |this, cx| {
                            this.on_tool_result(&call_id, &name, &result, cx)
                        });
                    }
                    LoopEvent::Usage {
                        input_tokens,
                        output_tokens,
                    } => {
                        let _ = this.update(cx, |this, cx| {
                            this.usage.0 += input_tokens;
                            this.usage.1 += output_tokens;
                            cx.notify();
                        });
                    }
                    LoopEvent::Compacted { kept, total } => {
                        let _ = this.update(cx, |this, cx| this.on_compacted(kept, total, cx));
                    }
                    LoopEvent::Error { message } => {
                        let _ = this.update(cx, |this, cx| this.on_error(&message, cx));
                    }
                    LoopEvent::Done => break,
                        }
                    }
                    ask = async {
                        match &mut worker_asks {
                            Some(receiver) => receiver.recv().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if let Some(ask) = ask {
                            let _ = this.update(cx, |this, cx| {
                                this.on_worker_ask(ask.id, ask.call, cx)
                            });
                        }
                    }
                }
                cx.background_executor()
                    .timer(Duration::from_millis(1))
                    .await;
            }
            let _ = this.update(cx, |this, cx| {
                this.streaming = false;
                this.streaming_assistant = None;
                this.sessions = this.store.list_sessions().unwrap_or_default();
                cx.notify();
            });
        })
        .detach();
    }

    /// Streams a delta into the round's assistant turn, creating its row
    /// (and database row) on the first delta.
    fn on_delta(&mut self, delta: &str, cx: &mut Context<Self>) {
        self.activity = "streaming…".into();
        let index = match self.streaming_assistant {
            Some(index) => index,
            None => {
                let db_id = self.session_id.and_then(|session_id| {
                    self.store
                        .append_message(session_id, self.last_db_id, Role::Assistant, "")
                        .ok()
                });
                if db_id.is_some() {
                    self.last_db_id = db_id;
                }
                self.turns.push(Turn::Message {
                    role: Role::Assistant,
                    text: String::new(),
                    db_id,
                });
                let index = self.turns.len() - 1;
                self.streaming_assistant = Some(index);
                // The list's item count is stale the moment a turn is
                // added — without a reset, scroll_to_end targets the old
                // end and the new turn renders below the viewport (the
                // "reply appeared only after I typed again" bug).
                self.list_state.reset(self.turns.len());
                index
            }
        };

        let persisted = if let Some(Turn::Message { text, db_id, .. }) = self.turns.get_mut(index) {
            text.push_str(delta);
            db_id.map(|db_id| (db_id, text.clone()))
        } else {
            None
        };
        if let Some((db_id, text)) = persisted {
            let _ = self.store.update_message(db_id, &text);
        }
        self.list_state.scroll_to_end();
        cx.notify();
    }

    /// A read-only tool started executing: give it a running card so the
    /// quiet parts of a turn still look alive.
    fn on_tool_started(&mut self, call: ToolCall, cx: &mut Context<Self>) {
        self.activity = format!("running {}…", call.name);
        self.streaming_assistant = None;
        self.turns.push(Turn::Tool {
            call,
            status: ToolStatus::Running,
            auto_approved: true,
            worker: false,
        });
        self.sync_list(cx);
    }

    /// A mutating tool wants to run: park the request id on the card
    /// until the human answers through the broker.
    fn on_tool_requested(&mut self, id: u64, call: ToolCall, cx: &mut Context<Self>) {
        self.activity = "waiting for approval".into();
        self.streaming_assistant = None;
        self.turns.push(Turn::Tool {
            call,
            status: ToolStatus::Pending(id),
            auto_approved: false,
            worker: false,
        });
        self.sync_list(cx);
    }

    /// A subagent's ask, arriving via the broker subscription — same
    /// card, labeled.
    fn on_worker_ask(&mut self, id: u64, call: ToolCall, cx: &mut Context<Self>) {
        self.activity = "worker waiting for approval".into();
        self.streaming_assistant = None;
        self.turns.push(Turn::Tool {
            call,
            status: ToolStatus::Pending(id),
            auto_approved: false,
            worker: true,
        });
        self.sync_list(cx);
    }

    /// The tool finished; show the outcome and persist the row so reloads
    /// and the next request see the same thing the model did.
    fn on_tool_result(&mut self, call_id: &str, name: &str, result: &str, cx: &mut Context<Self>) {
        self.activity = "thinking…".into();
        self.streaming_assistant = None;

        let arguments = self
            .turns
            .iter()
            .find_map(|turn| match turn {
                Turn::Tool { call, .. } if call.id == call_id => Some(call.arguments.clone()),
                _ => None,
            })
            .unwrap_or_default();

        for turn in &mut self.turns {
            if let Turn::Tool { call, status, .. } = turn
                && call.id == call_id
            {
                *status = ToolStatus::Done(result.to_string());
            }
        }

        if let Some(session_id) = self.session_id
            && let Ok(db_id) = self.store.append_tool_result(
                session_id,
                self.last_db_id,
                call_id,
                name,
                &arguments,
                result,
            )
        {
            self.last_db_id = Some(db_id);
        }
        self.list_state.scroll_to_end();
        cx.notify();
    }

    /// Loop-level failures (provider exhausted retries, round budget hit)
    /// surface in the transcript like any other turn.
    /// History shrank mid-turn: tell the user honestly — a muted notice
    /// card, not hidden magic. Not persisted: DB rows stay whole, the
    /// compaction is the in-memory view (a reload restores full history
    /// and the loop re-compacts if the threshold trips again).
    fn on_compacted(&mut self, kept: usize, total: usize, cx: &mut Context<Self>) {
        self.activity = "compacted".into();
        self.turns.push(Turn::Message {
            role: Role::System,
            text: format!(
                "⟳ context compacted — {kept} of {total} messages kept, earlier history summarized"
            ),
            db_id: None,
        });
        self.sync_list(cx);
    }

    fn on_error(&mut self, message: &str, cx: &mut Context<Self>) {
        let rendered = format!("[error] {message}");
        self.activity = "error".into();
        match self.streaming_assistant {
            Some(index) => {
                let persisted =
                    if let Some(Turn::Message { text, db_id, .. }) = self.turns.get_mut(index) {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(&rendered);
                        db_id.map(|db_id| (db_id, text.clone()))
                    } else {
                        None
                    };
                if let Some((db_id, text)) = persisted {
                    let _ = self.store.update_message(db_id, &text);
                }
            }
            None => self.turns.push(Turn::Message {
                role: Role::Assistant,
                text: rendered,
                db_id: None,
            }),
        }
        self.list_state.reset(self.turns.len());
        self.list_state.scroll_to_end();
        cx.notify();
    }

    /// Answers a pending permission card through the broker. A dropped
    /// request (UI closed) counts as a deny in the loop; here the human
    /// did answer.
    fn decide_tool(&mut self, index: usize, decision: PermissionDecision, cx: &mut Context<Self>) {
        if let Some(Turn::Tool { status, .. }) = self.turns.get_mut(index) {
            match std::mem::replace(status, ToolStatus::Running) {
                ToolStatus::Pending(id) => {
                    if !self.broker.respond(id, decision) {
                        // Unknown id (already answered): put nothing back;
                        // the card was settled elsewhere.
                    }
                }
                // Not actually pending (double click): put it back.
                other => *status = other,
            }
        }
        cx.notify();
    }

    fn render_turn(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        match &self.turns[index] {
            Turn::Message { role, text, .. } => self.render_message(role, text, cx),
            Turn::Tool {
                call,
                status,
                auto_approved,
                worker,
            } => self.render_tool(index, call, status, *auto_approved, *worker, cx),
        }
    }

    fn render_message(&self, role: &Role, text: &str, cx: &App) -> AnyElement {
        match role {
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
                        .child(text.to_string()),
                )
                .into_any_element(),
            _ => {
                let is_error = text.contains("[error]");
                div()
                    .w_full()
                    .pr_16()
                    .text_color(if is_error {
                        cx.theme().danger
                    } else {
                        cx.theme().foreground
                    })
                    .child(text.to_string())
                    .into_any_element()
            }
        }
    }

    /// The tool card: what it wants to do, the diff for file changes, and
    /// approve/deny while pending.
    fn render_tool(
        &self,
        index: usize,
        call: &ToolCall,
        status: &ToolStatus,
        auto_approved: bool,
        worker: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut inner = v_flex().gap_2().child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(cx.theme().accent)
                        .child(if worker {
                            format!("⚙ {} · worker", call.name)
                        } else {
                            format!("⚙ {}", call.name)
                        }),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(argument_summary(call)),
                ),
        );

        match status {
            ToolStatus::Pending(_) => {
                inner = inner
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().warning)
                            .child("wants to modify your workspace"),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new(SharedString::from(format!("approve-{index}")))
                                    .label("Approve")
                                    .small()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.decide_tool(index, PermissionDecision::Approve, cx)
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("deny-{index}")))
                                    .label("Deny")
                                    .small()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.decide_tool(index, PermissionDecision::Deny, cx)
                                    })),
                            ),
                    );
            }
            ToolStatus::Running => {
                inner = inner.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Spinner::new().small())
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child(if auto_approved {
                                    "running (read-only, auto-approved)"
                                } else {
                                    "running…"
                                }),
                        ),
                );
            }
            ToolStatus::Done(result) => {
                if let Some(diff) = diff_lines(call, cx) {
                    inner = inner.child(diff);
                }
                let preview: String = result.lines().take(12).collect::<Vec<_>>().join("\n");
                inner = inner.child(
                    div()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child(preview),
                );
            }
        }

        v_flex()
            .w_full()
            .pr_16()
            .pb_2()
            .child(
                v_flex()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded_lg()
                    .p_3()
                    .child(inner),
            )
            .into_any_element()
    }
}

/// One-line summary of what the call targets, for the card header.
fn argument_summary(call: &ToolCall) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&call.arguments) else {
        return String::new();
    };
    let field = ["path", "pattern", "command", "url"]
        .iter()
        .find_map(|field| value.get(*field))
        .and_then(|field| field.as_str());
    match field {
        Some(field) => field.to_string(),
        None => String::new(),
    }
}

/// For file-mutating calls, renders the change as +/- lines: edit shows
/// old in red and new in green; write shows the content as additions.
/// Returns None for tools without a natural diff.
fn diff_lines(call: &ToolCall, cx: &App) -> Option<AnyElement> {
    let value: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
    let mut container = v_flex()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().border)
        .text_size(px(12.))
        .overflow_hidden();

    let (add, remove): (Vec<&str>, Vec<&str>) = match call.name.as_str() {
        "edit_file" => {
            let old = value.get("old_text")?.as_str()?;
            let new = value
                .get("new_text")
                .and_then(|text| text.as_str())
                .unwrap_or("");
            (
                new.lines().take(24).collect(),
                old.lines().take(24).collect(),
            )
        }
        "write_file" => {
            let content = value.get("content")?.as_str()?;
            (content.lines().take(24).collect(), Vec::new())
        }
        _ => return None,
    };

    for line in remove {
        container = container.child(
            div()
                .px_2()
                .text_color(cx.theme().danger)
                .child(format!("− {line}")),
        );
    }
    for line in add {
        container = container.child(
            div()
                .px_2()
                .text_color(cx.theme().success)
                .child(format!("+ {line}")),
        );
    }
    Some(container.into_any_element())
}

impl Render for ChatView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let pending = self.turns.iter().any(|turn| {
            matches!(
                turn,
                Turn::Tool {
                    status: ToolStatus::Pending(_),
                    ..
                }
            )
        });
        let status = if pending {
            "approval needed"
        } else {
            &self.activity
        };
        let busy = pending || self.streaming;

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
                        h_flex()
                            .gap_2()
                            .items_center()
                            .when(busy, |row| {
                                row.child(Spinner::new().small().color(cx.theme().accent))
                            })
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(if pending {
                                        cx.theme().warning_foreground
                                    } else {
                                        cx.theme().muted_foreground
                                    })
                                    .child(status.to_string()),
                            )
                            .when(self.usage.0 + self.usage.1 > 0, |row| {
                                row.child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!(
                                            "in {} · out {}",
                                            human_tokens(self.usage.0),
                                            human_tokens(self.usage.1),
                                        )),
                                )
                            }),
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
                                Button::new(SharedString::from(format!("session-{}", summary.id.0)))
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
                            view.update(cx, |view, cx| view.render_turn(index, cx))
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

/// `1234` → "1.2k" — token counts in the status line stay scannable.
fn human_tokens(count: u64) -> String {
    if count >= 10_000 {
        format!("{:.0}k", count as f64 / 1000.)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1000.)
    } else {
        format!("{count}")
    }
}
