//! The M2 chat: session sidebar + transcript + the agent loop, with tool
//! cards that gate mutating calls behind approve/deny.

use crate::bootstrap::Reloaded;
use crate::settings_edit::{SettingsEvent, SettingsPanelView};
use futures_util::StreamExt as _;
use gertie_core::{
    AfterState, LoopEvent, MessageId, PermissionBroker, PermissionDecision, PermissionMode, Role,
    SessionId, SessionStore, SharedHistory, SnapshotStore, ToolCall, ToolRegistry, TurnSpec,
    run_turn,
};
use gertie_providers::AnyProvider;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::command::{Command, CommandState};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, Focusable, IntoElement,
    KeyBinding, ListAlignment, ListState, ParentElement, Render, SharedString,
    StatefulInteractiveElement as _, Styled, Window, actions, div, list, prelude::*, px,
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

/// A pending attachment, held until send. Two kinds, split by what the
/// endpoints actually read: images and PDFs ride the multimodal blocks
/// as data URLs; text-likes ride inline in the message text, the one
/// shape every endpoint accepts. `id` keys the chip and its remove
/// button — a content or path hash, so re-attaching the same thing
/// replaces the chip instead of stacking it.
struct Attachment {
    id: u64,
    name: String,
    kind: AttachmentKind,
}

enum AttachmentKind {
    /// Binary bytes base64-wrapped as a data URL for the wire; a send
    /// also writes them to the attachments dir so a reload re-sends.
    DataUrl(String),
    /// UTF-8 content inlined into the message text at send. No storage
    /// marker — the stored transcript carries the text itself.
    Text(String),
    /// A paste too big for the message: held whole here, written into
    /// the workspace (.gertie/pastes/) at send, and cited by path —
    /// the agent reads the file with its own tools instead of the
    /// context carrying the body.
    Paste { name: String, content: String },
}

/// At this size a paste stops being a quote and starts being a
/// document; it rides as a file, not as message text (t3code's rule).
const PASTE_ATTACHMENT_BYTES: usize = 32 * 1024;

/// How many most-recent rows a transcript loads on open and on
/// "load earlier": opening a chat touches one page of the store, not
/// the whole archive. Old sessions grow without bound; the view must
/// not (performance doctrine: bounded loads).
const RECENT_WINDOW: usize = 400;

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
        /// The persisted row for this tool's result, once it exists —
        /// how a palette hit lands on the right card.
        db_id: Option<MessageId>,
    },
    /// Synthetic pagination row at the transcript's top: the store
    /// holds rows older than the loaded window. Clicking pages in the
    /// next older batch; it is never a turn the loop sees.
    LoadEarlier,
}

actions!(gertie, [CloseTurnDiff, UndoDeleteSession]);

/// The chat surface's key map, bound once at app start: escape closes
/// the turn-diff panel while focus sits inside it, and Ctrl+Z outside
/// the composer undoes a sidebar delete inside its window (inside the
/// composer it stays text undo — the input consumes the keystroke).
pub fn bind_chat_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", CloseTurnDiff, Some("TurnDiffPanel")),
        KeyBinding::new("ctrl-z", UndoDeleteSession, None::<&str>),
    ]);
}

/// One file's change in the turn-diff panel, resolved at open time —
/// the panel never re-reads the ledger while it shows.
struct TurnFileView {
    path: std::path::PathBuf,
    /// The checkpoint to restore this path's pre-turn content from.
    checkpoint: gertie_core::CheckpointId,
    diff: FileDiff,
}

/// Text files diff; binary files refuse — a lossy text rendering would
/// lie about what changed.
enum FileDiff {
    Text(crate::diff::Patch),
    Binary { before: usize, after: usize },
}

/// The open turn-diff modal: this turn's files, and a focus handle so
/// escape closes it.
struct TurnDiffPanel {
    files: Vec<TurnFileView>,
    focus: gpui_kit::FocusHandle,
}

/// One row of the panel's list, as data — `AnyElement` isn't `Clone`,
/// so (like the transcript) rows re-render from data on every paint.
enum TurnDiffRow {
    /// A file's header: path, ± summary, expand toggle.
    Header(usize),
    /// A diff line, colored by its kind.
    Diff(crate::diff::Line),
    /// A muted note: unchanged, omitted count, binary refusal.
    Note(String),
}

/// A session inside its undo window: ✕ clicked, store row still there,
/// purge task scheduled. The vec membership is the "not yet purged"
/// marker — undo removes the entry, the purge finds nothing to do.
/// The snooze delay: how far a limited turn's resume is put off. Not
/// parsed from the provider's reported reset time (Retry-After and
/// friends differ per wire) — a fixed half hour is the boring middle,
/// and a fired-but-still-limited turn just ends limited again for a
/// human decision.
const SNOOZE_MINUTES: u64 = 30;

