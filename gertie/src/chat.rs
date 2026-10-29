//! The M2 chat: session sidebar + transcript + the agent loop, with tool
//! cards that gate mutating calls behind approve/deny.

use crate::bootstrap::Reloaded;
use crate::settings_edit::{SettingsEvent, SettingsPanelView};
use futures_util::StreamExt as _;
use gertie_core::{
    LoopEvent, MessageId, PermissionBroker, PermissionDecision, Role, SessionId, SessionStore,
    SharedHistory, SnapshotStore, ToolCall, ToolRegistry, TurnSpec, run_turn,
};
use gertie_providers::AnyProvider;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{ActiveTheme, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, Focusable, IntoElement,
    ListAlignment, ListState, ParentElement, Render, SharedString, StatefulInteractiveElement as _,
    Styled, Window, div, list, prelude::*, px,
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

/// A pending image attachment: clipboard bytes wrapped in a data URL,
/// held until send. `id` is gpui's content hash — unique per image, so
/// chips and their remove buttons key off it.
struct Attachment {
    id: u64,
    name: String,
    data_url: String,
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
    provider: Option<Arc<AnyProvider>>,
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
    /// True while the turn is alive but nothing visible is happening —
    /// the request is in flight or the model is chewing on tool results.
    /// Drives the in-transcript "Thinking…" row: the header status alone
    /// was too easy to miss, and a quiet transcript reads as a hang.
    thinking: bool,
    /// Session token totals from the provider's usage events — persisted
    /// per session, so a resumed chat shows what it spent before too.
    usage: (u64, u64),
    /// Token totals across all sessions, for the settings panel.
    /// Refreshed when the panel opens, never per render.
    total_usage: (u64, u64),
    /// The last request's context size (prompt + completion) — what the
    /// next request carries, the number the compaction check watches.
    /// Shown as a live gauge under the composer.
    context_used: u64,
    /// The provider's context window, when it declares one — the gauge's
    /// denominator.
    context_window: Option<u64>,
    /// Prompt templates for `/name` expansion in the composer. The
    /// builtins /compact and /export are methods, not files.
    templates: Vec<gertie_tools::templates::Template>,
    /// Shared approval registry: leader and worker asks, one answer path.
    broker: Arc<PermissionBroker>,
    /// The front's current session, read by subagent dispatch to link
    /// worker runs to the chat that dispatched them.
    session_scope: gertie_core::SessionScope,
    input: Entity<TextareaState>,
    /// Session-rename flow: which sidebar row is being renamed, and the
    /// field editing its title (Enter commits; clicking away cancels).
    renaming: Option<SessionId>,
    rename_input: Entity<InputState>,
    /// Two-step delete: the row whose ✕ was clicked once — the second
    /// click confirms. A destructive action with no dialog needs a
    /// speed bump.
    confirming_delete: Option<SessionId>,
    /// The right column: this chat's worker runs, or the settings view.
    /// One slot — opening one closes the other.
    panel: Panel,
    worker_runs: Vec<(gertie_core::SessionSummary, bool)>,
    /// The settings editor (summary + form) — its Saved event bubbles
    /// to the workspace, which reloads the bootstrap and applies here.
    settings_panel: Entity<SettingsPanelView>,
    list_state: ListState,
    streaming: bool,
    /// Mid-run steering: user messages land in the running turn at the
    /// next round boundary. Set while a turn drives; cleared at its end.
    steer_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Follow-ups typed while streaming with the secondary Enter — they
    /// wait out the run, then each fires as its own turn.
    queued: Vec<String>,
    /// Images waiting to ride with the next sent message.
    attachments: Vec<Attachment>,
    /// Bytes of the streaming assistant row already written to the
    /// store. Deltas persist in coarse steps (a write per token would
    /// dominate the render loop); transitions flush the rest.
    stream_persisted: usize,
    /// Tool cards with expanded output (beyond the 12-line preview),
    /// keyed by turn index; cleared on session switch.
    expanded_tools: std::collections::HashSet<usize>,
}

impl ChatView {
    /// The constructor is UI-wiring, not an abstraction boundary; every
    /// argument is a genuine dependency of the view.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<SessionStore>,
        provider: Option<Arc<AnyProvider>>,
        snapshots: Option<Arc<SnapshotStore>>,
        tools: ToolRegistry,
        endpoint_label: String,
        system_prompt: Option<String>,
        templates: Vec<gertie_tools::templates::Template>,
        broker: Arc<PermissionBroker>,
        session_scope: gertie_core::SessionScope,
        // The settings editor over the live config.
        settings_panel: Entity<SettingsPanelView>,
        // Resume the newest stored session (window open), or start
        // blank (a new tab — two views on one session would diverge).
        resume: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // The composer is a multi-line text area: Enter sends,
        // Shift+Enter is a newline (long prompts, pasted snippets).
        // One row at rest, growing with content up to a cap — a tall
        // empty box made the attach button look lost beside it.
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Message gertie… — / for commands")
                .auto_grow(1, 6)
        });
        cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter {
                secondary,
                shift: false,
            } = event
            {
                // A slash popup being open means Enter accepts the top
                // match instead of sending.
                if !this.accept_slash_top(window, cx) {
                    this.send(*secondary, window, cx)
                }
            }
        })
        .detach();
        // The rename field commits on Enter; Escape/clicking away
        // cancels implicitly.
        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("New title…"));
        cx.subscribe_in(
            &rename_input,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.commit_rename(window, cx)
                }
            },
        )
        .detach();
        // A settings save bubbles to the workspace, which reloads the
        // bootstrap and pushes the fresh provider into every tab.
        cx.subscribe_in(
            &settings_panel,
            window,
            |_this, _, event: &SettingsEvent, _, cx| match event {
                SettingsEvent::Saved => cx.emit(ChatViewEvent::ConfigSaved),
            },
        )
        .detach();

        let sessions = store.list_sessions().unwrap_or_default();
        // Resume the newest session if there is one; otherwise the first
        // message creates the session (lazy, so no empty rows accumulate).
        let session_id = if resume {
            sessions.first().map(|summary| summary.id)
        } else {
            None
        };
        session_scope.set(session_id);

        let (session_usage, context_used) = session_id
            .and_then(|id| sessions.iter().find(|summary| summary.id == id))
            .map(|summary| {
                (
                    (summary.input_tokens, summary.output_tokens),
                    summary.context_tokens,
                )
            })
            .unwrap_or(((0, 0), 0));
        let total_usage = store.total_usage();
        let context_window = provider.as_ref().and_then(|p| p.context_window());

        let mut view = Self {
            store,
            provider,
            snapshots,
            tools,
            endpoint_label,
            system_prompt,
            templates,
            sessions,
            session_id,
            turns: Vec::new(),
            history: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            streaming_assistant: None,
            last_db_id: None,
            activity: "starting…".into(),
            thinking: false,
            usage: session_usage,
            total_usage,
            context_used,
            context_window,
            broker,
            session_scope,
            input,
            renaming: None,
            rename_input,
            confirming_delete: None,
            panel: Panel::None,
            worker_runs: Vec::new(),
            settings_panel,
            // Tail: follow new content while pinned to the bottom, stop
            // when the user scrolls up, resume when they return.
            list_state: {
                let state = ListState::new(0, ListAlignment::Bottom, px(256.));
                state.set_follow_mode(gpui_kit::FollowMode::Tail);
                state
            },
            streaming: false,
            steer_tx: None,
            queued: Vec::new(),
            attachments: Vec::new(),
            stream_persisted: 0,
            expanded_tools: Default::default(),
        };
        view.reload_session(cx);
        window.focus(&view.input.read(cx).focus_handle(cx), cx);
        view
    }

    /// The session the transcript is bound to, for tests and tooling.
    pub fn session_id(&self) -> Option<SessionId> {
        self.session_id
    }

    /// How many turns the transcript holds, for tests and tooling.
    pub fn turn_count_for_test(&self) -> usize {
        self.turns.len()
    }

    /// The open session's title, for the tab bar. Blank chats say so.
    pub fn tab_title(&self) -> SharedString {
        match self
            .session_id
            .and_then(|id| self.sessions.iter().find(|summary| summary.id == id))
        {
            Some(summary) => summary.title.clone().into(),
            None => "new chat".into(),
        }
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
                            text: display_text(&row.content),
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
        self.thinking = false;
        self.streaming_assistant = None;
        self.expanded_tools.clear();
        self.sync_list(cx);
    }

    fn sync_list(&mut self, cx: &mut Context<Self>) {
        self.list_state.reset(self.item_count());
        // Jump to the end AND re-engage tail-following (a session switch
        // or a fresh chat starts following again).
        self.list_state.scroll_to_end();
        cx.notify();
    }

    /// The transcript's item count: one row per turn, plus the activity
    /// row while the model is chewing and nothing else is on screen.
    fn item_count(&self) -> usize {
        self.turns.len() + usize::from(self.show_activity_row())
    }

    /// Whether the "Thinking…" row belongs at the bottom right now: the
    /// turn is alive, but no card, prompt, or streaming text is telling
    /// the story — the exact gaps that used to read as a hang.
    fn show_activity_row(&self) -> bool {
        let pending = self.turns.iter().any(|turn| {
            matches!(
                turn,
                Turn::Tool {
                    status: ToolStatus::Pending(_),
                    ..
                }
            )
        });
        self.streaming && self.thinking && !pending
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
        self.session_scope.set(None);
        self.usage = (0, 0);
        self.context_used = 0;
        self.refresh_agent_runs(cx);
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
        // Pending attachments are composer state, not session state —
        // they don't hop conversations.
        self.attachments.clear();
        self.session_id = Some(session_id);
        self.session_scope.set(Some(session_id));
        let summary = self
            .sessions
            .iter()
            .find(|summary| summary.id == session_id)
            .cloned();
        self.usage = summary
            .as_ref()
            .map(|summary| (summary.input_tokens, summary.output_tokens))
            .unwrap_or((0, 0));
        self.context_used = summary.map(|summary| summary.context_tokens).unwrap_or(0);
        self.refresh_agent_runs(cx);
        self.reload_session(cx);
        window.focus(&self.input.read(cx).focus_handle(cx), cx);
    }

    /// The agents panel toggle: opens/closes and refreshes the run list.
    fn toggle_agents(
        &mut self,
        _event: &gpui_kit::ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.panel = if self.panel == Panel::Agents {
            Panel::None
        } else {
            Panel::Agents
        };
        self.refresh_agent_runs(cx);
    }

    /// The settings panel toggle. One column, one panel: opening one
    /// closes the other.
    fn toggle_settings(
        &mut self,
        _event: &gpui_kit::ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.panel = if self.panel == Panel::Settings {
            Panel::None
        } else {
            Panel::Settings
        };
        self.total_usage = self.store.total_usage();
        cx.notify();
    }

    /// Reloads this chat's worker runs (summary + report status). Cheap
    /// queries on agent-tool events only — never per render.
    fn refresh_agent_runs(&mut self, cx: &mut Context<Self>) {
        self.worker_runs = self
            .session_id
            .map(|session| {
                self.store
                    .list_child_sessions(session)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|summary| {
                        let reported = self.store.worker_run_reported(summary.id);
                        (summary, reported)
                    })
                    .collect()
            })
            .unwrap_or_default();
        cx.notify();
    }

    /// Puts a sidebar row into rename mode, prefilled with its title.
    fn start_rename(&mut self, session_id: SessionId, window: &mut Window, cx: &mut Context<Self>) {
        self.renaming = Some(session_id);
        self.confirming_delete = None;
        let title = self
            .sessions
            .iter()
            .find(|summary| summary.id == session_id)
            .map(|summary| summary.title.to_string())
            .unwrap_or_default();
        self.rename_input.update(cx, |state, cx| {
            state.set_value(title, window, cx);
        });
        window.focus(&self.rename_input.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// Enter in the rename field: persist the title, refresh the sidebar.
    /// Empty input cancels — an unnamed session is worse than a stale title.
    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session_id) = self.renaming.take() else {
            return;
        };
        let title = self.rename_input.read(cx).value().trim().to_string();
        if !title.is_empty() {
            let _ = self.store.retitle_session(session_id, &title);
            self.sessions = self.store.list_sessions().unwrap_or_default();
        }
        window.focus(&self.input.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// The ✕ on a sidebar row: first click arms the confirm ("sure?"),
    /// second click deletes. Deleting the open session returns to the
    /// blank transcript.
    fn delete_clicked(
        &mut self,
        session_id: SessionId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A streaming transcript can't be torn down; also refuse to arm
        // a confirm for it so the double-click can't race the stream.
        if self.streaming && self.session_id == Some(session_id) {
            return;
        }
        if self.confirming_delete == Some(session_id) {
            self.confirming_delete = None;
            if self.store.delete_session(session_id).is_ok() && self.session_id == Some(session_id)
            {
                self.session_id = None;
                self.turns.clear();
                self.last_db_id = None;
                self.streaming_assistant = None;
                self.history.blocking_lock().clear();
            }
            self.sessions = self.store.list_sessions().unwrap_or_default();
            self.refresh_agent_runs(cx);
            self.sync_list(cx);
        } else {
            self.confirming_delete = Some(session_id);
            self.renaming = None;
            cx.notify();
        }
    }

    /// The attach affordance: wraps the clipboard's image (if any) as a
    /// data URL and parks it for the next send. Text clipboards do
    /// nothing — the textarea already pastes those itself.
    fn attach_from_clipboard(&mut self, cx: &mut Context<Self>) {
        use base64::Engine as _;
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        for entry in item.entries {
            let gpui_kit::ClipboardEntry::Image(image) = entry else {
                continue;
            };
            let (media_type, extension) = match image.format {
                gpui_kit::ImageFormat::Jpeg => ("image/jpeg", "jpg"),
                gpui_kit::ImageFormat::Webp => ("image/webp", "webp"),
                gpui_kit::ImageFormat::Gif => ("image/gif", "gif"),
                _ => ("image/png", "png"),
            };
            let data_url = format!(
                "data:{media_type};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&image.bytes)
            );
            // Re-pasting the same image replaces the chip, not stacks it.
            self.attachments.retain(|held| held.id != image.id);
            self.attachments.push(Attachment {
                id: image.id,
                name: format!("image.{extension}"),
                data_url,
            });
            cx.notify();
            return;
        }
    }

    /// Pulls the chip back off the composer.
    fn remove_attachment(&mut self, id: u64, cx: &mut Context<Self>) {
        self.attachments.retain(|held| held.id != id);
        cx.notify();
    }

    /// Turns pending attachments into (markers for the stored text,
    /// data URLs for the wire). Bytes land in the attachments dir under
    /// their content hash; the DB keeps only the path — a reload reads
    /// the file back. A write failure degrades to wire-only: the live
    /// turn still sees the image, it just won't survive a restart.
    fn take_attachments(&mut self) -> (String, Vec<String>) {
        use base64::Engine as _;
        let mut images = Vec::new();
        let mut markers = String::new();
        let dir = crate::config::Config::data_dir().map(|dir| dir.join("attachments"));
        for attachment in std::mem::take(&mut self.attachments) {
            images.push(attachment.data_url.clone());
            let stored = match (
                dir.as_deref(),
                gertie_core::data_url_parts(&attachment.data_url),
            ) {
                (Some(dir), Some((_, payload))) => base64::engine::general_purpose::STANDARD
                    .decode(payload)
                    .ok()
                    .map(|bytes| {
                        let path = dir.join(format!("{}-{}", attachment.id, attachment.name));
                        std::fs::create_dir_all(dir).ok()?;
                        std::fs::write(&path, bytes).ok()?;
                        Some(path)
                    })
                    .unwrap_or(None),
                _ => None,
            };
            if let Some(path) = stored {
                markers.push_str(&format!(
                    "\n\n{}{}}}",
                    gertie_core::IMAGE_MARKER_PREFIX,
                    path.display()
                ));
            }
        }
        (markers, images)
    }

    /// Accepts the popup's top match (what Enter does while the slash
    /// popup is open): builtins execute, templates expand into the
    /// composer for editing — args ride along. Returns false when no
    /// popup is open, so Enter falls through to sending.
    fn accept_slash_top(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let value = self.input.read(cx).value().to_string();
        let matches = slash_matches(&value, &self.templates);
        let Some(first) = matches.first() else {
            return false;
        };
        let (builtin, text) = match first {
            SlashMatch::Builtin(name, _) => (true, format!("/{name}")),
            SlashMatch::Template(index) => {
                let args = gertie_tools::templates::parse_invocation(&value)
                    .map(|(_, args)| args)
                    .unwrap_or_default();
                (false, self.templates[*index].render(&args))
            }
        };
        self.accept_slash_text(builtin, text, window, cx);
        true
    }

    /// Accepts one popup row: builtins execute through the normal send
    /// path; templates expand into the composer — you see the real
    /// prompt, edit it, send it yourself.
    fn accept_slash_text(
        &mut self,
        builtin: bool,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input
            .update(cx, |input, cx| input.set_value(text, window, cx));
        if builtin {
            self.send(false, window, cx);
        }
        cx.notify();
    }

    fn send(&mut self, queue_only: bool, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.input.read(cx).value().trim().to_string();
        if typed.is_empty() {
            return;
        }
        // Slash commands act on the session, not through the model:
        // builtins first (/compact, /export), then file templates. An
        // unknown name reports and keeps the input — the user fixes
        // the spelling instead of retyping the message.
        let text = match gertie_tools::templates::parse_invocation(&typed) {
            Some((name, args)) => match name.as_str() {
                "compact" => {
                    self.input
                        .update(cx, |input, cx| input.set_value("", window, cx));
                    self.compact_now(cx);
                    return;
                }
                "export" => {
                    self.input
                        .update(cx, |input, cx| input.set_value("", window, cx));
                    self.export_now(cx);
                    return;
                }
                _ => {
                    let Some(template) = gertie_tools::templates::lookup(&self.templates, &name)
                    else {
                        let mut names: Vec<String> = self
                            .templates
                            .iter()
                            .map(|t| format!("/{}", t.name))
                            .collect();
                        names.push("/compact".into());
                        names.push("/export".into());
                        self.push_notice(
                            format!("No template `/{name}` — available: {}.", names.join(", ")),
                            cx,
                        );
                        return;
                    };
                    template.render(&args)
                }
            },
            None => typed.clone(),
        };
        // The run is busy: the words still land. Enter steers the
        // running turn (the model reads it at the next round
        // boundary); the secondary Enter queues a follow-up that
        // runs as its own turn when this one ends.
        if self.streaming {
            self.input
                .update(cx, |input, cx| input.set_value("", window, cx));
            window.focus(&self.input.read(cx).focus_handle(cx), cx);
            if queue_only {
                self.queued.push(text);
            } else if let Some(steer_tx) = &self.steer_tx {
                let _ = steer_tx.send(text);
            } else {
                self.queued.push(text);
            }
            cx.notify();
            return;
        }
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        window.focus(&self.input.read(cx).focus_handle(cx), cx);

        // First message of a chat: the session is born here, titled by
        // what was typed — the invocation, not the expansion.
        let session_id = match self.session_id {
            Some(session_id) => session_id,
            None => {
                let title: String = typed.chars().take(48).collect();
                let session_id = self.store.create_session(&title).expect("create session");
                self.session_id = Some(session_id);
                self.sessions = self.store.list_sessions().unwrap_or_default();
                session_id
            }
        };

        // Attachments become storage markers + wire data URLs; the
        // wire text stays clean — the model gets the images as images.
        let (markers, images) = self.take_attachments();
        let stored = format!("{text}{markers}");

        let db_id = self
            .store
            .append_message(session_id, self.last_db_id, Role::User, &stored)
            .ok();
        if db_id.is_some() {
            self.last_db_id = db_id;
        }
        self.turns.push(Turn::Message {
            role: Role::User,
            text: display_text(&stored),
            db_id,
        });
        self.drive_turn(text, images, cx);
    }

    /// `/compact`: summarize the history now instead of waiting for the
    /// context ceiling. Shapes the next request, not the record — the
    /// DB keeps every row, so a reload restores full history.
    fn compact_now(&mut self, cx: &mut Context<Self>) {
        if self.streaming {
            self.push_notice("A run is in flight — wait for it to finish.", cx);
            return;
        }
        let Some(provider) = self.provider.clone() else {
            self.push_notice(NO_ENDPOINT_NOTICE, cx);
            return;
        };
        let history = self.history.clone();
        // Below twice the kept tail there's nothing to give back —
        // summarizing would spend tokens to shrink nothing.
        let big_enough = {
            let current = history.blocking_lock();
            current.len() >= 2 * gertie_core::agent_loop::KEEP_TAIL
        };
        if !big_enough {
            self.push_notice("Nothing to compact — the history is already short.", cx);
            return;
        }
        cx.spawn(async move |this, cx| {
            let result = gertie_core::agent_loop::compact_history(provider, history).await;
            let _ = this.update(cx, |this, cx| match result {
                Some((kept, total)) => {
                    this.on_compacted(kept, total, cx);
                    this.activity = "Context compacted".into();
                }
                None => this.push_notice(
                    "Nothing to compact — history is small (or the summarizer failed).",
                    cx,
                ),
            });
        })
        .detach();
    }

    /// `/export`: write the session's persisted rows as JSONL — one
    /// object per message, the shape the model saw. Lands in the data
    /// dir's `exports/`; the notice reports the path.
    fn export_now(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id else {
            self.push_notice("Nothing to export — this chat has no messages yet.", cx);
            return;
        };
        let title = self
            .sessions
            .iter()
            .find(|summary| summary.id == session_id)
            .map(|summary| summary.title.clone())
            .unwrap_or_else(|| "session".into());
        let slug: String = title
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let path = crate::config::Config::data_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("exports")
            .join(format!("{}-{}.jsonl", slug.trim_matches('-'), session_id.0));
        match self.store.export_session_jsonl(session_id, &path) {
            Ok(count) => self.push_notice(
                format!("Exported {count} messages → {}", path.display()),
                cx,
            ),
            Err(error) => self.push_notice(format!("Export failed: {error}"), cx),
        }
    }

    /// A muted system row in the transcript — notices, command results.
    /// Display-only: never persisted, never sent.
    fn push_notice(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.turns.push(Turn::Message {
            role: Role::System,
            text: text.into(),
            db_id: None,
        });
        self.sync_list(cx);
    }

    /// Hands the turn to the agent loop and renders its event stream:
    /// deltas stream into an assistant row, tool calls become cards, and
    /// results persist as tool rows the model also sees on reload.
    fn drive_turn(&mut self, user_text: String, user_images: Vec<String>, cx: &mut Context<Self>) {
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
        self.context_window = context_window;
        let provider: Arc<dyn gertie_core::Provider> = provider;
        // Subagent dispatch reads this to link its runs to this chat.
        self.session_scope.set(self.session_id);
        // Steering: the channel lives exactly as long as the turn —
        // Enter while streaming lands here; at the turn's end the
        // sender drops and Enter queues instead.
        let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel();
        let stream = run_turn(TurnSpec {
            provider,
            tools: self.tools.clone(),
            history: self.history.clone(),
            user_message: user_text,
            user_images,
            snapshots: self.snapshots.clone(),
            context_window,
            steer: Some(steer_rx),
            permission_broker: Some(self.broker.clone()),
            prompt_source: gertie_core::PromptSource::Leader,
            system_prompt: self.system_prompt.clone(),
        });
        // Subagent asks arrive here, not on the leader's stream — a
        // worker's loop runs inside a tool, so nobody drains its events.
        let mut worker_asks = self.broker.subscribe();

        self.streaming = true;
        self.streaming_assistant = None;
        self.activity = "Thinking…".into();
        self.thinking = true;
        self.steer_tx = Some(steer_tx);
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
                            // The last request's prompt + completion is
                            // the context the next request carries — the
                            // live number the compaction check watches.
                            this.context_used = input_tokens + output_tokens;
                            // The ledger outlives the view: persist per
                            // request so a resumed session shows its
                            // whole spend, not just since reopen.
                            if let Some(session) = this.session_id {
                                let _ = this
                                    .store
                                    .add_usage(session, input_tokens, output_tokens);
                            }
                            cx.notify();
                        });
                    }
                    LoopEvent::Compacted { kept, total } => {
                        let _ = this.update(cx, |this, cx| this.on_compacted(kept, total, cx));
                    }
                    LoopEvent::Steered { text } => {
                        let _ = this.update(cx, |this, cx| this.on_steered(&text, cx));
                    }
                    LoopEvent::Error { message } => {
                        let _ = this.update(cx, |this, cx| this.on_error(&message, cx));
                    }
                    LoopEvent::Done => break,
                        }
                    }
                    ask = worker_asks.recv() => {
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
                this.flush_stream(cx);
                this.streaming = false;
                this.thinking = false;
                this.streaming_assistant = None;
                // The turn is over: steering closes, the queue takes
                // over. A queued follow-up runs as its own turn, right
                // now — the composer keeps whatever was typed meanwhile.
                this.steer_tx = None;
                this.sessions = this.store.list_sessions().unwrap_or_default();
                this.sync_list(cx);
                if !this.queued.is_empty() {
                    let next = this.queued.remove(0);
                    let session_id = match this.session_id {
                        Some(session_id) => session_id,
                        None => return,
                    };
                    let db_id = this
                        .store
                        .append_message(session_id, this.last_db_id, Role::User, &next)
                        .ok();
                    if db_id.is_some() {
                        this.last_db_id = db_id;
                    }
                    this.turns.push(Turn::Message {
                        role: Role::User,
                        text: next.clone(),
                        db_id,
                    });
                    this.drive_turn(next, Vec::new(), cx);
                }
            });
        })
        .detach();
    }

    /// Streams a delta into the round's assistant turn, creating its row
    /// (and database row) on the first delta.
    fn on_delta(&mut self, delta: &str, cx: &mut Context<Self>) {
        self.activity = "Writing a reply…".into();
        self.thinking = false;
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
                self.stream_persisted = 0;
                // The list's item count is stale the moment a turn is
                // added — without a reset, scroll_to_end targets the old
                // end and the new turn renders below the viewport (the
                // "reply appeared only after I typed again" bug).
                self.list_state.reset(self.item_count());
                index
            }
        };

        // Tail mode handles following (no per-delta scroll_to_end — it
        // used to yank the viewport on every token). Persistence is
        // throttled: a SQLite write per delta would dominate streaming.
        if let Some(Turn::Message { text, db_id, .. }) = self.turns.get_mut(index) {
            text.push_str(delta);
            if text.len() - self.stream_persisted >= 2048
                && let Some(db_id) = *db_id
            {
                let _ = self.store.update_message(db_id, text);
                self.stream_persisted = text.len();
            }
        }
        cx.notify();
    }

    /// Writes the streaming row's unsaved tail to the store. Called when
    /// the stream changes shape — a tool starts, a prompt appears, the
    /// turn ends — so a reload always sees the full text.
    fn flush_stream(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.streaming_assistant
            && let Some(Turn::Message { text, db_id, .. }) = self.turns.get_mut(index)
        {
            if text.len() > self.stream_persisted
                && let Some(db_id) = *db_id
            {
                let _ = self.store.update_message(db_id, text);
            }
            self.stream_persisted = text.len();
        }
        cx.notify();
    }

    /// A read-only tool started executing: give it a running card so the
    /// quiet parts of a turn still look alive.
    fn on_tool_started(&mut self, call: ToolCall, cx: &mut Context<Self>) {
        self.activity = crate::tools_ui::action_label(&call.name, &call.arguments, true);
        self.thinking = false;
        self.flush_stream(cx);
        self.streaming_assistant = None;
        let dispatches_workers = call.name.starts_with("agent_");
        self.turns.push(Turn::Tool {
            call,
            status: ToolStatus::Running,
            auto_approved: true,
            worker: false,
        });
        if dispatches_workers {
            // The worker run's child session exists once the tool starts
            // executing; a second refresh lands with its result.
            self.refresh_agent_runs(cx);
        }
        self.sync_list(cx);
    }

    /// A mutating tool wants to run: park the request id on the card
    /// until the human answers through the broker.
    fn on_tool_requested(&mut self, id: u64, call: ToolCall, cx: &mut Context<Self>) {
        self.activity = "Waiting for your approval…".into();
        self.thinking = false;
        self.flush_stream(cx);
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
        self.activity = "A worker needs your approval…".into();
        self.thinking = false;
        self.flush_stream(cx);
        self.streaming_assistant = None;
        self.turns.push(Turn::Tool {
            call,
            status: ToolStatus::Pending(id),
            auto_approved: false,
            worker: true,
        });
        // A worker is mid-run: its child session exists by now.
        self.refresh_agent_runs(cx);
        self.sync_list(cx);
    }

    /// The tool finished; show the outcome and persist the row so reloads
    /// and the next request see the same thing the model did.
    fn on_tool_result(&mut self, call_id: &str, name: &str, result: &str, cx: &mut Context<Self>) {
        // The tool is done, so the model is chewing on the result before
        // it writes the next thing — say so in the transcript.
        self.activity = "Thinking…".into();
        self.thinking = true;
        self.streaming_assistant = None;
        if name.starts_with("agent_") {
            // The worker's report row is on disk; the panel flips to done.
            self.refresh_agent_runs(cx);
        }

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
        // The thinking row appears here (item count grew by one), so a
        // reset — not just a scroll — keeps the tail pinned.
        self.sync_list(cx);
    }

    /// Loop-level failures (provider exhausted retries, round budget hit)
    /// surface in the transcript like any other turn.
    /// History shrank mid-turn: tell the user honestly — a muted notice
    /// card, not hidden magic. Not persisted: DB rows stay whole, the
    /// compaction is the in-memory view (a reload restores full history
    /// and the loop re-compacts if the threshold trips again).
    fn on_compacted(&mut self, kept: usize, total: usize, cx: &mut Context<Self>) {
        self.activity = "Compacting context…".into();
        self.turns.push(Turn::Message {
            role: Role::System,
            text: format!(
                "⟳ context compacted — {kept} of {total} messages kept, earlier history summarized"
            ),
            db_id: None,
        });
        self.sync_list(cx);
    }

    /// A steered message joined the running turn at a round boundary:
    /// persist and bubble it exactly like a send-time row — the loop
    /// owns the in-memory history, the DB row is ours.
    fn on_steered(&mut self, text: &str, cx: &mut Context<Self>) {
        let db_id = match self.session_id {
            Some(session_id) => {
                let db_id = self
                    .store
                    .append_message(session_id, self.last_db_id, Role::User, text)
                    .ok();
                if db_id.is_some() {
                    self.last_db_id = db_id;
                }
                db_id
            }
            None => None,
        };
        self.turns.push(Turn::Message {
            role: Role::User,
            text: text.to_string(),
            db_id,
        });
        self.sync_list(cx);
    }

    fn on_error(&mut self, message: &str, cx: &mut Context<Self>) {
        self.flush_stream(cx);
        let rendered = format!("[error] {message}");
        self.activity = "Something went wrong".into();
        self.thinking = false;
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
        self.list_state.reset(self.item_count());
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

    /// "Fork from here" on a persisted user turn: branch the conversation
    /// into a new session seeded with everything up to that message, and
    /// switch to it. The original keeps running its own life.
    fn fork_from(&mut self, upto: MessageId, window: &mut Window, cx: &mut Context<Self>) {
        if self.streaming {
            self.push_notice("A run is in flight — wait for it to finish.", cx);
            return;
        }
        let Some(session_id) = self.session_id else {
            return;
        };
        let title = self
            .sessions
            .iter()
            .find(|summary| summary.id == session_id)
            .map(|summary| format!("{} (fork)", summary.title))
            .unwrap_or_else(|| "fork".to_string());
        match self.store.fork_session(session_id, upto, &title) {
            Ok((new_id, _fork_point)) => {
                self.sessions = self.store.list_sessions().unwrap_or_default();
                self.switch_to_session(new_id, window, cx);
            }
            Err(error) => self.push_notice(format!("Fork failed: {error}"), cx),
        }
    }

    fn render_turn(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        // The transcript row that isn't a turn: while the model thinks
        // and nothing else is on screen, a sentinel item past the last
        // turn keeps the conversation visibly alive.
        if index >= self.turns.len() {
            return div()
                .w_full()
                .px_4()
                .pt_1()
                .pb_2()
                .child(self.render_activity_row(cx))
                .into_any_element();
        }
        let inner = match &self.turns[index] {
            Turn::Message { role, text, .. } => self.render_message(role, text, cx),
            Turn::Tool {
                call,
                status,
                auto_approved,
                worker,
            } => self.render_tool(index, call, status, *auto_approved, *worker, cx),
        };
        // Fork affordance: persisted user turns only — "branch from this
        // question" is the mental model that survives contact with use.
        let fork_button = match &self.turns[index] {
            Turn::Message {
                role: Role::User,
                db_id: Some(db_id),
                ..
            } => {
                let db_id = *db_id;
                Some(
                    Button::new(format!("fork-{index}"))
                        .label("fork")
                        .small()
                        .ghost()
                        .tooltip("Branch a new session from here")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.fork_from(db_id, window, cx);
                        })),
                )
            }
            _ => None,
        };
        let inner = match fork_button {
            // The user bubble is right-aligned; the fork button tucks
            // under it, same side, quiet.
            Some(button) => div()
                .w_full()
                .flex()
                .flex_col()
                .items_end()
                .child(inner)
                .child(button)
                .into_any_element(),
            None => inner,
        };
        // Horizontal padding lives on each item, not the list: gpui's
        // `list` paints items at the container's left edge and ignores
        // `px_*` on the list element, so without this the transcript
        // butts against the window border.
        div().w_full().px_4().child(inner).into_any_element()
    }

    /// The quiet-state row: spinner plus what's happening, in the flow
    /// of the conversation where the user is actually looking.
    fn render_activity_row(&self, cx: &Context<Self>) -> AnyElement {
        h_flex()
            .gap_2()
            .items_center()
            .child(Spinner::new().small().color(cx.theme().accent))
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    // Activity strings carry `code` spans (tool labels
                    // quote paths and commands) — same treatment as
                    // the cards.
                    .child(crate::markdown::inline_element(&self.activity, cx)),
            )
            .into_any_element()
    }

    /// The footer line under the composer. Left: this session's spend
    /// and the live context gauge — `12.3k (23%)` — so the distance to
    /// the compaction threshold is visible before it happens. Right:
    /// what you're talking to — `gertie · ppq · glm-5.3-flash` — or,
    /// when the chat is blocked on you, that fact in warning color.
    /// Quiet until there is something to say.
    fn render_token_footer(&self, status: Option<&str>, cx: &Context<Self>) -> AnyElement {
        let mut parts = Vec::new();
        if self.usage.0 + self.usage.1 > 0 {
            parts.push(format!(
                "in {} · out {}",
                human_tokens(self.usage.0),
                human_tokens(self.usage.1)
            ));
        }
        let near_compaction = self
            .context_window
            .is_some_and(|window| self.context_used * 100 >= window.max(1) * 75);
        if self.context_used > 0 {
            let mut gauge = human_tokens(self.context_used);
            if let Some(window) = self.context_window {
                let percent = self.context_used.min(window) * 100 / window.max(1);
                gauge = format!("{gauge} ({percent}%)");
            }
            parts.push(gauge);
        }
        if parts.is_empty() {
            return div().into_any_element();
        }
        let ledger = div()
            .text_size(px(11.))
            .text_color(if near_compaction {
                cx.theme().warning
            } else {
                cx.theme().muted_foreground
            })
            .child(if parts.is_empty() {
                " ".into()
            } else {
                parts.join("  ·  ")
            });
        let ident = div()
            .text_size(px(11.))
            .truncate()
            .text_color(match status {
                Some(_) => cx.theme().warning_foreground,
                None => cx.theme().muted_foreground,
            })
            .child(match status {
                // The one status that earns a place outside the
                // transcript: the chat is blocked on you. Everything
                // else — thinking, writing, tool work — lives in the
                // transcript, where the user is looking anyway.
                Some(status) => status.to_string(),
                None => self.endpoint_label.clone(),
            });
        h_flex()
            .justify_between()
            .gap_4()
            .px_4()
            .pb_2()
            .child(ledger)
            .child(ident)
            .into_any_element()
    }

    fn render_message(&self, role: &Role, text: &str, cx: &App) -> AnyElement {
        match role {
            Role::User => h_flex()
                .w_full()
                .justify_end()
                .pb_2()
                .child(
                    div()
                        .max_w_3_4()
                        .rounded_lg()
                        .px_3()
                        .py_2()
                        .bg(cx.theme().primary)
                        .text_color(cx.theme().primary_foreground)
                        .child(display_text(text)),
                )
                .into_any_element(),
            Role::System => {
                // Loop notices (compaction) — centered, quiet, not a chat
                // message from anyone.
                h_flex()
                    .w_full()
                    .justify_center()
                    .pb_2()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(text.to_string()),
                    )
                    .into_any_element()
            }
            Role::Assistant => {
                let is_error = text.contains("[error]");
                div()
                    .w_full()
                    .pr_8()
                    .pb_2()
                    .text_color(if is_error {
                        cx.theme().danger
                    } else {
                        cx.theme().foreground
                    })
                    .when(is_error, |row| {
                        // Error text is for the human, verbatim — don't
                        // run it through the markdown parser.
                        row.child(text.to_string())
                    })
                    .when(!is_error, |row| {
                        row.child(crate::markdown::render(text, cx))
                    })
                    .into_any_element()
            }
            Role::Tool => div().into_any_element(),
        }
    }

    /// The tool card: a friendly statement of what the model is doing
    /// (not the wire name), the diff for file changes, a one-line
    /// outcome when finished, and approve/deny while pending. The raw
    /// tool output is one click away; it's the exception, not the face.
    fn render_tool(
        &self,
        index: usize,
        call: &ToolCall,
        status: &ToolStatus,
        auto_approved: bool,
        worker: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let running = matches!(status, ToolStatus::Running);
        let label = crate::tools_ui::action_label(&call.name, &call.arguments, running);

        let mut inner = v_flex().gap_2().child(
            h_flex()
                .gap_2()
                .items_center()
                .when(running, |row| {
                    row.child(Spinner::new().xsmall().color(cx.theme().accent))
                })
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(cx.theme().accent)
                        // Labels carry `code` spans (paths, commands) —
                        // through the inline renderer they become chips
                        // instead of literal backticks.
                        .child(crate::markdown::inline_element(&label, cx)),
                )
                .when(worker, |row| {
                    row.child(
                        div()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child("worker"),
                    )
                }),
        );

        match status {
            ToolStatus::Pending(_) => {
                inner = inner
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().warning)
                            .child(if call.name == "bash" {
                                "This runs a shell command — your OK first."
                            } else {
                                "This changes files in the workspace — your OK first."
                            }),
                    )
                    .children(diff_lines(call, cx))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new(SharedString::from(format!("approve-{index}")))
                                    .label("Approve")
                                    .small()
                                    .primary()
                                    .tooltip("Run it")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.decide_tool(index, PermissionDecision::Approve, cx)
                                    })),
                            )
                            .child(
                                Button::new(SharedString::from(format!("deny-{index}")))
                                    .label("Deny")
                                    .small()
                                    .outline()
                                    .tooltip("Refuse this call")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.decide_tool(index, PermissionDecision::Deny, cx)
                                    })),
                            ),
                    );
            }
            ToolStatus::Running => {
                // The header already carries the spinner and the label;
                // a second "running…" line says nothing new.
                inner = inner.when(auto_approved, |row| {
                    row.child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child("read-only, so it runs without asking"),
                    )
                });
            }
            ToolStatus::Done(result) => {
                let failed = result.starts_with("error:")
                    || result.starts_with("exit ")
                    || result.starts_with("blocked by policy")
                    || result.ends_with("denied by user");
                // The diff tells the story for file changes — but never
                // for a call that didn't happen (denied or failed).
                if !failed && let Some(diff) = diff_lines(call, cx) {
                    inner = inner.child(diff);
                }
                let summary = crate::tools_ui::result_summary(&call.name, result);
                let lines: Vec<&str> = result.lines().collect();
                let expanded = self.expanded_tools.contains(&index);

                // Default face: the one-line outcome. The raw result —
                // file contents, match dumps, exit noise — is what the
                // model eats, not what the human asked to read.
                inner = inner.child(
                    div()
                        .text_size(px(12.))
                        .text_color(if failed {
                            cx.theme().danger
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(summary),
                );
                if lines.len() > 1 {
                    if expanded {
                        inner = inner.child(
                            div()
                                .rounded_md()
                                .border_1()
                                .border_color(cx.theme().border)
                                .p_2()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_size(cx.theme().mono_font_size)
                                .text_color(cx.theme().muted_foreground)
                                .child(result.to_string()),
                        );
                    }
                    let (label, tooltip) = if expanded {
                        (
                            "hide raw output".to_string(),
                            "Collapse the raw tool output".to_string(),
                        )
                    } else {
                        (
                            format!("show raw output ({} lines)", lines.len()),
                            "What the model actually received".to_string(),
                        )
                    };
                    inner = inner.child(
                        Button::new(SharedString::from(format!("tool-toggle-{index}")))
                            .label(label)
                            .xsmall()
                            .ghost()
                            .tooltip(tooltip)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.expanded_tools.remove(&index) {
                                    this.expanded_tools.insert(index);
                                }
                                cx.notify();
                            })),
                    );
                }
            }
        }

        v_flex()
            .w_full()
            .pr_8()
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

    /// The agents panel: this chat's worker runs, oldest first, with a
    /// report status per run. A live org-chart in miniature — dispatch,
    /// watch, inspect later via search_history.
    fn render_agents_panel(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        v_flex()
            .id("agents-panel")
            .w(px(240.))
            .h_full()
            .flex_shrink_0()
            .border_l_1()
            .border_color(theme.border)
            .pt_3()
            .px_3()
            .pb_3()
            .gap_1()
            .overflow_y_scroll()
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child("WORKER RUNS"),
            )
            .when(self.worker_runs.is_empty(), |col| {
                col.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .child("none yet — dispatch an agent and its run shows up here"),
                )
            })
            .children(self.worker_runs.iter().map(|(summary, reported)| {
                h_flex()
                    .gap_1()
                    .items_center()
                    .min_w_0()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .child(summary.title.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .flex_shrink_0()
                            .text_color(if *reported {
                                theme.muted_foreground
                            } else {
                                theme.accent
                            })
                            .child(if *reported { "done" } else { "running…" }),
                    )
            }))
            .into_any_element()
    }
}