struct PendingDelete {
    session_id: SessionId,
    /// The title at delete time — the toast speaks in past tense about
    /// a row that no longer exists to ask.
    title: String,
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
    /// builtins /compact, /export, /new, /settings and /agents are
    /// methods, not files.
    templates: Vec<gertie_tools::templates::Template>,
    /// The folder the agent's tools run in — shown in the footer so
    /// "which project am I talking about" is always answered on screen.
    workspace_root: std::path::PathBuf,
    /// The reclaim command a manual settle runs in the settling
    /// session's recorded workspace (config: settle_command).
    settle_command: Option<String>,
    /// The command palette (Ctrl+K): the kit's field + keyboard nav,
    /// pointed at the store's FTS index by the callbacks in render.
    palette: Entity<CommandState>,
    /// Whether the palette is on screen.
    palette_open: bool,
    /// The hits behind the palette's rows, in search-rank order — the
    /// row index a confirm hands back indexes this vector.
    palette_hits: Vec<gertie_core::HistoryHit>,
    /// The turn-diff modal, when open — raised from a tool card's
    /// "turn diff" button, dismissed by escape, backdrop, or restore.
    turn_diff: Option<TurnDiffPanel>,
    /// Which file's diff is expanded in the panel, one at a time.
    turn_diff_open: Option<usize>,
    /// The panel's scroll list, reset on each open.
    turn_diff_list: ListState,
    /// Shared approval registry: leader and worker asks, one answer path.
    broker: Arc<PermissionBroker>,
    /// The front's current session, read by subagent dispatch to link
    /// worker runs to the chat that dispatched them.
    session_scope: gertie_core::SessionScope,
    /// Worker dispatch reads the turn's posture here; the chat sets it
    /// at every turn start, never mid-turn.
    mode_scope: gertie_core::ModeScope,
    /// Whether bash runs inside the Landlock sandbox (config) — the
    /// auto mode's bash approval keys off it.
    sandbox_bash: bool,
    /// This session's permission mode — the chip's state and the next
    /// turn's posture. Follows the open session; a fresh chat starts
    /// at the configured default.
    mode: PermissionMode,
    /// The mode new sessions open in ([permissions] default_mode).
    default_mode: PermissionMode,
    input: Entity<TextareaState>,
    /// Session-rename flow: which sidebar row is being renamed, and the
    /// field editing its title (Enter commits; clicking away cancels).
    renaming: Option<SessionId>,
    rename_input: Entity<InputState>,
    /// Reply inputs for pending permission cards, keyed by turn index —
    /// the deny-with-feedback path: the human's text rides back to the
    /// model in the tool result. Created on first render (that's where
    /// a window handle exists), dropped when the card is answered.
    approval_replies: std::collections::HashMap<usize, Entity<InputState>>,
    /// Deleted-but-restorable sessions: off every list the moment ✕ is
    /// clicked, still in the store behind the undo toast at the
    /// sidebar's foot. The purge task drops the row when the window
    /// closes; undo pulls the entry back out first.
    pending_deletes: Vec<PendingDelete>,
    /// The snooze promise: this open session's limited turn will be
    /// resumed by itself, thirty minutes out. One shot — cleared by the
    /// fire, a manual resume, a fresh send, or a fresh limit; the
    /// pending timer no-ops whenever the field no longer agrees.
    snoozed: Option<SessionId>,
    /// Whether the sidebar's settled fold is expanded. Collapsed by
    /// default — settled work is put down, not gone; one click brings
    /// it back.
    settled_open: bool,
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
        // The folder the agent's tools run in, shown in the footer.
        workspace_root: std::path::PathBuf,
        // The reclaim command a manual settle runs in the settling
        // session's recorded workspace (config: settle_command).
        settle_command: Option<String>,
        broker: Arc<PermissionBroker>,
        session_scope: gertie_core::SessionScope,
        // Worker dispatch reads this turn's posture; the chat sets it
        // at every turn start.
        mode_scope: gertie_core::ModeScope,
        // Whether bash runs inside the Landlock sandbox (config) —
        // the auto mode's bash approval keys off it.
        sandbox_bash: bool,
        // The mode new sessions open in ([permissions] default_mode).
        default_mode: gertie_core::PermissionMode,
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
        // The palette is a component entity like the input: created
        // once, parked until Ctrl+K puts it on screen.
        let palette = cx.new(|cx| CommandState::new(window, cx));
        let turn_diff_list = ListState::new(0, ListAlignment::Top, px(256.));
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
        // Resume the newest unsettled session if there is one; settled
        // work stays folded (opening it is the un-settle door, and a
        // cold open shouldn't shove put-down work back in your face).
        // Otherwise the first message creates the session (lazy, so no
        // empty rows accumulate).
        let session_id = if resume {
            sessions
                .iter()
                .find(|summary| !summary.settled)
                .map(|summary| summary.id)
        } else {
            None
        };
        session_scope.set(session_id);
        // The chip follows the session: a resumed conversation comes
        // back with the posture it ran under; a blank composer starts
        // at the configured default.
        let mode = session_id
            .and_then(|id| sessions.iter().find(|summary| summary.id == id))
            .map(|summary| summary.mode)
            .unwrap_or(default_mode);

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
            workspace_root,
            settle_command,
            palette,
            palette_open: false,
            palette_hits: Vec::new(),
            turn_diff: None,
            turn_diff_open: None,
            turn_diff_list,
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
            mode_scope,
            sandbox_bash,
            mode,
            default_mode,
            input,
            renaming: None,
            rename_input,
            pending_deletes: Vec::new(),
            snoozed: None,
            settled_open: false,
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
            approval_replies: Default::default(),
        };
        view.reload_session(cx);
        window.focus(&view.input.read(cx).focus_handle(cx), cx);
        view
    }

    /// The session the transcript is bound to, for tests and tooling.
    pub fn session_id(&self) -> Option<SessionId> {
        self.session_id
    }

    /// Runs the big-paste rule directly (the real paste hook needs a
    /// clipboard item delivered by the textarea). Returns whether the
    /// paste was consumed as a file chip.
    pub fn attach_big_paste_for_test(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        self.attach_big_paste(text, cx)
    }

    /// How many turns the transcript holds, for tests and tooling.
    pub fn turn_count_for_test(&self) -> usize {
        self.turns.len()
    }

    /// The text of the transcript's notice rows, for tests.
    pub fn notice_texts_for_test(&self) -> Vec<String> {
        self.turns
            .iter()
            .filter_map(|turn| match turn {
                Turn::Message {
                    role: Role::System,
                    text,
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Whether the palette is on screen, for tests.
    pub fn palette_open_for_test(&self) -> bool {
        self.palette_open
    }

    /// The sidebar's settled sessions, for tests — the ids that folded
    /// out of the active list.
    pub fn settled_ids_for_test(&self) -> Vec<SessionId> {
        self.sessions
            .iter()
            .filter(|summary| summary.settled)
            .map(|summary| summary.id)
            .collect()
    }

    /// How many hits the palette is showing, for tests.
    pub fn palette_hits_for_test(&self) -> usize {
        self.palette_hits.len()
    }

    /// Where the transcript's virtual list is scrolled, for tests —
    /// the first visible turn index.
    pub fn scroll_top_for_test(&self) -> usize {
        self.list_state.logical_scroll_top().item_ix
    }

    /// Scrolls the transcript to an absolute row, for tests — the
    /// programmatic stand-in for the user scrolling up (tail-follow
    /// pauses the same way).
    pub fn scroll_to_item_for_test(&self, ix: usize) {
        self.list_state.pause_following_tail();
        self.list_state.scroll_to(gpui_kit::ListOffset {
            item_ix: ix,
            offset_in_item: px(0.),
        });
    }

    /// The turn-diff panel's file count — `None` when closed, `Some(n)`
    /// when open with n files listed, for tests.
    pub fn turn_diff_for_test(&self) -> Option<usize> {
        self.turn_diff.as_ref().map(|panel| panel.files.len())
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

    /// Whether a turn is alive in this chat, for the tab strip — a tab
    /// working in the background reads as busy even when its window is
    /// elsewhere.
    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    /// Swaps the transcript for a session's stored messages and rebuilds
    /// the canonical history the loop will extend. The transcript takes
    /// a bounded window (the newest `RECENT_WINDOW` rows); the wire
    /// history takes everything — the loop resumes with exactly what
    /// the model saw before.
    fn reload_session(&mut self, cx: &mut Context<Self>) {
        let (turns, history, last_db_id) = match self.session_id {
            Some(session_id) => {
                // One page of rows renders the transcript, however deep
                // the archive grows. `limit + 1` detects a truncation
                // without a COUNT query.
                let mut rows = self
                    .store
                    .load_recent_messages(session_id, RECENT_WINDOW + 1)
                    .unwrap_or_default();
                let older_rows = rows.len() > RECENT_WINDOW;
                if older_rows {
                    rows.remove(0);
                }
                let mut turns: Vec<Turn> = rows.iter().map(turn_from_row).collect();
                if older_rows {
                    turns.insert(0, Turn::LoadEarlier);
                }
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
        self.approval_replies.clear();
        self.sync_list(cx);
    }

    fn sync_list(&mut self, cx: &mut Context<Self>) {
        self.list_state.reset(self.item_count());
        // Jump to the end AND re-engage tail-following (a session switch
        // or a fresh chat starts following again).
        self.list_state.scroll_to_end();
        cx.notify();
    }

    /// "Load earlier": page the next older batch of rows into the
    /// transcript. The prepend shifts every turn index, so index-keyed
    /// state (expanded cards, reply inputs) dies here, and the click is
    /// refused mid-stream — a live turn is still appending and would
    /// race the renumbering.
    fn load_earlier(&mut self, cx: &mut Context<Self>) {
        if self.streaming {
            return;
        }
        let Some(session_id) = self.session_id else {
            return;
        };
        // The anchor is the loaded window's oldest persisted row.
        let Some(oldest) = self.turns.iter().find_map(|turn| match turn {
            Turn::Message {
                db_id: Some(id), ..
            }
            | Turn::Tool {
                db_id: Some(id), ..
            } => Some(*id),
            _ => None,
        }) else {
            return;
        };
        // `limit + 1` detects a truncation without a COUNT query.
        let mut rows = self
            .store
            .load_messages_before(session_id, oldest, RECENT_WINDOW + 1)
            .unwrap_or_default();
        let more = rows.len() > RECENT_WINDOW;
        if more {
            rows.remove(0);
        }
        if rows.is_empty() {
            // The row that got us here was stale — retire it.
            let had_row = matches!(self.turns.first(), Some(Turn::LoadEarlier));
            if had_row {
                self.turns.remove(0);
                self.list_state.splice(0..1, 0);
            }
            cx.notify();
            return;
        }
        // Reading history pauses tail-following — the next append must
        // not yank the view to the bottom. (A user who scrolled up here
        // already paused it; this is for the fresh-session edge.)
        self.list_state.pause_following_tail();
        let mut old = std::mem::take(&mut self.turns);
        old.remove(0); // the clicked row retires; a fresh one leads when more remain
        let mut page: Vec<Turn> = rows.iter().map(turn_from_row).collect();
        if more {
            page.insert(0, Turn::LoadEarlier);
        }
        page.extend(old);
        let added = page.len();
        self.turns = page;
        // The clicked row's slot grew into the new page: one splice
        // keeps the list's height cache and scroll position in step —
        // the reader holds their place, no snap.
        self.list_state.splice(0..1, added);
        self.expanded_tools.clear();
        self.approval_replies.clear();
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

    /// The composer chip's click: the mode advances around the ring —
    /// supervised → accept edits → auto → full access — and persists
    /// with the session. It applies from the next turn on; a running
    /// turn keeps the posture it was built with, and the notice says
    /// so when it matters.
    fn cycle_mode(&mut self, cx: &mut Context<Self>) {
        let next = match self.mode {
            PermissionMode::Supervised => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::Auto,
            PermissionMode::Auto => PermissionMode::FullAccess,
            PermissionMode::FullAccess => PermissionMode::Supervised,
        };
        self.set_mode(next, cx);
    }

    fn set_mode(&mut self, mode: PermissionMode, cx: &mut Context<Self>) {
        self.mode = mode;
        if let Some(session_id) = self.session_id {
            let _ = self.store.set_session_mode(session_id, mode);
            self.sessions = self.store.list_sessions().unwrap_or_default();
        }
        if self.streaming {
            self.push_notice("Permission mode applies when this turn ends.", cx);
        }
        cx.notify();
    }

    fn new_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.streaming {
            return;
        }
        // Sessions are created lazily by the first message; "New chat" just
        // blanks the transcript until then.
        self.session_id = None;
        self.session_scope.set(None);
        // A fresh chat starts at the configured posture again.
        self.mode = self.default_mode;
        self.usage = (0, 0);
        self.context_used = 0;
        self.refresh_agent_runs(cx);
        self.turns.clear();
        self.approval_replies.clear();
        self.last_db_id = None;
        self.streaming_assistant = None;
        self.history.blocking_lock().clear();
        self.close_turn_diff(cx);
        self.sync_list(cx);
        window.focus(&self.input.read(cx).focus_handle(cx), cx);
    }

    fn switch_to_session(
        &mut self,
        session_id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.switch_session_state(session_id, cx);
        window.focus(&self.input.read(cx).focus_handle(cx), cx);
    }

    /// The window-free body of a session switch — the palette's confirm
    /// callback runs with an App handle, not a window, and jumps here.
    fn switch_session_state(&mut self, session_id: SessionId, cx: &mut Context<Self>) {
        if self.streaming {
            return;
        }
        // Pending attachments are composer state, not session state —
        // they don't hop conversations. Neither does the turn-diff
        // panel: it answers for the session it was opened in.
        self.attachments.clear();
        self.close_turn_diff(cx);
        self.session_id = Some(session_id);
        self.session_scope.set(Some(session_id));
        let summary = self
            .sessions
            .iter()
            .find(|summary| summary.id == session_id)
            .cloned();
        // Opening a settled conversation is activity: it comes back to
        // the active list. The one un-settle door besides its own ↩.
        if summary.as_ref().is_some_and(|summary| summary.settled) {
            let _ = self.store.set_session_settled(session_id, false);
            self.sessions = self.store.list_sessions().unwrap_or_default();
        }
        self.usage = summary
            .as_ref()
            .map(|summary| (summary.input_tokens, summary.output_tokens))
            .unwrap_or((0, 0));
        // The chip follows the session: its stored posture comes back
        // with the rest of it.
        self.mode = summary
            .as_ref()
            .map(|summary| summary.mode)
            .unwrap_or(self.default_mode);
        self.context_used = summary.map(|summary| summary.context_tokens).unwrap_or(0);
        self.refresh_agent_runs(cx);
        self.reload_session(cx);
    }

    /// Ctrl+K (or the sidebar's search button): open the palette over
    /// the store's memory. A fresh query — last time's hits are last
    /// time's; and focus the field so typing starts immediately.
    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = true;
        self.palette_hits.clear();
        self.palette.update(cx, |palette, cx| {
            palette.set_query("", window, cx);
        });
        let handle = Focusable::focus_handle(self.palette.read(cx), cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn close_palette(&mut self, cx: &mut Context<Self>) {
        if !self.palette_open {
            return;
        }
        self.palette_open = false;
        cx.notify();
    }

    /// The palette's query callback: the store's FTS index answers,
    /// bounded — twenty hits is a page a human can actually scan.
    /// Sessions inside their undo window never show up here.
    fn palette_search(&mut self, query: &str, cx: &mut Context<Self>) {
        self.palette_hits.clear();
        if !query.trim().is_empty() {
            self.palette_hits = self
                .store
                .search(query, 20)
                .unwrap_or_default()
                .into_iter()
                .filter(|hit| {
                    !self
                        .pending_deletes
                        .iter()
                        .any(|pending| pending.session_id == hit.session_id)
                })
                .collect();
        }
        cx.notify();
    }

    /// Confirming a hit: land on that turn. A hit from another session
    /// switches first; either way the matching row scrolls into view.
    fn palette_jump(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(hit) = self.palette_hits.get(row) else {
            self.close_palette(cx);
            return;
        };
        let session_id = hit.session_id;
        let message_id = hit.message_id;
        if self.session_id != Some(session_id) {
            self.switch_session_state(session_id, cx);
        }
        let turn_index = self.turns.iter().position(|turn| match turn {
            Turn::Message {
                db_id: Some(db_id), ..
            }
            | Turn::Tool {
                db_id: Some(db_id), ..
            } => *db_id == message_id,
            _ => false,
        });
        if let Some(index) = turn_index {
            // The jump is navigation: drop tail-follow first, or the
            // next layout snaps back to the end and the reveal never
            // shows. Scrolling back to the bottom re-engages it.
            self.list_state.pause_following_tail();
            self.list_state.scroll_to_reveal_item(index);
        }
        self.close_palette(cx);
    }

    /// "turn diff" on a done tool card: anchor to the card's turn —
    /// the latest user message at or before it — and open the ledger's
    /// answer. Works from any card in the turn; the diff is the turn's,
    /// not the call's.
    fn open_turn_diff(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let end = (index + 1).min(self.turns.len());
        let anchor = self.turns[..end].iter().rev().find_map(|turn| match turn {
            Turn::Message {
                role: Role::User,
                db_id: Some(db_id),
                ..
            } => Some(*db_id),
            _ => None,
        });
        let Some(anchor) = anchor else {
            self.push_notice("This turn has no user message to anchor a diff to.", cx);
            return;
        };
        self.open_turn_diff_for_message(anchor, cx);
        if let Some(focus) = self.turn_diff.as_ref().map(|panel| panel.focus.clone()) {
            window.focus(&focus, cx);
        }
    }

    /// The panel body, window-free — tests call this directly. The
    /// ledger resolves the turn from the user message row, and every
    /// file's before/after is read once, here.
    pub fn open_turn_diff_for_message(&mut self, message_id: MessageId, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id else {
            return;
        };
        let Some(snapshots) = &self.snapshots else {
            return;
        };
        let Some(turn) = snapshots
            .turn_for_message(session_id, message_id)
            .ok()
            .flatten()
        else {
            self.push_notice(
                "No checkpoint ledger for this turn — it predates turn diffs.",
                cx,
            );
            return;
        };
        let changes = snapshots.turn_changes(&turn).unwrap_or_default();
        let files = changes
            .into_iter()
            .map(|change| {
                let before = snapshots
                    .read_object(&change.before_hash)
                    .unwrap_or_default();
                let after = match &change.after {
                    AfterState::Object(hash) => snapshots.read_object(hash).unwrap_or_default(),
                    // Never touched again: disk is the answer, resolved
                    // against the workspace like every tool path.
                    AfterState::Disk => {
                        std::fs::read(resolve_tool_path(&self.workspace_root, &change.path))
                            .unwrap_or_default()
                    }
                };
                let diff = match (std::str::from_utf8(&before), std::str::from_utf8(&after)) {
                    (Ok(before), Ok(after)) => FileDiff::Text(crate::diff::lines(before, after)),
                    _ => FileDiff::Binary {
                        before: before.len(),
                        after: after.len(),
                    },
                };
                TurnFileView {
                    path: change.path,
                    checkpoint: change.checkpoint,
                    diff,
                }
            })
            .collect();
        // One modal at a time: the panel replaces the palette.
        self.palette_open = false;
        self.turn_diff_open = None;
        self.turn_diff_list.reset(0);
        let focus = cx.focus_handle();
        self.turn_diff = Some(TurnDiffPanel { files, focus });
        cx.notify();
    }

    fn close_turn_diff(&mut self, cx: &mut Context<Self>) {
        if self.turn_diff.take().is_some() {
            self.turn_diff_open = None;
            cx.notify();
        }
    }

    /// Restore: every touched path back to its pre-turn content — the
    /// turn's own undo. Bounded local writes; gated on a quiet turn
    /// because files changing under a running loop is chaos.
    fn restore_turn(&mut self, cx: &mut Context<Self>) {
        if self.streaming {
            self.push_notice("A run is in flight — restoring waits for it.", cx);
            return;
        }
        let Some(panel) = self.turn_diff.take() else {
            return;
        };
        self.turn_diff_open = None;
        let mut restored = 0;
        if let Some(snapshots) = &self.snapshots {
            for file in &panel.files {
                if snapshots
                    .restore_under(file.checkpoint, &self.workspace_root)
                    .is_ok()
                {
                    restored += 1;
                }
            }
        }
        cx.notify();
        self.push_notice(
            format!(
                "Restored {restored} of {} path(s) to their pre-turn state.",
                panel.files.len()
            ),
            cx,
        );
    }

    /// The turn-diff modal: what this turn changed in the workspace.
    /// The ledger answered at open; the panel only shows.
    fn render_turn_diff(
        &self,
        panel: &TurnDiffPanel,
        view: Entity<Self>,
        cx: &Context<Self>,
    ) -> AnyElement {
        // The rows as data; the list's closure paints them through the
        // view (elements are rebuilt per paint — the transcript works
        // the same way).
        let mut rows: Vec<TurnDiffRow> = Vec::new();
        for (index, file) in panel.files.iter().enumerate() {
            rows.push(TurnDiffRow::Header(index));
            if self.turn_diff_open == Some(index) {
                match &file.diff {
                    FileDiff::Text(patch) if patch.adds + patch.removes == 0 => {
                        rows.push(TurnDiffRow::Note("unchanged".into()));
                    }
                    FileDiff::Text(patch) => {
                        for line in &patch.lines {
                            rows.push(TurnDiffRow::Diff(line.clone()));
                        }
                        if patch.omitted > 0 {
                            rows.push(TurnDiffRow::Note(format!("… {} more lines", patch.omitted)));
                        }
                    }
                    FileDiff::Binary { .. } => {
                        rows.push(TurnDiffRow::Note("binary file — contents not shown".into()))
                    }
                }
            }
        }
        self.turn_diff_list.reset(rows.len());
        let paint = std::rc::Rc::new(rows);
        div()
            .absolute()
            .inset_0()
            .bg(cx.theme().overlay)
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close_turn_diff(cx)),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top_12()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .w(px(720.))
                            .key_context("TurnDiffPanel")
                            .track_focus(&panel.focus)
                            .on_mouse_down(
                                gpui_kit::MouseButton::Left,
                                |_, _, cx| cx.stop_propagation(),
                            )
                            .bg(cx.theme().background)
                            .border_1()
                            .border_color(cx.theme().border)
                            .rounded_lg()
                            .child(
                                v_flex()
                                    .child(
                                        h_flex()
                                            .px_4()
                                            .py_3()
                                            .gap_2()
                                            .items_center()
                                            .justify_between()
                                            .child(
                                                div()
                                                    .text_size(px(14.))
                                                    .child("Turn changes"),
                                            )
                                            .child(
                                                Button::new("close-turn-diff")
                                                    .label("close")
                                                    .xsmall()
                                                    .ghost()
                                                    .on_click(cx.listener(
                                                        |this, _, _, cx| {
                                                            this.close_turn_diff(cx)
                                                        },
                                                    )),
                                            ),
                                    )
                                    .when(paint.is_empty(), |body| {
                                        body.child(
                                            div()
                                                .px_4()
                                                .py_3()
                                                .text_size(px(12.))
                                                .text_color(cx.theme().muted_foreground)
                                                .child(
                                                    "No workspace changes recorded this turn.",
                                                ),
                                        )
                                    })
                                    .when(!paint.is_empty(), |body| {
                                        body.child(
                                            div()
                                                .h(px(420.))
                                                .child(list(
                                                    self.turn_diff_list.clone(),
                                                    move |index, _, cx| {
                                                        let rows = paint.clone();
                                                        view.update(cx, |view, cx| {
                                                            view.paint_diff_row(&rows[index], cx)
                                                        })
                                                    },
                                                )),
                                        )
                                        .child(
                                            h_flex()
                                                .px_4()
                                                .py_3()
                                                .gap_2()
                                                .justify_between()
                                                .border_t_1()
                                                .border_color(cx.theme().border)
                                                .child(
                                                    div()
                                                        .text_size(px(11.))
                                                        .text_color(
                                                            cx.theme().muted_foreground,
                                                        )
                                                        .child(
                                                            "Git's own churn (pulls, merges) never appears here — the diff is what the tools did.",
                                                        ),
                                                )
                                                .child(
                                                    Button::new("restore-turn")
                                                        .label("restore turn")
                                                        .xsmall()
                                                        .outline()
                                                        .on_click(cx.listener(
                                                            |this, _, _, cx| {
                                                                this.restore_turn(cx)
                                                            },
                                                        )),
                                                ),
                                        )
                                    }),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Paints one panel row from its data. Runs per paint, per row —
    /// nothing here touches the ledger.
    fn paint_diff_row(&self, row: &TurnDiffRow, cx: &mut Context<Self>) -> AnyElement {
        match row {
            TurnDiffRow::Note(text) => div()
                .px_4()
                .pb_2()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child(text.clone())
                .into_any_element(),
            TurnDiffRow::Diff(line) => {
                let (text, color) = match line {
                    crate::diff::Line::Same(text) => {
                        (format!("  {text}"), cx.theme().muted_foreground)
                    }
                    crate::diff::Line::Add(text) => (format!("+ {text}"), cx.theme().success),
                    crate::diff::Line::Remove(text) => (format!("− {text}"), cx.theme().danger),
                };
                div()
                    .px_4()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(px(11.))
                    .text_color(color)
                    .child(text)
                    .into_any_element()
            }
            TurnDiffRow::Header(index) => {
                let Some(panel) = self.turn_diff.as_ref() else {
                    return div().into_any_element();
                };
                let file = &panel.files[*index];
                let summary = match &file.diff {
                    FileDiff::Text(patch) => format!("+{} −{}", patch.adds, patch.removes),
                    FileDiff::Binary { before, after } => {
                        format!("{before} → {after} bytes")
                    }
                };
                let index = *index;
                h_flex()
                    .px_4()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(13.))
                            .max_w(px(480.))
                            .truncate()
                            .child(file.path.display().to_string()),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(summary),
                            )
                            .child(
                                Button::new(SharedString::from(format!("diff-{index}")))
                                    .label(if self.turn_diff_open == Some(index) {
                                        "hide"
                                    } else {
                                        "diff"
                                    })
                                    .xsmall()
                                    .ghost()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.turn_diff_open = if this.turn_diff_open == Some(index)
                                        {
                                            None
                                        } else {
                                            Some(index)
                                        };
                                        cx.notify();
                                    })),
                            ),
                    )
                    .into_any_element()
            }
        }
    }

    /// The agents panel toggle: opens/closes and refreshes the run list.
    fn toggle_agents(&mut self, cx: &mut Context<Self>) {
        self.panel = if self.panel == Panel::Agents {
            Panel::None
        } else {
            Panel::Agents
        };
        self.refresh_agent_runs(cx);
    }

    /// The settings panel toggle. One column, one panel: opening one
    /// closes the other.
    fn toggle_settings(&mut self, cx: &mut Context<Self>) {
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

    /// The ✕ on a sidebar row: the session leaves every list right now,
    /// but the store row survives a five-second undo window — the toast
    /// at the sidebar's foot is the speed bump the "sure?" step used to
    /// be. Undo puts it back; the window closing purges it for real.
    /// Deleting the open session returns to the blank transcript.
    fn delete_clicked(
        &mut self,
        session_id: SessionId,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A streaming transcript can't be torn down; also no toast for
        // a row that's already inside its undo window.
        if self.streaming && self.session_id == Some(session_id) {
            return;
        }
        let Some(summary) = self.sessions.iter().find(|s| s.id == session_id) else {
            return;
        };
        self.pending_deletes.push(PendingDelete {
            session_id,
            title: summary.title.clone(),
        });
        self.renaming = None;
        if self.session_id == Some(session_id) {
            // The human asked for this transcript gone: blank it now,
            // not after the purge — the view never contradicts ✕.
            self.session_id = None;
            self.turns.clear();
            self.approval_replies.clear();
            self.last_db_id = None;
            self.streaming_assistant = None;
            self.history.blocking_lock().clear();
        }
        self.refresh_agent_runs(cx);
        self.sync_list(cx);
        cx.notify();

        // The window itself: a task holding the deadline. Undo removes
        // the entry first, so this purge finds nothing and does less.
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(5)).await;
            let _ = this.update(cx, |this, cx| this.purge_expired_delete(session_id, cx));
        })
        .detach();
    }

    /// The undo window closed on `session_id`: really delete it. If the
    /// human undid first, the entry is gone and this is a no-op.
    fn purge_expired_delete(&mut self, session_id: SessionId, cx: &mut Context<Self>) {
        if !self
            .pending_deletes
            .iter()
            .any(|pending| pending.session_id == session_id)
        {
            return;
        }
        self.pending_deletes
            .retain(|pending| pending.session_id != session_id);
        let _ = self.store.delete_session(session_id);
        self.sessions = self.store.list_sessions().unwrap_or_default();
        self.refresh_agent_runs(cx);
        cx.notify();
    }

    /// Undo on the toast (button or Ctrl+Z outside the composer): pop
    /// the most recent pending delete — the row returns to its lists
    /// untouched, the store row was never gone.
    fn undo_delete_clicked(&mut self, cx: &mut Context<Self>) {
        if self.pending_deletes.pop().is_some() {
            self.sessions = self.store.list_sessions().unwrap_or_default();
            self.refresh_agent_runs(cx);
            cx.notify();
        }
    }

    /// The ✓ on a sidebar row: settle — the human put the work down.
    /// Out of the active list, never out of the record; opening it
    /// again un-settles. Reversible on the row itself (↩).
    fn settle_clicked(&mut self, session_id: SessionId, settled: bool, cx: &mut Context<Self>) {
        // A live turn is neither done nor abandoned; settling it would
        // lie about the state.
        if settled && self.streaming && self.session_id == Some(session_id) {
            return;
        }
        if self.store.set_session_settled(session_id, settled).is_ok() {
            self.sessions = self.store.list_sessions().unwrap_or_default();
        }
        if settled {
            self.run_reclaim(session_id, cx);
        }
        cx.notify();
    }

    /// The settle_command (config), run in the settling session's
    /// recorded workspace — the reclaim hook (`cargo clean` in a
    /// worktree checkout, say). The command never blocks the UI and
    /// never runs from the auto-settle sweep: a human pressed ✓ here.
    /// A session with no recorded root (older than the column) says so
    /// instead of guessing a folder.
    fn run_reclaim(&mut self, session_id: SessionId, cx: &mut Context<Self>) {
        let Some(command) = self.settle_command.clone() else {
            return;
        };
        let Some(root) = self.store.session_workspace(session_id) else {
            self.push_notice(
                "no workspace recorded for this session — settle command skipped",
                cx,
            );
            return;
        };
        cx.spawn(async move |this, cx| {
            let notice_command = command.clone();
            let output = cx
                .background_executor()
                .spawn(async move {
                    std::process::Command::new("sh")
                        .arg("-c")
                        .arg(&command)
                        .current_dir(&root)
                        .output()
                })
                .await;
            let _ = this.update(cx, |this, cx| match output {
                Ok(output) if output.status.success() => {
                    this.push_notice(format!("settle: `{notice_command}` — done"), cx);
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let tail = stderr.lines().next_back().unwrap_or("no output");
                    this.push_notice(format!("settle: `{notice_command}` failed — {tail}"), cx);
                }
                Err(error) => {
                    this.push_notice(format!("settle: `{notice_command}` failed — {error}"), cx)
                }
            });
        })
        .detach();
    }

    /// The attach affordance, three ways in, one destination:
    /// the "attach file" button opens a native file picker; pasting
    /// intercepts images and copied files before the textarea sees
    /// them; both land in the same pending set as removable chips.
    fn attach_from_picker(&mut self, cx: &mut Context<Self>) {
        let picker = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach files".into()),
        });
        cx.spawn(async move |this, cx| {
            let paths = match picker.await {
                Ok(Ok(Some(paths))) => paths,
                _ => return, // cancelled, or the portal is missing
            };
            let _ = this.update(cx, |this, cx| {
                for path in paths {
                    this.attach_file(path, cx);
                }
            });
        })
        .detach();
    }

    /// The paste hook body: image and file payloads become chips and
    /// the paste is consumed; anything else falls through to the
    /// textarea's normal text insertion.
    fn attach_paste(&mut self, item: &gpui_kit::ClipboardItem, cx: &mut Context<Self>) -> bool {
        use base64::Engine as _;
        let mut attached = false;
        for entry in &item.entries {
            match entry {
                gpui_kit::ClipboardEntry::Image(image) => {
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
                    self.attach_chip(
                        image.id,
                        format!("image.{extension}"),
                        AttachmentKind::DataUrl(data_url),
                        cx,
                    );
                    attached = true;
                }
                gpui_kit::ClipboardEntry::ExternalPaths(paths) => {
                    for path in &paths.0 {
                        self.attach_file(path.clone(), cx);
                        attached = true;
                    }
                }
                // Text entries paste as text, as always — unless the
                // paste is bigger than a message should be, in which
                // case it becomes a file chip and the paste is consumed.
                gpui_kit::ClipboardEntry::String(text) => {
                    attached |= self.attach_big_paste(&text.text, cx);
                }
            }
        }
        attached
    }

    /// A text paste at or above the threshold becomes a file chip
    /// instead of message text: at send the content is filed into the
    /// workspace and the message cites the path, so neither the model
    /// context nor the transcript carries the body. Smaller pastes
    /// fall through as text (returns false). The chip's id is the
    /// content hash, so re-pasting the same text replaces the chip.
    fn attach_big_paste(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        if text.len() < PASTE_ATTACHMENT_BYTES {
            return false;
        }
        let id = {
            use std::hash::{Hash, Hasher as _};
            let mut hasher = std::hash::DefaultHasher::new();
            text.hash(&mut hasher);
            hasher.finish()
        };
        let name = format!("paste-{id:016x}.txt");
        let kb = text.len() / 1024;
        self.attach_chip(
            id,
            format!("{name} · {kb} KB"),
            AttachmentKind::Paste {
                name: name.clone(),
                content: text.to_string(),
            },
            cx,
        );
        self.push_notice(
            format!("Big paste ({kb} KB) filed as {name} — it rides as a path the agent reads with read_file, not as message text."),
            cx,
        );
        true
    }

    /// One file from the picker or a copied path: read it, classify it
    /// by extension, park it as a chip. Refusals speak — an unsupported
    /// type or an oversized file explains itself instead of silently
    /// vanishing on the wire later.
    fn attach_file(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        use base64::Engine as _;
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            return;
        };
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("");
        let Some(media_type) = gertie_core::media_type_for_extension(extension) else {
            self.push_notice(
                format!(
                    "Can't attach {name} — models read images, PDFs, and text files; \
                     .{extension} has no wire shape."
                ),
                cx,
            );
            return;
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.push_notice(format!("Couldn't read {name}."), cx);
                return;
            }
        };
        // Endpoints cap requests far below disk sizes; a chip that
        // can never send is a trap.
        const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            self.push_notice(
                format!(
                    "{name} is too large to attach ({} MB limit).",
                    MAX_ATTACHMENT_BYTES / 1024 / 1024
                ),
                cx,
            );
            return;
        }
        // Path hash: re-attaching the same file replaces the chip.
        let id = {
            use std::hash::{Hash, Hasher as _};
            let mut hasher = std::hash::DefaultHasher::new();
            path.hash(&mut hasher);
            hasher.finish()
        };
        let kind = if media_type == "text/plain" {
            AttachmentKind::Text(String::from_utf8_lossy(&bytes).into_owned())
        } else {
            AttachmentKind::DataUrl(format!(
                "data:{media_type};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(&bytes)
            ))
        };
        self.attach_chip(id, name, kind, cx);
    }

    /// Parks one classified attachment as a chip, replacing any held
    /// under the same id.
    fn attach_chip(&mut self, id: u64, name: String, kind: AttachmentKind, cx: &mut Context<Self>) {
        self.attachments.retain(|held| held.id != id);
        self.attachments.push(Attachment { id, name, kind });
        cx.notify();
    }
    /// Pulls the chip back off the composer.
    fn remove_attachment(&mut self, id: u64, cx: &mut Context<Self>) {
        self.attachments.retain(|held| held.id != id);
        cx.notify();
    }

    /// Turns pending attachments into (inline text, storage markers,
    /// wire data URLs). Binary bytes land in the attachments dir under
    /// their content hash; the DB keeps only the path — a reload reads
    /// the file back. A write failure degrades to wire-only: the live
    /// turn still sees the image, it just won't survive a restart.
    /// Text attachments never touch the dir — their content joins the
    /// message text itself, so the record and the wire stay identical.
    /// A big paste is filed inside the workspace (.gertie/pastes/) and
    /// the message cites the path for the agent's read_file.
    fn take_attachments(&mut self) -> (String, String, Vec<String>) {
        use base64::Engine as _;
        let mut images = Vec::new();
        let mut markers = String::new();
        let mut inline = String::new();
        let dir = crate::config::Config::data_dir().map(|dir| dir.join("attachments"));
        for attachment in std::mem::take(&mut self.attachments) {
            match attachment.kind {
                AttachmentKind::Text(content) => {
                    let extension = attachment
                        .name
                        .rsplit('.')
                        .next()
                        .unwrap_or("txt")
                        .to_ascii_lowercase();
                    inline.push_str(&format!(
                        "\n\n--- attached file: {} ---\n```{extension}\n{content}\n```",
                        attachment.name
                    ));
                }
                AttachmentKind::Paste { name, content } => {
                    // Filed into the workspace — the only place the
                    // agent's read_file can reach — and cited by path.
                    // The write is gertie's own, not a tool call, so no
                    // checkpoint: .gertie/ is scratch by convention, and
                    // the local exclude keeps it out of git status the
                    // same way the worktrees are kept out.
                    let pastes = self.workspace_root.join(".gertie").join("pastes");
                    let path = pastes.join(&name);
                    let filed = std::fs::create_dir_all(&pastes).is_ok()
                        && std::fs::write(&path, &content).is_ok();
                    if filed {
                        let _ = crate::worktree::ensure_local_exclude(&self.workspace_root);
                        inline.push_str(&format!(
                            "\n\n--- pasted text: {name} ({} KB) — too large for the message, \
                             saved to .gertie/pastes/{name}; read it with read_file ---",
                            content.len() / 1024
                        ));
                    } else {
                        // Couldn't file it — the paste still rides, inline.
                        // Losing the user's text to a disk error is worse
                        // than a fat message.
                        inline.push_str(&format!(
                            "\n\n--- attached paste: {name} (could not be filed to disk) ---\n```\n{content}\n```"
                        ));
                    }
                }
                AttachmentKind::DataUrl(data_url) => {
                    images.push(data_url.clone());
                    let stored = match (dir.as_deref(), gertie_core::data_url_parts(&data_url)) {
                        (Some(dir), Some((_, payload))) => {
                            base64::engine::general_purpose::STANDARD
                                .decode(payload)
                                .ok()
                                .map(|bytes| {
                                    let path =
                                        dir.join(format!("{}-{}", attachment.id, attachment.name));
                                    std::fs::create_dir_all(dir).ok()?;
                                    std::fs::write(&path, bytes).ok()?;
                                    Some(path)
                                })
                                .unwrap_or(None)
                        }
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
            }
        }
        (inline, markers, images)
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
        let raw = self.input.read(cx).value().trim().to_string();
        // Invisible characters never reach a prompt — or a title. Bidi
        // controls and zero-width letters can't be typed but paste in
        // fine; stripped here at the system's entrance, so the record,
        // the transcript, and the model all read the same words.
        let typed = gertie_core::strip_invisible(&raw);
        if typed.is_empty() {
            return;
        }
        // Slash commands act on the session, not through the model:
        // builtins first, then file templates. An unknown name reports
        // and keeps the input — the user fixes the spelling instead of
        // retyping the message.
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
                "new" => {
                    // The composer blanks only when the new chat
                    // actually happens — a run in flight keeps its
                    // turn's state, and the user hears why.
                    if self.streaming {
                        self.push_notice("A run is in flight — /new waits for it to finish.", cx);
                        return;
                    }
                    self.input
                        .update(cx, |input, cx| input.set_value("", window, cx));
                    self.new_session(window, cx);
                    return;
                }
                "settings" => {
                    self.input
                        .update(cx, |input, cx| input.set_value("", window, cx));
                    self.toggle_settings(cx);
                    return;
                }
                "agents" => {
                    self.input
                        .update(cx, |input, cx| input.set_value("", window, cx));
                    self.toggle_agents(cx);
                    return;
                }
                _ => {
                    let Some(template) = gertie_tools::templates::lookup(&self.templates, &name)
                    else {
                        // The same list the bare `/` popup shows —
                        // derived, so the notice can't drift from it.
                        let names: Vec<String> = slash_matches("/", &self.templates)
                            .iter()
                            .map(|matched| match matched {
                                SlashMatch::Builtin(name, _) => format!("/{name}"),
                                SlashMatch::Template(index) => {
                                    format!("/{}", self.templates[*index].name)
                                }
                            })
                            .collect();
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
        // A template's body is front-authored too, so it rides through
        // the same strip. One notice when anything was actually
        // removed — the message itself proceeds cleaned.
        let cleaned = gertie_core::strip_invisible(&text);
        if cleaned != text || typed != raw {
            self.push_notice("Invisible characters were removed from the message.", cx);
        }
        let text = cleaned;
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
                // The folder this session's tools run in — the settle
                // hook's reclaim command runs there, whichever tab
                // settles it later.
                self.store
                    .set_session_workspace(session_id, &self.workspace_root);
                // The chip's posture at birth is the session's record —
                // config default unless the chip moved first.
                let _ = self.store.set_session_mode(session_id, self.mode);
                self.session_id = Some(session_id);
                self.sessions = self.store.list_sessions().unwrap_or_default();
                session_id
            }
        };

        // Attachments split three ways at send: text files join the
        // message text (record and wire stay identical), binary bytes
        // become storage markers for the DB and data URLs for the wire
        // — the wire text itself stays clean, the model gets files as
        // typed blocks.
        let (inline, markers, images) = self.take_attachments();
        let text = format!("{text}{inline}");
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
        // The turn's window into the checkpoint ledger, minted before
        // the loop runs: everything its mutating tools record from here
        // is this turn's work, reviewable as one diff. A mint failure
        // costs the diff view, never the turn.
        if let (Some(snapshots), Some(db_id)) = (&self.snapshots, db_id) {
            let _ = snapshots.begin_turn(session_id, db_id);
        }
        // A fresh send is the human driving again: whatever limit
        // stopped the last turn, this one supersedes it.
        let _ = self.store.set_session_limited(session_id, false);
        self.snoozed = None;
        self.sessions = self.store.list_sessions().unwrap_or_default();
        self.drive_turn(Some(text), images, cx);
    }

    /// `/compact`: replace the in-memory history with its selection now
    /// instead of waiting for the context ceiling. Shapes the next
    /// request, not the record — the DB keeps every row, so a reload
    /// restores full history. No model is called; this works with no
    /// endpoint configured at all.
    fn compact_now(&mut self, cx: &mut Context<Self>) {
        if self.streaming {
            self.push_notice("A run is in flight — wait for it to finish.", cx);
            return;
        }
        let history = self.history.clone();
        // Below twice the kept tail there's nothing to give back —
        // selecting would shrink nothing.
        let big_enough = {
            let current = history.blocking_lock();
            current.len() >= 2 * gertie_core::agent_loop::KEEP_TAIL
        };
        if !big_enough {
            self.push_notice("Nothing to compact — the history is already short.", cx);
            return;
        }
        let budget = gertie_core::agent_loop::selection_budget(self.context_window);
        cx.spawn(async move |this, cx| {
            let result = gertie_core::agent_loop::compact_history(history, budget).await;
            let _ = this.update(cx, |this, cx| match result {
                Some((kept, total)) => {
                    this.on_compacted(kept, total, cx);
                    this.activity = "Context compacted".into();
                }
                None => this.push_notice("Nothing to compact — history is small.", cx),
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

    /// A notice from the harness side — the window-close gate is the
    /// one caller — saying why something the user just did did
    /// nothing. Display-only, like every notice row.
    pub fn warn(&mut self, text: &str, cx: &mut Context<Self>) {
        self.push_notice(text, cx);
    }

    /// Hands the turn to the agent loop and renders its event stream:
    /// deltas stream into an assistant row, tool calls become cards, and
    /// results persist as tool rows the model also sees on reload.
    /// `user_text` None means *resume* — continue from history as it
    /// stands, no new user row.
    fn drive_turn(
        &mut self,
        user_text: Option<String>,
        user_attachments: Vec<String>,
        cx: &mut Context<Self>,
    ) {
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
        // Worker dispatch reads the posture the same way: set once per
        // turn, read at their spawn — never mutated mid-turn.
        self.mode_scope.set(self.mode);
        // Steering: the channel lives exactly as long as the turn —
        // Enter while streaming lands here; at the turn's end the
        // sender drops and Enter queues instead.
        let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel();
        let stream = run_turn(TurnSpec {
            provider,
            tools: self.tools.clone(),
            history: self.history.clone(),
            user_message: user_text,
            user_attachments,
            snapshots: self.snapshots.clone(),
            checkpoint_session: self.session_id,
            workspace_root: Some(self.workspace_root.clone()),
            context_window,
            steer: Some(steer_rx),
            permission_broker: Some(self.broker.clone()),
            permission_mode: self.mode,
            sandbox_bash: self.sandbox_bash,
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
                    LoopEvent::Limited { message } => {
                        let _ = this.update(cx, |this, cx| this.on_limited(&message, cx));
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
                    // A queued follow-up is the human driving again;
                    // whatever limited the last turn, this supersedes.
                    let _ = this.store.set_session_limited(session_id, false);
                    this.snoozed = None;
                    this.sessions = this.store.list_sessions().unwrap_or_default();
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
                    this.drive_turn(Some(next), Vec::new(), cx);
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
            db_id: None,
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
            db_id: None,
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
            db_id: None,
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

        // Mark the card done, remembering which one it is — the
        // persisted row's id lands on it below, so a palette hit on
        // this result can find the card after a reload too.
        let mut done_index: Option<usize> = None;
        for (index, turn) in self.turns.iter_mut().enumerate() {
            if let Turn::Tool { call, status, .. } = turn
                && call.id == call_id
            {
                *status = ToolStatus::Done(result.to_string());
                done_index = Some(index);
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
            if let Some(index) = done_index
                && let Turn::Tool {
                    db_id: turn_db_id, ..
                } = &mut self.turns[index]
            {
                *turn_db_id = Some(db_id);
            }
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
                "⟳ context compacted — {kept} of {total} messages kept: the original \
                 request, prefix lines of the omitted turns, and the live tail. \
                 read_session retrieves any omitted message in full."
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
        self.activity = "Something went wrong".into();
        self.thinking = false;
        self.persist_stream_note(format!("[error] {message}"));
        self.list_state.reset(self.item_count());
        self.list_state.scroll_to_end();
        cx.notify();
    }

    /// The turn ended on the provider's rate limit: like an error, but
    /// the state persists — the ask is "wait, then continue", so the
    /// session goes Limited (badge on the row, Resume affordance) and
    /// comes back clear on resume or the next send.
    fn on_limited(&mut self, message: &str, cx: &mut Context<Self>) {
        self.flush_stream(cx);
        self.activity = "Rate limited — pick it up when you're ready".into();
        self.thinking = false;
        // A fresh limit is a fresh decision: no stale snooze from the
        // round before survives it.
        self.snoozed = None;
        self.persist_stream_note(format!("[limited] {message}"));
        if let Some(session_id) = self.session_id {
            let _ = self.store.set_session_limited(session_id, true);
            self.sessions = self.store.list_sessions().unwrap_or_default();
        }
        self.list_state.reset(self.item_count());
        self.list_state.scroll_to_end();
        cx.notify();
    }

    /// Appends a terminal note (`[error] …` / `[limited] …`) to the
    /// assistant message in flight, or opens a fresh assistant row with
    /// it when nothing streamed.
    fn persist_stream_note(&mut self, rendered: String) {
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
    }

    /// Resume on a limited session: a new turn with no new user message
    /// — history already ends with what the model should continue from
    /// (the user's ask, or the tool result of the round that got
    /// rate-limited). The flag clears up front; if the resume hits the
    /// limit again, the turn ends limited once more.
    fn resume_clicked(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id else {
            return;
        };
        if self.streaming {
            return;
        }
        // Resuming by hand supersedes any snooze; the pending timer
        // finds the field cleared and no-ops.
        self.snoozed = None;
        let _ = self.store.set_session_limited(session_id, false);
        self.sessions = self.store.list_sessions().unwrap_or_default();
        self.drive_turn(None, Vec::new(), cx);
    }

    /// Snooze on the limited bar: the human asks for the resume later —
    /// [`SNOOZE_MINUTES`] out, one shot, this app run. A deferred
    /// click, not an auto-resume: nothing here re-arms itself, so every
    /// cycle still starts with a human.
    fn snooze_clicked(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id else {
            return;
        };
        if self.streaming {
            return;
        }
        self.snoozed = Some(session_id);
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs(SNOOZE_MINUTES * 60))
                .await;
            let _ = this.update(cx, |this, cx| this.snooze_fired(session_id, cx));
        })
        .detach();
    }

    /// The snooze is up. It only keeps its promise if this very session
    /// is still the open one, still limited, and not mid-turn — a
    /// manual resume, a new send, a switch away, or a fresh limit all
    /// dissolved it already, and then the fire is a quiet no-op.
    fn snooze_fired(&mut self, session_id: SessionId, cx: &mut Context<Self>) {
        if self.snoozed != Some(session_id) {
            return;
        }
        self.snoozed = None;
        if self.session_id == Some(session_id) && !self.streaming && self.open_session_limited() {
            self.push_notice(
                format!("Snooze is up — resuming the limited turn ({SNOOZE_MINUTES} min)."),
                cx,
            );
            self.resume_clicked(cx);
        }
    }

    /// Cancel the snooze: the session stays limited and quiet — the
    /// pending timer no-ops when it fires. The Resume button stays.
    fn cancel_snooze_clicked(&mut self, cx: &mut Context<Self>) {
        self.snoozed = None;
        cx.notify();
    }

    /// Answers a pending permission card through the broker. A dropped
    /// request (UI closed) counts as a deny in the loop; here the human
    /// did answer.
    fn decide_tool(&mut self, index: usize, decision: PermissionDecision, cx: &mut Context<Self>) {
        if let Some(Turn::Tool { status, .. }) = self.turns.get_mut(index) {
            match std::mem::replace(status, ToolStatus::Running) {
                ToolStatus::Pending(id) => {
                    // The card is settled; its reply input goes with it.
                    self.approval_replies.remove(&index);
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

    /// "Reply" on a pending card: deny with the typed text as feedback.
    /// An empty reply is a bare deny. The model sees the text in the
    /// tool result and can act on it in the next round.
    fn reply_tool(&mut self, index: usize, cx: &mut Context<Self>) {
        let feedback = self
            .approval_replies
            .get(&index)
            .map(|reply| reply.read(cx).value().trim().to_string())
            .unwrap_or_default();
        self.decide_tool(
            index,
            PermissionDecision::Deny {
                feedback: (!feedback.is_empty()).then_some(feedback),
            },
            cx,
        );
    }

    /// The reply input for a pending card, created at first render —
    /// that's where a window handle exists — and remembered until the
    /// card is answered. Per-card, never shared: two pending asks must
    /// not merge their drafts.
    fn approval_reply_for(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        self.approval_replies
            .entry(index)
            .or_insert_with(|| {
                cx.new(|cx| {
                    InputState::new(window, cx).placeholder("…or reply to the model — why deny?")
                })
            })
            .clone()
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

    fn render_turn(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
        // The reply input for a pending permission card must exist
        // before the turn is borrowed below: creating it takes &mut
        // self (and a window handle, which only render has), the match
        // takes &self for the whole arm.
        if matches!(
            self.turns.get(index),
            Some(Turn::Tool {
                status: ToolStatus::Pending(_),
                ..
            })
        ) {
            self.approval_reply_for(index, window, cx);
        }
        let inner = match &self.turns[index] {
            Turn::Message { role, text, .. } => self.render_message(role, text, cx),
            Turn::Tool {
                call,
                status,
                auto_approved,
                worker,
                ..
            } => self.render_tool(index, call, status, *auto_approved, *worker, cx),
            Turn::LoadEarlier => {
                // The pagination row: one quiet button, centered —
                // clearly furniture, not a turn. The handler gates on
                // streaming; mid-turn the index space is shifting.
                div()
                    .w_full()
                    .flex()
                    .justify_center()
                    .py_1()
                    .child(
                        Button::new("load-earlier")
                            .label("load earlier")
                            .small()
                            .ghost()
                            .tooltip("Page in older messages from the archive")
                            .on_click(cx.listener(|this, _, _, cx| this.load_earlier(cx))),
                    )
                    .into_any_element()
            }
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

    /// The quiet-state row: what's happening, in the flow of the
    /// conversation where the user is actually looking. No spinner —
    /// a repeating animation repaints every frame to say what the
    /// words already say (performance doctrine: no continuously
    /// repainting animations).
    fn render_activity_row(&self, cx: &Context<Self>) -> AnyElement {
        h_flex()
            .gap_2()
            .items_center()
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
                // transcript, where the user is looking anyway. At
                // rest, the identity line leads with the folder the
                // agent's tools run in — "which project am I talking
                // about" is answered without opening anything.
                Some(status) => status.to_string(),
                None => format!(
                    "{} · {}",
                    tildify(&self.workspace_root),
                    self.endpoint_label
                ),
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

    /// The palette, floating over the whole chat: a backdrop that
    /// eats stray clicks, the card centered above the fold. The
    /// kit's Command owns the field and the keyboard nav; the
    /// callbacks here own what a row means.
    fn render_palette_overlay(&self, view: Entity<Self>, cx: &Context<Self>) -> AnyElement {
        let search_view = view.clone();
        let jump_view = view.clone();
        let cancel_view = view;
        div()
            .absolute()
            .inset_0()
            .bg(cx.theme().overlay)
            // Backdrop click dismisses; the card stops propagation so
            // choosing text or moving focus never closes it.
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, _, _, cx| this.close_palette(cx)),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top_12()
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .w(px(640.))
                            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .child(
                                Command::new(&self.palette)
                                    .items(crate::palette::search_items(&self.palette_hits))
                                    .filterable(false)
                                    .placeholder("Search your history…")
                                    .empty(|_, _, cx| {
                                        div()
                                            .px_4()
                                            .py_3()
                                            .text_size(px(12.))
                                            .text_color(cx.theme().muted_foreground)
                                            .child("Nothing in memory matches.")
                                            .into_any_element()
                                    })
                                    .on_query(move |query, _, cx| {
                                        search_view
                                            .update(cx, |chat, cx| chat.palette_search(query, cx));
                                    })
                                    .on_confirm(move |path, _, cx| {
                                        jump_view
                                            .update(cx, |chat, cx| chat.palette_jump(path.row, cx));
                                    })
                                    .on_cancel(move |_, cx| {
                                        cancel_view.update(cx, |chat, cx| chat.close_palette(cx));
                                    }),
                            ),
                    ),
            )
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

        // No spinner on the running card: the accent label is the
        // activity, and a repeating animation repaints every frame
        // saying nothing the label doesn't (performance doctrine).
        let mut inner = v_flex().gap_2().child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(cx.theme().accent)
                        // Labels carry `code` spans (paths, commands) —
                        // through the inline renderer they become chips
                        // instead of literal backticks.
                        .child(crate::markdown::inline_element(&label, cx)),
                )
                .when(running, |row| {
                    row.child(
                        div()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child("running"),
                    )
                })
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
                let reply = self.approval_replies.get(&index);
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
                                        this.decide_tool(
                                            index,
                                            PermissionDecision::Deny { feedback: None },
                                            cx,
                                        )
                                    })),
                            ),
                    )
                    // The text path: what you type here denies the call
                    // AND goes back to the model — "no, use the other
                    // file" becomes a correction, not a dead end.
                    .children(reply.map(|reply| {
                        h_flex()
                            .gap_2()
                            .child(Input::new(reply).small().flex_1())
                            .child(
                                Button::new(SharedString::from(format!("reply-{index}")))
                                    .label("Reply")
                                    .small()
                                    .outline()
                                    .tooltip("Deny the call and send your text to the model")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.reply_tool(index, cx)
                                    })),
                            )
                    }));
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
                // The turn's own diff, from any done card in the turn:
                // the ledger answers "what did the tools do" — the
                // agent's work, not git's.
                if self.snapshots.is_some() {
                    inner = inner.child(
                        Button::new(SharedString::from(format!("turn-diff-{index}")))
                            .label("turn diff")
                            .xsmall()
                            .ghost()
                            .tooltip("What this turn changed in the workspace")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_turn_diff(index, window, cx)
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
/// Where a stored tool path lives on disk: absolute as-is, relative
/// against the workspace root — the same rule the loop's checkpoint
/// hook applies before snapshotting.
fn resolve_tool_path(root: &std::path::Path, path: &std::path::Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

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

impl ChatView {
    /// The sessions the UI may show: everything in the store except
    /// rows inside their undo window — ✕ already answered, the purge
    /// pending. The sidebar, its fold, and the palette all look here.
    fn visible_sessions(&self) -> Vec<gertie_core::SessionSummary> {
        self.sessions
            .iter()
            .filter(|summary| {
                !self
                    .pending_deletes
                    .iter()
                    .any(|pending| pending.session_id == summary.id)
            })
            .cloned()
            .collect()
    }

    /// Whether the open session's last turn ended on a rate limit —
    /// the resume bar's condition. Test-visible: the limited state is
    /// the §2.3 contract.
    pub fn open_session_limited(&self) -> bool {
        self.session_id
            .and_then(|id| self.sessions.iter().find(|summary| summary.id == id))
            .is_some_and(|summary| summary.limited)
    }

    /// The sidebar's active rows: every session not yet settled, in the
    /// store's attention order (most recent activity first).
    fn active_session_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.visible_sessions()
            .iter()
            .filter(|summary| !summary.settled)
            .map(|summary| self.session_row(summary, cx))
            .collect()
    }

    /// The fold's rows: settled work, muted, restore affordance up front.
    fn settled_session_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        self.visible_sessions()
            .iter()
            .filter(|summary| summary.settled)
            .map(|summary| self.session_row(summary, cx))
            .collect()
    }

    /// One sidebar row — active or settled; the settled variant renders
    /// muted with the restore glyph. The one shape both lists share.
    fn session_row(
        &self,
        summary: &gertie_core::SessionSummary,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session_id = summary.id;
        let settled = summary.settled;
        if !settled && self.renaming == Some(session_id) {
            // Rename row: the field replaces the row; Enter commits.
            return h_flex()
                .w_full()
                .child(Input::new(&self.rename_input).small())
                .into_any_element();
        }
        let is_active = self.session_id == Some(session_id);
        let delete_button = Button::new(SharedString::from(format!("delete-{}", session_id.0)))
            .label("✕")
            .xsmall()
            .ghost()
            .tooltip("Delete this conversation — five seconds to undo");
        h_flex()
            .w_full()
            .min_w_0()
            .gap_0p5()
            .child(
                Button::new(SharedString::from(format!("session-{}", session_id.0)))
                    .label(summary.title.clone())
                    .small()
                    .flex_1()
                    .min_w_0()
                    .when(is_active, |button| {
                        // The open conversation is tinted, not loud — the
                        // accent at conversation weight.
                        button.custom(
                            ButtonCustomVariant::new(cx)
                                .color(cx.theme().accent.opacity(0.14))
                                .hover(cx.theme().accent.opacity(0.2))
                                .active(cx.theme().accent.opacity(0.24)),
                        )
                    })
                    .when(settled, |button| {
                        // Put down, not deleted: the muted tint keeps the
                        // row readable without competing with live work.
                        button.custom(
                            ButtonCustomVariant::new(cx)
                                .color(cx.theme().muted_foreground.opacity(0.4))
                                .hover(cx.theme().muted_foreground.opacity(0.55))
                                .active(cx.theme().muted_foreground.opacity(0.7)),
                        )
                    })
                    .tooltip(summary.title.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.switch_to_session(session_id, window, cx);
                    })),
            )
            // The limited chip: the session's last turn died on the
            // provider's rate limit. Static text, warning tint — the
            // open transcript carries the Resume button.
            .when(summary.limited, |row| {
                row.child(
                    div()
                        .text_size(px(10.))
                        .text_color(cx.theme().warning)
                        .child("limited"),
                )
            })
            .child(
                Button::new(SharedString::from(format!("rename-{}", session_id.0)))
                    .label("✎")
                    .xsmall()
                    .ghost()
                    .tooltip("Rename this conversation")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_rename(session_id, window, cx);
                    })),
            )
            .child(
                Button::new(SharedString::from(format!("settle-{}", session_id.0)))
                    .label(if settled { "↩" } else { "✓" })
                    .xsmall()
                    .ghost()
                    .tooltip(if settled {
                        "Un-settle — back to the active list"
                    } else {
                        "Settle — done; folds out of the list"
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settle_clicked(session_id, !settled, cx);
                    })),
            )
            .child(
                delete_button.on_click(cx.listener(move |this, _, window, cx| {
                    this.delete_clicked(session_id, window, cx);
                })),
            )
            .into_any_element()
    }
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
            // The palette's trigger fires from wherever focus sits —
            // the root catches it on the way up the dispatch path.
            .on_action(cx.listener(
                |this, _: &crate::palette::OpenPalette, window, cx| {
                    this.open_palette(window, cx);
                },
            ))
            // Escape inside the turn-diff panel closes it (focus is
            // handed to the panel on open).
            .on_action(cx.listener(
                |this, _: &CloseTurnDiff, _, cx| this.close_turn_diff(cx),
            ))
            // Ctrl+Z outside the composer: undo a sidebar delete still
            // inside its window.
            .on_action(cx.listener(|this, _: &UndoDeleteSession, _, cx| {
                this.undo_delete_clicked(cx)
            }))
            // h_flex centers children on the cross axis by default —
            // stretch instead, so the main row fills the window height
            // and the transcript list keeps its flex sizing.
            .items_stretch()
            .relative()
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
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.new_session(window, cx)
                                    })),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .mt_1()
                                    .text_size(px(11.))
                                    .text_color(cx.theme().muted_foreground)
                                    .child("SESSIONS"),
                            )
                            .children(self.active_session_rows(cx))
                            // The settled fold: put-down work, collapsed
                            // by default — findable, never in the way.
                            .when(
                                self.visible_sessions()
                                    .iter()
                                    .any(|summary| summary.settled),
                                |col| {
                                    col.child(
                                        Button::new("settled-fold")
                                            .label(format!(
                                                "{} SETTLED ({})",
                                                if self.settled_open { "▾" } else { "▸" },
                                                self.visible_sessions()
                                                    .iter()
                                                    .filter(|summary| summary.settled)
                                                    .count()
                                            ))
                                            .xsmall()
                                            .ghost()
                                            .tooltip("Conversations put down — expand to bring one back")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.settled_open = !this.settled_open;
                                                cx.notify();
                                            })),
                                    )
                                    .when(self.settled_open, |col| {
                                        col.children(self.settled_session_rows(cx))
                                    })
                                },
                            ),
                    )
                    .child(
                        // The utilities: stacked at the
                        // sidebar's foot — findable, never
                        // in the way.
                        v_flex()
                            .gap_1()
                            .px_3()
                            .pt_2()
                            .pb_3()
                            // The undo toast: sessions inside their
                            // five-second window, newest first. One
                            // line, one button — the whole mechanism.
                            .when(!self.pending_deletes.is_empty(), |util| {
                                let newest = self
                                    .pending_deletes
                                    .last()
                                    .expect("non-empty just checked");
                                let older = self.pending_deletes.len() - 1;
                                util.child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .text_size(px(11.))
                                                .text_color(cx.theme().muted_foreground)
                                                .child(if older > 0 {
                                                    format!(
                                                        "deleted “{}” and {} more",
                                                        newest.title, older
                                                    )
                                                } else {
                                                    format!("deleted “{}”", newest.title)
                                                }),
                                        )
                                        .child(
                                            Button::new("undo-delete")
                                                .label("Undo")
                                                .xsmall()
                                                .outline()
                                                .tooltip("Put it back (Ctrl+Z outside the composer)")
                                                .on_click(cx.listener(
                                                    |this, _, _, cx| {
                                                        this.undo_delete_clicked(cx)
                                                    },
                                                )),
                                        ),
                                )
                            })
                            .child(
                                Button::new("search-history")
                                    .label("search")
                                    .small()
                                    .ghost()
                                    .w_full()
                                    .tooltip("Search every session — Ctrl+K")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_palette(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("toggle-agents")
                                    .label("agents")
                                    .small()
                                    .ghost()
                                    .w_full()
                                    .tooltip("Worker runs dispatched from this chat")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.toggle_agents(cx)
                                    })),
                            )
                            .child(
                                Button::new("toggle-settings")
                                    .label("settings")
                                    .small()
                                    .ghost()
                                    .w_full()
                                    .tooltip("Configuration, theme, and token totals")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.toggle_settings(cx)
                                    })),
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
                                list(self.list_state.clone(), move |index, window, cx| {
                                    view.update(cx, |view, cx| {
                                        view.render_turn(index, window, cx)
                                    })
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
                                    // The resume bar: the open session's
                                    // last turn ended on the provider's
                                    // rate limit. One static line, one
                                    // button — the way back in without
                                    // retyping the ask. Snoozed, the
                                    // line says the resume will happen
                                    // by itself, and "now" plus
                                    // "cancel" replace the snooze.
                                    .when(
                                        self.open_session_limited() && !self.streaming,
                                        |composer| {
                                            let snoozed = self
                                                .session_id
                                                .is_some_and(|id| self.snoozed == Some(id));
                                            composer.child(
                                                h_flex()
                                                    .px_4()
                                                    .pt_2()
                                                    .gap_2()
                                                    .items_center()
                                                    .text_size(px(11.))
                                                    .text_color(cx.theme().warning)
                                                    .child(if snoozed {
                                                        format!(
                                                            "rate limited — snoozed, the turn \
                                                             resumes by itself ({SNOOZE_MINUTES} min)"
                                                        )
                                                    } else {
                                                        "rate limited — the turn stopped \
                                                         on the provider's limit"
                                                            .into()
                                                    })
                                                    .child(
                                                        Button::new("resume-turn")
                                                            .label(if snoozed {
                                                                "Resume now"
                                                            } else {
                                                                "Resume"
                                                            })
                                                            .xsmall()
                                                            .outline()
                                                            .tooltip(
                                                                "Continue from where the turn stopped",
                                                            )
                                                            .on_click(cx.listener(
                                                                |this, _, _, cx| {
                                                                    this.resume_clicked(cx)
                                                                },
                                                            )),
                                                    )
                                                    .when(!snoozed, |bar| {
                                                        bar.child(
                                                            Button::new("snooze-turn")
                                                                .label(format!(
                                                                    "Snooze {SNOOZE_MINUTES} min"
                                                                ))
                                                                .xsmall()
                                                                .outline()
                                                                .tooltip(
                                                                    "Retry the turn by itself, \
                                                                     half an hour out",
                                                                )
                                                                .on_click(cx.listener(
                                                                    |this, _, _, cx| {
                                                                        this.snooze_clicked(cx)
                                                                    },
                                                                )),
                                                        )
                                                    })
                                                    .when(snoozed, |bar| {
                                                        bar.child(
                                                            Button::new("cancel-snooze")
                                                                .label("cancel")
                                                                .xsmall()
                                                                .ghost()
                                                                .tooltip(
                                                                    "Keep the session limited; \
                                                                     resume by hand",
                                                                )
                                                                .on_click(cx.listener(
                                                                    |this, _, _, cx| {
                                                                        this.cancel_snooze_clicked(cx)
                                                                    },
                                                                )),
                                                        )
                                                    }),
                                            )
                                        },
                                    )
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
                                        // Pending attachments: one removable
                                        // chip per file, above the input.
                                        composer.child(
                                            h_flex().px_4().pt_2().gap_2().flex_wrap().children(
                                                self.attachments.iter().map(|attachment| {
                                                    let id = attachment.id;
                                                    let name = attachment.name.clone();
                                                    Button::new(format!("attach-{id}"))
                                                        .label(format!("{name} ✕"))
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
                                                Button::new("mode-chip")
                                                    .label(format!("mode: {}", mode_label(self.mode)))
                                                    .small()
                                                    .ghost()
                                                    .tooltip(concat!(
                                                        "Permission mode — click to change. ",
                                                        "Applies from the next turn on."
                                                    ))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.cycle_mode(cx);
                                                    })),
                                            )
                                            .child(
                                                Button::new("attach-file")
                                                    // A word, not a glyph: the
                                                    // emoji-only label rasterized
                                                    // as nothing on Linux — the
                                                    // button was there and no one
                                                    // could see it.
                                                    .label("attach file")
                                                    .small()
                                                    .ghost()
                                                    .tooltip(
                                                        "Attach images, PDFs, or text files",
                                                    )
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.attach_from_picker(cx);
                                                    })),
                                            )
                                            .child(div().flex_1().child(
                                                // Auto-grown by the state: one
                                                // row at rest, up to six with
                                                // content. Paste is hooked:
                                                // images and copied files
                                                // become chips, text pastes
                                                // as text.
                                                Textarea::new(&self.input).on_paste({
                                                    let view = cx.entity();
                                                    move |item, _, cx| {
                                                        view.update(cx, |this, cx| {
                                                            this.attach_paste(item, cx)
                                                        })
                                                    }
                                                }),
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
            // The palette floats over everything, last child so it
            // paints on top.
            .when(self.palette_open, |root| {
                root.child(self.render_palette_overlay(cx.entity(), cx))
            })
            // The turn-diff panel: same modal pattern, palette first
            // (opening either closes the other).
            .when_some(self.turn_diff.as_ref(), |root, panel| {
                root.child(self.render_turn_diff(panel, cx.entity(), cx))
            })
    }
}

const NO_ENDPOINT_NOTICE: &str = "No endpoint configured — none is built in. Add one in the \
    settings panel (type `/settings` in the composer): any OpenAI-compatible URL, the model id it \
    serves, and the env var holding your key. `gertie doctor` checks the setup.";

/// A stored row as a transcript turn: tool rows render as finished
/// cards, everything else as its role's bubble. The one mapping both
/// the reload and the "load earlier" pagination share — a row must
/// look the same however it reached the screen.
fn turn_from_row(row: &gertie_core::Message) -> Turn {
    match row.role {
        Role::Tool => Turn::Tool {
            call: ToolCall {
                id: row.tool_call_id.clone().unwrap_or_default(),
                name: row.tool_name.clone().unwrap_or_default(),
                arguments: row.tool_arguments.clone().unwrap_or_default(),
            },
            status: ToolStatus::Done(row.content.clone()),
            auto_approved: false,
            worker: false,
            db_id: Some(row.id),
        },
        role => Turn::Message {
            role,
            text: display_text(&row.content),
            db_id: Some(row.id),
        },
    }
}

/// The composer chip's word for a mode — the enum's snake_case config
/// spelling, spoken the way the UI speaks.
fn mode_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Supervised => "supervised",
        PermissionMode::AcceptEdits => "accept edits",
        PermissionMode::Auto => "auto",
        PermissionMode::FullAccess => "full access",
    }
}