/// Which right-column panel is open, if any. One slot: the agents
/// org-chart and the settings view alternate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Panel {
    #[default]
    None,
    Agents,
    Settings,
}

/// Bubbles out of the chat: a settings save landed. The workspace
/// (which owns the bootstrap) reloads and fans the result to every tab.
pub enum ChatViewEvent {
    ConfigSaved,
}

impl EventEmitter<ChatViewEvent> for ChatView {}

impl ChatView {
    /// Applies a fresh bootstrap snapshot — what a settings save
    /// triggers. The provider swap takes effect on the next turn's
    /// send (a running turn keeps the provider it started with).
    pub fn apply_provider(&mut self, update: Reloaded, cx: &mut Context<Self>) {
        self.provider = update.provider;
        self.endpoint_label = update.model_label;
        self.templates = update.templates;
        let config = update.config.clone();
        let summary = (*update.settings).clone();
        self.settings_panel.update(cx, |panel, cx| {
            panel.set_config(&config, summary, cx);
        });
        cx.notify();
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
            Some("approval needed")
        } else {
            None
        };

        h_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            // h_flex centers children on the cross axis by default —
            // stretch instead, so the main row fills the window height
            // and the transcript list keeps its flex sizing.
            .items_stretch()
            .child(
                // Session sidebar: sessions scroll, the utility
                // buttons stay put at the foot.
                v_flex()
                    .w(px(240.))
                    .h_full()
                    .flex_shrink_0()
                    .border_r_1()
                    .border_color(cx.theme().border)
                    .child(
                        v_flex()
                            .id("session-sidebar")
                            .flex_1()
                            .min_h_0()
                            .pt_3()
                            .px_3()
                            .gap_1()
                            .overflow_y_scroll()
                            .child(
                                Button::new("new-chat")
                                    .label("New chat")
                                    .small()
                                    .w_full()
                                    .tooltip("Start a new conversation in this tab")
                                    .on_click(cx.listener(Self::new_session)),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .mt_1()
                                    .text_size(px(11.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child("SESSIONS"),
                            )
                            .children(self.sessions.iter().map(|summary| {
                                let session_id = summary.id;
                                if self.renaming == Some(session_id) {
                                    // Rename row: the field replaces the
                                    // row; Enter commits.
                                    return h_flex()
                                        .w_full()
                                        .child(Input::new(&self.rename_input).small());
                                }
                                let confirming = self.confirming_delete == Some(session_id);
                                let delete_label = if confirming { "sure?" } else { "✕" };
                                let is_active = self.session_id == Some(session_id);
                                let delete_button = Button::new(SharedString::from(format!(
                                    "delete-{}",
                                    session_id.0
                                )))
                                .label(delete_label)
                                .xsmall()
                                .ghost()
                                .tooltip(if confirming {
                                    "Click again to delete permanently"
                                } else {
                                    "Delete this conversation"
                                });
                                let delete_button = if confirming {
                                    delete_button.danger()
                                } else {
                                    delete_button
                                };
                                h_flex()
                                    .w_full()
                                    .min_w_0()
                                    .gap_0p5()
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "session-{}",
                                            session_id.0
                                        )))
                                        .label(summary.title.clone())
                                        .small()
                                        .flex_1()
                                        .min_w_0()
                                        .when(is_active, |button| {
                                            // The open conversation is
                                            // tinted, not loud — the
                                            // accent at conversation
                                            // weight.
                                            button.custom(
                                                ButtonCustomVariant::new(cx)
                                                    .color(cx.theme().accent.opacity(0.14))
                                                    .hover(cx.theme().accent.opacity(0.2))
                                                    .active(cx.theme().accent.opacity(0.24)),
                                            )
                                        })
                                        .tooltip(summary.title.clone())
                                        .on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                this.switch_to_session(session_id, window, cx);
                                            }),
                                        ),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "rename-{}",
                                            session_id.0
                                        )))
                                        .label("✎")
                                        .xsmall()
                                        .ghost()
                                        .tooltip("Rename this conversation")
                                        .on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                this.start_rename(session_id, window, cx);
                                            }),
                                        ),
                                    )
                                    .child(delete_button.on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            this.delete_clicked(session_id, window, cx);
                                        },
                                    )))
                            })),
                    )
                    .child(
                        // The two utilities: stacked at the
                        // sidebar's foot — findable, never
                        // in the way.
                        v_flex()
                            .gap_1()
                            .px_3()
                            .pt_2()
                            .pb_3()
                            .child(
                                Button::new("toggle-agents")
                                    .label("agents")
                                    .small()
                                    .ghost()
                                    .w_full()
                                    .tooltip("Worker runs dispatched from this chat")
                                    .on_click(cx.listener(Self::toggle_agents)),
                            )
                            .child(
                                Button::new("toggle-settings")
                                    .label("settings")
                                    .small()
                                    .ghost()
                                    .w_full()
                                    .tooltip("Configuration, theme, and token totals")
                                    .on_click(cx.listener(Self::toggle_settings)),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(
                        // The transcript: flex sizing goes directly on the
                        // list element — a wrapper div collapses it.
                        // Horizontal padding is per-item (see render_turn):
                        // gpui's list paints items at the container edge
                        // and would ignore px_* here. The pt/pb DO apply.
                        v_flex()
                            .flex_1()
                            .min_h_0()
                            .relative()
                            .child(
                                list(self.list_state.clone(), move |index, _window, cx| {
                                    view.update(cx, |view, cx| view.render_turn(index, cx))
                                        .unwrap_or_else(|_| div().into_any_element())
                                })
                                .flex_1()
                                .min_h_0()
                                .overflow_hidden()
                                .pt_3()
                                .pb_4(),
                            )
                            // Empty state: the transcript is quiet until
                            // it isn't — say so, quietly.
                            .when(self.turns.is_empty(), |col| {
                                col.child(
                                    div()
                                        .absolute()
                                        .inset_0()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            v_flex()
                                                .items_center()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .text_size(px(14.))
                                                        .text_color(cx.theme().foreground)
                                                        .child("Ready."),
                                                )
                                                .child(
                                                    div()
                                                        .text_size(px(12.))
                                                        .text_color(cx.theme().muted_foreground)
                                                        .child(
                                                            "Ask, paste, or point me at a file.",
                                                        ),
                                                ),
                                        ),
                                )
                            })
                            .child(
                                // Composer + footer: the input and the
                                // token ledger live under the
                                // conversation — the sidebar owns the
                                // full window height, so nothing here
                                // spans the window.
                                v_flex()
                                    .border_t_1()
                                    .border_color(cx.theme().border)
                                    .when(!self.queued.is_empty(), |composer| {
                                        composer.child(
                                            h_flex()
                                                .px_4()
                                                .pt_2()
                                                .gap_2()
                                                .items_center()
                                                .text_size(px(11.))
                                                .text_color(cx.theme().muted_foreground)
                                                .child(format!(
                                                    "{} queued — runs when this turn finishes",
                                                    self.queued.len()
                                                )),
                                        )
                                    })
                                    .when(
                                        !slash_matches(
                                            &self.input.read(cx).value(),
                                            &self.templates,
                                        )
                                        .is_empty(),
                                        |composer| {
                                            // Slash discovery: typing `/` lists
                                            // every command; typing narrows it.
                                            // Enter (or a click) accepts — a
                                            // template expands into the composer
                                            // so the real prompt is visible
                                            // before it goes anywhere.
                                            let value = self.input.read(cx).value().to_string();
                                            let matches =
                                                slash_matches(&value, &self.templates);
                                            composer.child(
                                                v_flex()
                                                    .id("slash-popup")
                                                    .mx_4()
                                                    .mt_2()
                                                    .max_h(px(220.))
                                                    .overflow_y_scroll()
                                                    .rounded_md()
                                                    .border_1()
                                                    .border_color(cx.theme().border)
                                                    .bg(cx.theme().background)
                                                    .py_1()
                                                    .children(matches.iter().enumerate().map(
                                                        |(row_index, matched)| {
                                                            let (name, description, builtin,
                                                                accept_text) = match matched {
                                                                SlashMatch::Builtin(
                                                                    name,
                                                                    description,
                                                                ) => (
                                                                    format!("/{name}"),
                                                                    description.to_string(),
                                                                    true,
                                                                    format!("/{name}"),
                                                                ),
                                                                SlashMatch::Template(index) => {
                                                                    let template =
                                                                        &self.templates
                                                                            [*index];
                                                                    let args =
                                                                        gertie_tools::templates::parse_invocation(
                                                                            &value,
                                                                        )
                                                                        .map(|(_, args)| args)
                                                                        .unwrap_or_default();
                                                                    (
                                                                        format!(
                                                                            "/{}",
                                                                            template.name
                                                                        ),
                                                                        template
                                                                            .description
                                                                            .clone(),
                                                                        false,
                                                                        template.render(&args),
                                                                    )
                                                                }
                                                            };
                                                            div()
                                                                .id(("slash-row", row_index))
                                                                .w_full()
                                                                .px_3()
                                                                .py_1()
                                                                .rounded_sm()
                                                                .cursor_pointer()
                                                                .hover(|style| {
                                                                    style.bg(
                                                                        cx.theme()
                                                                            .accent
                                                                            .opacity(0.14),
                                                                    )
                                                                })
                                                                .on_click(cx.listener(
                                                                    move |this, _, window, cx| {
                                                                        this.accept_slash_text(
                                                                            builtin,
                                                                            accept_text
                                                                                .clone(),
                                                                            window,
                                                                            cx,
                                                                        );
                                                                    },
                                                                ))
                                                                .child(
                                                                    h_flex()
                                                                        .gap_2()
                                                                        .items_center()
                                                                        .min_w_0()
                                                                        .child(
                                                                            div()
                                                                                .text_size(px(
                                                                                    13.
                                                                                ))
                                                                                .text_color(
                                                                                    cx.theme()
                                                                                        .foreground,
                                                                                )
                                                                                .child(name
                                                                                    .clone()),
                                                                        )
                                                                        .child(
                                                                            div()
                                                                                .text_size(px(
                                                                                    11.
                                                                                ))
                                                                                .text_color(
                                                                                    cx.theme()
                                                                                        .muted_foreground,
                                                                                )
                                                                                .truncate()
                                                                                .child(
                                                                                    description
                                                                                        .clone(),
                                                                                ),
                                                                        ),
                                                                )
                                                        },
                                                    )),
                                            )
                                        },
                                    )
                                    .when(!self.attachments.is_empty(), |composer| {
                                        // Pending images: one removable chip
                                        // per attachment, above the input.
                                        composer.child(
                                            h_flex().px_4().pt_2().gap_2().flex_wrap().children(
                                                self.attachments.iter().map(|attachment| {
                                                    let id = attachment.id;
                                                    let name = attachment.name.clone();
                                                    Button::new(format!("attach-{id}"))
                                                        .label(format!("🖼 {name} ✕"))
                                                        .small()
                                                        .ghost()
                                                        .tooltip("Remove attachment")
                                                        .on_click(cx.listener(
                                                            move |this, _, _, cx| {
                                                                this.remove_attachment(id, cx);
                                                            },
                                                        ))
                                                }),
                                            ),
                                        )
                                    })
                                    .child(
                                        h_flex()
                                            .px_4()
                                            .py_3()
                                            .gap_2()
                                            .child(
                                                Button::new("attach-image")
                                                    .label("🖼")
                                                    .small()
                                                    .ghost()
                                                    .tooltip("Attach the clipboard's image")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.attach_from_clipboard(cx);
                                                    })),
                                            )
                                            .child(div().flex_1().child(
                                                // Auto-grown by the state: one
                                                // row at rest, up to six with
                                                // content.
                                                Textarea::new(&self.input),
                                            )),
                                    )
                                    .child(self.render_token_footer(status, cx)),
                            ),
                    )
                    // The right column: agent runs or settings.
                    .when(self.panel == Panel::Agents, |row| {
                        row.child(self.render_agents_panel(cx))
                    })
                    .when(self.panel == Panel::Settings, |row| {
                        row.child(self.settings_panel.clone())
                    }),
            )
    }
}