/// Stored text as display text: attachment markers become a quiet note
/// naming the files — the bytes live in the attachments dir, not the
/// transcript. File names, not paths: the dir is gertie's own.
fn display_text(text: &str) -> String {
    let (clean, images) = gertie_core::extract_image_markers(text);
    if images.is_empty() {
        clean
    } else {
        let names: Vec<String> = images
            .iter()
            .map(|path| {
                std::path::Path::new(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone())
            })
            .collect();
        format!("\n\n[attached: {}]", names.join(", "))
    }
}

/// `/home/martin/x/y` → `~/x/y` — the footer's folder stays short. No
/// `HOME`, or a path outside it: printed whole.
fn tildify(path: &std::path::Path) -> String {
    let home = std::env::var("HOME").ok().filter(|home| !home.is_empty());
    match home {
        Some(home) if path.starts_with(&home) => {
            let tail = path.strip_prefix(&home).unwrap_or(path);
            if tail.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", tail.display())
            }
        }
        _ => path.display().to_string(),
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
    if "new".starts_with(name.as_str()) {
        matches.push(SlashMatch::Builtin("new", "Start a fresh chat in this tab"));
    }
    if "settings".starts_with(name.as_str()) {
        matches.push(SlashMatch::Builtin("settings", "Open the settings panel"));
    }
    if "agents".starts_with(name.as_str()) {
        matches.push(SlashMatch::Builtin(
            "agents",
            "Show the worker runs dispatched from this chat",
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