const NO_ENDPOINT_NOTICE: &str = "No endpoint configured. Run `gertie doctor` to check the setup, or create \
    ~/.config/gertie/config.toml:\n\n\
    [[endpoints]]\n\
    name = \"ppq\"\n\
    base_url = \"https://api.ppq.ai/v1\"\n\
    model = \"glm-5.3-flash\"\n\
    api_key_env = \"PPQ_API_KEY\"\n\n\
    …set PPQ_API_KEY in your environment, then restart gertie.";

/// Stored text as display text: image markers become a quiet note —
/// the bytes live in the attachments dir, not the transcript.
fn display_text(text: &str) -> String {
    let (clean, images) = gertie_core::extract_image_markers(text);
    if images.is_empty() {
        clean
    } else {
        format!(
            "{clean}\n\n🖼 {} image{} attached",
            images.len(),
            if images.len() == 1 { "" } else { "s" }
        )
    }
}

/// One row of the slash popup: a builtin (executes on accept) or a file
/// template (expands into the composer).
enum SlashMatch {
    Builtin(&'static str, &'static str),
    Template(usize),
}

/// What the current composer text would complete to, for the popup and
/// for Enter routing. Builtins first — they always win over same-named
/// files — then templates, filtered by prefix on the typed name. Empty
/// = no popup: not an invocation, or nothing matches.
fn slash_matches(input: &str, templates: &[gertie_tools::templates::Template]) -> Vec<SlashMatch> {
    // The bare slash is the discovery moment: list everything.
    let parsed = if input == "/" {
        Some((String::new(), String::new()))
    } else {
        gertie_tools::templates::parse_invocation(input)
    };
    let Some((name, args)) = parsed else {
        return Vec::new();
    };
    let mut matches = Vec::new();
    if "compact".starts_with(name.as_str()) {
        matches.push(SlashMatch::Builtin(
            "compact",
            "Summarize older history now — frees context, leaves the record whole",
        ));
    }
    if "export".starts_with(name.as_str()) {
        matches.push(SlashMatch::Builtin(
            "export",
            "Write this conversation out as JSONL",
        ));
    }
    for (index, template) in templates.iter().enumerate() {
        if template.name.starts_with(name.as_str()) {
            matches.push(SlashMatch::Template(index));
        }
    }
    // With args present the name is already chosen — the popup narrows
    // to the exact row, as a confirm-and-expand.
    if !args.is_empty() {
        matches.retain(|matched| match matched {
            SlashMatch::Builtin(found, _) => *found == name,
            SlashMatch::Template(index) => templates[*index].name == name,
        });
    }
    matches
}

/// `1234` → "1.2k" — token counts stay scannable: one decimal below
/// 100k (`12.3k`), whole k above (`123k`).
fn human_tokens(count: u64) -> String {
    if count >= 100_000 {
        format!("{:.0}k", count as f64 / 1000.)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1000.)
    } else {
        format!("{count}")
    }
}
