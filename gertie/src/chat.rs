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
use gpui_kit::component::input::{
    Input, InputEvent, InputState, MoveDown, MoveToEnd, MoveUp, Textarea, TextareaState,
};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    Action, AnyElement, AnyWindowHandle, App, AppContext as _, Context, Entity, EventEmitter,
    Focusable, IntoElement, ListAlignment, ListState, ParentElement, Render, SharedString,
    StatefulInteractiveElement as _, Styled, TestSupportExt as _, Window, actions, div, list,
    prelude::*, px,
};
use sha2::Digest;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

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
/// button, a content or path hash, so re-attaching the same thing
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
    /// marker, the stored transcript carries the text itself.
    Text(String),
    /// A paste too big for the message: held whole here, written into
    /// the workspace (.gertie/pastes/) at send, and cited by path,
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

/// The cadence streaming repaints at. Deltas arrive in bursts; each
/// repaint re-parses the growing row, so a token's cost must be a
/// frame's cost, not a token-shaped pile of them. 16 ms is one frame,
/// and the turn's transitions (flush_stream) notify regardless.
const STREAM_NOTIFY_CADENCE: std::time::Duration = std::time::Duration::from_millis(16);

/// The working tick: how often the status line wakes while a turn
/// runs. Two ticks a second step the spinner and roll the elapsed
/// clock's digits; the tick dies with the turn, so nothing animates
/// at rest.
const WORKING_TICK: std::time::Duration = std::time::Duration::from_millis(500);

/// How long without a loop event before the status line says so.
/// Silence during a tool run is normal (tests take minutes); silence
/// while the model chews is the difference between "slow" and
/// "stopped", and only worth speaking when it exists.
const QUIET_AFTER: std::time::Duration = std::time::Duration::from_secs(20);

/// The status spinner's frames, half-moons, one step per working
/// tick (2 fps). Geometric shapes on purpose: the emoji lesson
/// (the paperclip below), glyphs a bare Linux font stack actually
/// carries.
const SPINNER_FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];

/// The composer's ghost text at rest.
const IDLE_PLACEHOLDER: &str = "Message Gertie…, / for commands";

/// The composer's ghost text while a turn runs: the send key's two
/// meanings, stated where the keys are. Placeholders only render on
/// an empty input, so the hint vanishes with anything typed.
const STEER_PLACEHOLDER: &str = "Enter steers the running turn · Ctrl+Enter queues";

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
        /// Subagent asks get labeled, same card, provenance visible.
        worker: bool,
        /// The persisted row for this tool's result, once it exists,
        /// how a palette hit lands on the right card.
        db_id: Option<MessageId>,
    },
    /// Synthetic pagination row at the transcript's top: the store
    /// holds rows older than the loaded window. Clicking pages in the
    /// next older batch; it is never a turn the loop sees.
    LoadEarlier,
}

/// Composer recall: which sent prompt the composer is sitting on
/// (counting back from the newest) and what was typed before recall
/// began, ArrowDown past the newest restores it. `set` is the exact
/// text recall last wrote: a Change carrying anything else is the
/// user typing, which ends recall.
struct Recall {
    index: usize,
    saved: String,
    set: String,
}

/// An "edit from here" waiting on its user's choice, which flavor of
/// rewind to run, if either. The confirm row renders from this; the
/// rewind itself is real on the next send (the composer carries the
/// edited text as a draft, the transcript already shows the kept
/// prefix).
#[derive(Clone)]
struct PendingRewind {
    /// Where the edited turn sits in the transcript, the cut point.
    turn_index: usize,
    /// The edited row's id, re-verified at completion: the only
    /// guarantee that the index still means the same turn.
    edit_id: MessageId,
    /// The row the next send parents to, the edited message's own
    /// parent. `None` rewinds to the session's very start.
    keep_point: Option<MessageId>,
    /// What each later turn changed, oldest first: the file-restore
    /// offer. Per path, the earliest turn's checkpoint wins, its
    /// object is the before content for everything after.
    restore_points: Vec<(gertie_core::CheckpointId, std::path::PathBuf)>,
    /// The edited message's text, restored into the composer.
    text: String,
}

actions!(
    gertie,
    [
        CloseTurnDiff,
        UndoDeleteSession,
        RecallUp,
        RecallDown,
        CycleFocus,
        CycleFocusBack,
        FindInTranscript,
        FindClose
    ]
);

/// F6's focus areas, in cycle order: the sidebar, the transcript, the
/// composer, and the right panel when one is open. F6 walks forward
/// (wrapping), Shift+F6 backward; each area remembers the control that
/// last held focus inside it and restores it on return.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum FocusArea {
    Sidebar,
    Transcript,
    Composer,
    SidePanel,
}

/// The rewind safety guard: is this workspace root busy with someone
/// else's turn? A file restore rewrites working-tree content while a
/// model may be mid-edit in another tab pointed at the same checkout
///, the restore refuses (fail-closed) rather than race it. The
/// closure is built once by the workspace, which knows every tab;
/// the second argument is the caller, so a tab never guards against
/// itself.
pub type LiveElsewhere =
    Arc<dyn Fn(&mut App, &Entity<ChatView>, &std::path::Path) -> Result<(), String> + 'static>;

/// The workspace switch, as a chat asks for it: hand the closure a
/// folder, get back everything the chat must re-root, registry,
/// prompt, templates, sandbox posture. Built once by the workspace,
/// which owns the bootstrap; the chat never sees it. The switch is a
/// human gesture only: nothing on the model's side of the glass can
/// call it.
pub type SwitchWorkspace =
    Arc<dyn Fn(&std::path::Path) -> Result<crate::bootstrap::Switched, String> + 'static>;

/// Binds the keymap's default rules directly, the test-fixture
/// path. The app binds through `keymap::load_and_bind` (the same
/// defaults, plus the user's keymap.toml on top).
pub fn bind_chat_keys(cx: &mut App) {
    crate::keymap::bind_rules(&crate::keymap::default_rules(), cx);
}

/// One file's change in the turn-diff panel, resolved at open time,
/// the panel never re-reads the ledger while it shows.
struct TurnFileView {
    path: std::path::PathBuf,
    /// The checkpoint to restore this path's pre-turn content from.
    checkpoint: gertie_core::CheckpointId,
    diff: FileDiff,
}

/// Text files diff; binary files refuse, a lossy text rendering would
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

/// One row of the panel's list, as data, `AnyElement` isn't `Clone`,
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
/// marker, undo removes the entry, the purge finds nothing to do.
/// The snooze delay: how far a limited turn's resume is put off. Not
/// parsed from the provider's reported reset time (Retry-After and
/// friends differ per wire), a fixed half hour is the boring middle,
/// and a fired-but-still-limited turn just ends limited again for a
/// human decision.
const SNOOZE_MINUTES: u64 = 30;

struct PendingDelete {
    session_id: SessionId,
    /// The title at delete time, the toast speaks in past tense about
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
    /// Session instructions (AGENTS.md, the always-layer), framing
    /// like the prompt, threaded through on every request.
    instructions: Option<String>,
    /// The memory index, snapshotted at session start, frozen for the
    /// session. Swapped on workspace switch with the rest of the
    /// framing.
    memory_index: Option<String>,
    /// Long-term memory (M6): the flag gates extraction and dreaming,
    /// the store is the shared handle (both tiers), the extract model
    /// name picks the endpoint for the session-end call (None = this
    /// session's own endpoint). Filled from config at construction.
    memory_enabled: bool,
    memory_store: Option<Arc<gertie_core::MemoryStore>>,
    /// The extraction provider, resolved at construction (the named
    /// endpoint, else the session's own). None = extraction disabled.
    memory_extract_provider: Option<Arc<dyn gertie_core::Provider>>,
    /// Proposals awaiting a card answer, keyed by the broker ask id.
    memory_proposals: std::collections::HashMap<u64, gertie_core::memory_extract::Proposal>,
    /// Extraction debounce: last extraction trigger per session, the
    /// compaction path can fire several times an hour.
    last_memory_extract: std::collections::HashMap<u64, Instant>,
    /// The memory panel's rows: a fresh scan at open and after every
    /// vote. Empty when there is no store (headless, tests) or the
    /// store is empty; the panel and the footer word both hide then.
    memory_rows: Vec<gertie_core::MemoryEntry>,
    /// The turn's round budgets, resolved from config at boot (None =
    /// the loop's built-in defaults). Every TurnSpec carries them.
    max_tool_rounds: Option<usize>,
    grace_rounds: Option<usize>,
    /// Proposals awaiting their card's answer: the call id routes the
    /// decision to the store apply (approve) or the consumed mark (deny).
    pending_memory_apply: std::collections::HashMap<
        String,
        (
            gertie_core::memory_extract::Proposal,
            Arc<gertie_core::MemoryStore>,
            SessionId,
        ),
    >,
    /// The palette's confirm-ask id, so the answer routes to the mode
    /// change instead of a tool run. Set by `full_access_confirm`,
    /// consumed in `decide_tool`.
    pending_mode_confirm: Option<u64>,
    /// The transcript's find bar: the query (Some = open) and which
    /// match is current. Matches recompute from the turns on demand;
    /// the bar walks them with Enter/Shift+Enter, Esc closes.
    find: Option<(String, usize)>,
    /// The composer popups' selected row (slash and mentions share
    /// the grammar) with the query token it was walked under: the
    /// walk resets when the token changes (a real edit), and
    /// survives the newline insert Enter itself performs.
    popup_selection: Option<(usize, String)>,
    /// The find bar's input, a second InputState beside the composer's.
    find_input: Entity<InputState>,
    sessions: Vec<gertie_core::SessionSummary>,
    /// None until the first message, empty chats don't exist as rows, so
    /// the sidebar shows only real conversations.
    session_id: Option<SessionId>,
    turns: Vec<Turn>,
    /// Canonical conversation the loop reads and extends across turns.
    history: SharedHistory,
    /// Index of the assistant turn currently receiving deltas; None until
    /// the first delta of a round (tool rounds each get their own row).
    streaming_assistant: Option<usize>,
    /// Last persisted row, the parent for whatever is appended next.
    last_db_id: Option<MessageId>,
    /// What the agent is up to right now, for the status line, the loop
    /// goes quiet between streaming rounds, and silence reads as a hang.
    activity: String,
    /// The current work run's accumulated wall-clock time. Starts at
    /// the turn's first event, accumulates across system-imposed pauses
    /// (round budget, rate limit: the wait is part of the work), and
    /// resets when the human acts (a new send, a stop, a rewind) or the
    /// work completes. The footer's "run 12:30".
    run_elapsed: std::time::Duration,
    /// When the current accumulation segment began. None = no run is
    /// live (the clock shows nothing).
    run_started: Option<Instant>,
    /// When the current activity began. The status line's clock resets
    /// with every change, thinking, a tool, thinking again, so the
    /// number always answers "how long has THIS been going on".
    activity_at: Instant,
    /// The last event the loop delivered. The status line's quiet
    /// gauge hangs off it: nothing new for [`QUIET_AFTER`] while the
    /// model chews, and "slow" gets distinguished from "stopped".
    last_event_at: Instant,
    /// Running tool cards' clocks, keyed by wire call id. A tool can
    /// outlive several silent minutes; its card's "running · 2m 05s"
    /// is its own time, not the turn's.
    tool_starts: std::collections::HashMap<String, Instant>,
    /// What spawned workers are doing right now, the same handle the
    /// turn's spec carries: the agent tools' worker loops write their
    /// steps into it, the working tick reads them into the line (the
    /// leader's stream parks inside the agent tool, so its own events
    /// say nothing while the worker runs).
    worker_progress: gertie_core::WorkerProgress,
    /// The leader call id of the agent tool currently running, if
    /// any: the tick's key into the progress map.
    active_worker_call: Option<String>,
    /// A write card's before-content, keyed by wire call id: what the
    /// file held when the call arrived, captured before anything can
    /// run. The diff face diffs the arguments against it, exact at
    /// whatever moment the card renders this session.
    before_contents: std::collections::HashMap<String, String>,
    /// This chat's window, captured at construction, the composer's
    /// placeholder swaps from windowless async contexts (turn start
    /// and end both live inside the turn driver's spawn).
    window_handle: AnyWindowHandle,
    /// True while the turn is alive but nothing visible is happening,
    /// the request is in flight or the model is chewing on tool results.
    /// Drives the in-transcript "Thinking…" row: the header status alone
    /// was too easy to miss, and a quiet transcript reads as a hang.
    thinking: bool,
    /// Session token totals from the provider's usage events, persisted
    /// per session, so a resumed chat shows what it spent before too.
    usage: (u64, u64, u64),
    /// Token totals across all sessions, for the settings panel.
    /// Refreshed when the panel opens, never per render.
    total_usage: (u64, u64, u64),
    /// The last request's context size (prompt + completion), what the
    /// next request carries, the number the compaction check watches.
    /// Shown as a live gauge under the composer.
    context_used: u64,
    /// The provider's context window, when it declares one, the gauge's
    /// denominator.
    context_window: Option<u64>,
    /// Prompt templates for `/name` expansion in the composer. The
    /// builtins /compact, /export, /new and /settings are methods,
    /// not files.
    templates: Vec<gertie_tools::templates::Template>,
    /// The folder the agent's tools run in, shown in the footer so
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
    /// The hits behind the palette's rows, in search-rank order, the
    /// row index a confirm hands back indexes this vector.
    palette_hits: Vec<gertie_core::HistoryHit>,
    /// The `>` mode's matched commands, same row-index contract.
    palette_commands: Vec<(&'static str, &'static str, crate::palette::PaletteCommand)>,
    /// The query the palette is sitting on, the pick ledger's key and
    /// the mode switch (`>` = commands).
    palette_query: String,
    /// The turn-diff modal, when open, raised from a tool card's
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
    /// Whether bash runs inside the Landlock sandbox (config), the
    /// auto mode's bash approval keys off it.
    sandbox_bash: bool,
    /// This session's permission mode, the chip's state and the next
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
    /// Reply inputs for pending permission cards, keyed by turn index,
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
    /// resumed by itself, thirty minutes out. One shot, cleared by the
    /// fire, a manual resume, a fresh send, or a fresh limit; the
    /// pending timer no-ops whenever the field no longer agrees.
    snoozed: Option<SessionId>,
    /// Whether the sidebar's settled fold is expanded. Collapsed by
    /// default, settled work is put down, not gone; one click brings
    /// it back.
    settled_open: bool,
    /// The right column: the settings view.
    panel: Panel,
    /// The settings editor (summary + form), its Saved event bubbles
    /// to the workspace, which reloads the bootstrap and applies here.
    settings_panel: Entity<SettingsPanelView>,
    list_state: ListState,
    streaming: bool,
    /// Mid-run steering: user messages land in the running turn at the
    /// next round boundary. Set while a turn drives; cleared at its end.
    steer_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Follow-ups typed while streaming with the secondary Enter,
    /// queued rows, mirrored from the store: they run as their own
    /// turns when the current one ends, and they survive a stop, a
    /// crash, or a restart because they were rows all along.
    queued: Vec<gertie_core::QueuedFollowup>,
    /// The prompt-recall walk (ArrowUp in an empty composer), when the
    /// composer is riding one.
    recall: Option<Recall>,
    /// An "edit from here" waiting on its user's choice, the confirm
    /// row above the queue shows it.
    pending_rewind: Option<PendingRewind>,
    /// The rewind safety guard, built by the workspace (see
    /// `LiveElsewhere`): consulted before any file restore.
    live_elsewhere: LiveElsewhere,
    /// The workspace switch, built by the workspace (see
    /// `SwitchWorkspace`): the footer's word asks it, the chat applies
    /// what it returns.
    switch: SwitchWorkspace,
    /// The worktree branch naming, when this chat works in its own
    /// worktree: the branch is renamed from the session's first
    /// message, so a tab reads `gertie/fix-the-flaky-widget` where it
    /// read `gertie/tab-3`. None for shared-workspace tabs.
    branch_naming: Option<crate::worktree::BranchNaming>,
    /// F6's area containers: the sidebar, the transcript, and the
    /// right column each track one of these; focusing one means the
    /// area (or some control inside it) holds focus.
    sidebar_focus: gpui_kit::FocusHandle,
    transcript_focus: gpui_kit::FocusHandle,
    side_focus: gpui_kit::FocusHandle,
    /// Each area's last-focused control, the cycle restores it when
    /// it comes back around.
    focus_memory: std::collections::HashMap<FocusArea, gpui_kit::FocusHandle>,
    /// Images waiting to ride with the next sent message.
    attachments: Vec<Attachment>,
    /// Bytes of the streaming assistant row already written to the
    /// store. Deltas persist in coarse steps (a write per token would
    /// dominate the render loop); transitions flush the rest.
    stream_persisted: usize,
    /// Tool cards with expanded output (beyond the 12-line preview),
    /// keyed by turn index; cleared on session switch.
    expanded_tools: std::collections::HashSet<usize>,
    /// Runs of finished tool calls shown expanded into their full
    /// cards, keyed by the run's first index; cleared with the rest
    /// of the index-keyed state.
    expanded_runs: std::collections::HashSet<usize>,
    /// Memoized markdown parses per transcript row (the turn index):
    /// text is append-only per row, so a length match reuses the
    /// parse. Cleared when a reload replaces the turns.
    markdown_cache: std::cell::RefCell<crate::markdown::ParseCache>,
    /// Memoized tool-card faces per turn index (see [`ToolFace`]).
    tool_face: std::cell::RefCell<std::collections::HashMap<usize, ToolFace>>,
    /// The last repaint a streaming delta asked for, deltas coalesce
    /// to [`STREAM_NOTIFY_CADENCE`].
    stream_notify: Option<std::time::Instant>,
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
        instructions: Option<String>,
        memory_index: Option<String>,
        memory_enabled: bool,
        memory_store: Option<Arc<gertie_core::MemoryStore>>,
        memory_extract_provider: Option<Arc<dyn gertie_core::Provider>>,
        max_tool_rounds: Option<usize>,
        grace_rounds: Option<usize>,
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
        // Whether bash runs inside the Landlock sandbox (config),
        // the auto mode's bash approval keys off it.
        sandbox_bash: bool,
        // The mode new sessions open in ([permissions] default_mode).
        default_mode: gertie_core::PermissionMode,
        // The settings editor over the live config.
        settings_panel: Entity<SettingsPanelView>,
        // Resume the newest stored session (window open), or start
        // blank (a new tab, two views on one session would diverge).
        resume: bool,
        // The worktree branch naming, when this chat works in its own
        // worktree: the branch is renamed from the session's first
        // message. None = a shared-workspace tab (nothing to rename).
        branch_naming: Option<crate::worktree::BranchNaming>,
        // The rewind safety guard, built by the workspace: refuses a
        // file restore while another tab streams in the same checkout.
        live_elsewhere: LiveElsewhere,
        // The workspace switch, built by the workspace (see the type):
        // the footer's word asks it, the chat applies the answer.
        switch: SwitchWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // The composer is a multi-line text area: Enter sends,
        // Shift+Enter is a newline (long prompts, pasted snippets).
        // One row at rest, growing with content up to a cap, a tall
        // empty box made the attach button look lost beside it.
        // The palette is a component entity like the input: created
        // once, parked until Ctrl+K puts it on screen.
        let palette = cx.new(|cx| CommandState::new(window, cx));
        let turn_diff_list = ListState::new(0, ListAlignment::Top, px(256.));
        let find_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("find in this transcript…"));
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(IDLE_PLACEHOLDER)
                .auto_grow(1, 6)
        });
        cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter {
                secondary,
                shift: false,
            } = event
            {
                // A popup being open (slash commands or @-mentions)
                // means Enter accepts the top match instead of sending.
                if !this.accept_slash_top(window, cx) && !this.accept_mention_top(window, cx) {
                    this.send(*secondary, window, cx)
                }
            }
            // Typing ends the recall walk, but recall's own writes
            // don't: the set text is the walk's, until the user edits.
            if let InputEvent::Change = event {
                let value = this.input.read(cx).value().to_string();
                if let Some(recall) = &this.recall
                    && value != recall.set
                {
                    this.recall = None;
                }
                // A new query resets the popup walk: the selection
                // belonged to the last state of the list. The token,
                // not the raw value, decides: Enter's own newline
                // insert keeps the token, so the walked row survives
                // to the acceptor that runs right after.
                let token = this
                    .input
                    .read(cx)
                    .value()
                    .to_string()
                    .trim_end_matches('\n')
                    .split('@')
                    .next_back()
                    .map(|query| format!("@{query}"));
                if this
                    .popup_selection
                    .as_ref()
                    .map(|(_, walked)| walked.clone())
                    != token
                {
                    this.popup_selection = None;
                } else if let Some((_, walked)) = this.popup_selection.as_mut() {
                    *walked = token.unwrap_or_default();
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
        // The find bar: typing recomputes matches, Enter walks
        // forward, Shift+Enter back, Escape closes and refocuses
        // the composer.
        cx.subscribe_in(
            &find_input,
            window,
            |this, _, event: &InputEvent, _window, cx| match event {
                InputEvent::Change => this.find_changed(cx),
                InputEvent::PressEnter { shift, .. } => {
                    this.find_step(!*shift, cx);
                }
                _ => {}
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
        // Resume the newest unsettled session **of this workspace** if
        // there is one; settled work stays folded (opening it is the
        // un-settle door, and a cold open shouldn't shove put-down work
        // back in your face). The store is one global memory across
        // every folder, but a chat opened in a folder is that folder's
        // chat, resuming the global newest is how one conversation's
        // files end up in another folder's fresh window. Otherwise the
        // first message creates the session (lazy, so no empty rows
        // accumulate).
        let session_id = if resume {
            sessions
                .iter()
                .find(|summary| {
                    !summary.settled
                        && summary
                            .workspace_root
                            .as_deref()
                            .is_some_and(|stored| same_workspace(stored, &workspace_root))
                })
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
                    (
                        summary.input_tokens,
                        summary.output_tokens,
                        summary.cached_input_tokens,
                    ),
                    summary.context_tokens,
                )
            })
            .unwrap_or(((0, 0, 0), 0));
        let total_usage = store.total_usage();
        let context_window = provider.as_ref().and_then(|p| p.context_window());

        let mut view = Self {
            store,
            provider,
            snapshots,
            tools,
            endpoint_label,
            system_prompt,
            instructions,
            memory_index,
            memory_enabled,
            memory_store,
            memory_extract_provider,
            max_tool_rounds,
            grace_rounds,
            memory_proposals: std::collections::HashMap::new(),
            last_memory_extract: std::collections::HashMap::new(),
            memory_rows: Vec::new(),
            pending_memory_apply: std::collections::HashMap::new(),
            pending_mode_confirm: None,
            find: None,
            popup_selection: None,
            find_input,
            templates,
            workspace_root,
            settle_command,
            palette,
            palette_open: false,
            palette_hits: Vec::new(),
            palette_commands: Vec::new(),
            palette_query: String::new(),
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
            activity_at: Instant::now(),
            last_event_at: Instant::now(),
            run_elapsed: std::time::Duration::ZERO,
            run_started: None,
            tool_starts: std::collections::HashMap::new(),
            worker_progress: gertie_core::WorkerProgress::default(),
            active_worker_call: None,
            before_contents: std::collections::HashMap::new(),
            window_handle: window.window_handle(),
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
            recall: None,
            pending_rewind: None,
            live_elsewhere,
            switch,
            branch_naming,
            sidebar_focus: cx.focus_handle(),
            transcript_focus: cx.focus_handle(),
            side_focus: cx.focus_handle(),
            focus_memory: Default::default(),
            attachments: Vec::new(),
            stream_persisted: 0,
            expanded_tools: Default::default(),
            expanded_runs: Default::default(),
            markdown_cache: Default::default(),
            tool_face: Default::default(),
            stream_notify: None,
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

    /// The footer's switch, portal-free: the closure runs and the
    /// answer lands, exactly as the picker path would.
    pub fn switch_workspace_for_test(&mut self, root: &std::path::Path, cx: &mut Context<Self>) {
        let switched = (self.switch)(root).expect("test switch should succeed");
        self.apply_switch(switched, cx)
    }

    /// The composer's current text, for tests.
    pub fn composer_value_for_test(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    /// Whether a turn is driving, for tests.
    pub fn streaming_state_for_test(&self) -> bool {
        self.streaming
    }

    /// How many turns the transcript holds, for tests and tooling.
    pub fn turn_count_for_test(&self) -> usize {
        self.turns.len()
    }

    /// The current permission mode, for tests.
    pub fn mode_for_test(&self) -> PermissionMode {
        self.mode
    }

    /// The footer's ledger words: spend, cached share, the gauge with
    /// its compaction sentence. The footer renders these joined; the
    /// tests read them joined. The compaction sentence (M7 PR 7) is
    /// the mechanism's own words: the gauge shows pressure, the
    /// sentence says what will happen, and the retained tail is the
    /// loop's `KEEP_TAIL`, not a guess.
    fn footer_parts(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if self.usage.0 + self.usage.1 > 0 {
            parts.push(format!(
                "in {} · out {}",
                human_tokens(self.usage.0),
                human_tokens(self.usage.1)
            ));
        }
        // The cached share is the memory layer's scoreboard: input the
        // prefix cache absorbed, as a percent of all input. Zero or
        // absent (no cache fields this wire) renders nothing.
        if self.usage.0 > 0 && self.usage.2 > 0 {
            let percent = self.usage.2 * 100 / self.usage.0;
            parts.push(format!("cached {percent}%"));
        }
        // The context gauge names itself: "119k (11%)" answered
        // "11% of what?" for nobody. With the label it reads whole.
        let near_compaction = self
            .context_window
            .is_some_and(|window| self.context_used * 100 >= window.max(1) * 75);
        if self.context_used > 0 {
            let mut gauge = format!("ctx {}", human_tokens(self.context_used));
            if let Some(window) = self.context_window {
                let percent = self.context_used.min(window) * 100 / window.max(1);
                gauge = format!("{gauge} ({percent}%)");
                if near_compaction {
                    gauge.push_str(&format!(
                        " · next compaction keeps the last {} messages",
                        gertie_core::agent_loop::KEEP_TAIL
                    ));
                }
            }
            parts.push(gauge);
        }
        parts
    }

    /// The footer's ledger line, for tests.
    pub fn gauge_line_for_test(&self) -> String {
        self.footer_parts().join("  ·  ")
    }

    /// Sets the context window and usage, for tests (the gauge's
    /// compaction sentence rides these).
    pub fn set_context_for_test(&mut self, window: Option<u64>, used: u64, cx: &mut Context<Self>) {
        self.context_window = window;
        self.context_used = used;
        cx.notify();
    }

    /// The find bar's current match row (None = closed or no
    /// matches), for tests: the row highlight reads the same value.
    pub fn current_match_row_for_test(&self) -> Option<usize> {
        let (query, position) = self.find.as_ref()?;
        self.find_matches(query).get(*position).copied()
    }

    /// Whether the find bar is open, for tests.
    pub fn find_open_for_test(&self) -> bool {
        self.find.is_some()
    }

    /// The popup's walked row, for tests.
    pub fn popup_selection_for_test(&self) -> Option<usize> {
        self.popup_selection.as_ref().map(|(index, _)| *index)
    }

    /// Sets the popup's walked row, for tests: the key-dispatch path
    /// is exercised by the PR 4 e2e; what the walk changes is WHICH
    /// row Enter takes, and this pins it.
    pub fn set_popup_selection_for_test(&mut self, selection: Option<usize>, cx: &App) {
        // The test pins the row without a walked token: the token is
        // whatever the current query implies, computed the same way
        // the Change handler computes it.
        let token = self.last_popup_token_for_test(cx).unwrap_or_default();
        self.popup_selection = selection.map(|index| (index, token));
    }

    /// The current query's popup token, for tests.
    fn last_popup_token_for_test(&self, cx: &App) -> Option<String> {
        crate::chat::popup_token_for_test(self.input.read(cx).value().as_ref())
    }

    /// How many attachments wait for the next send, for tests.
    pub fn attachments_count_for_test(&self) -> usize {
        self.attachments.len()
    }

    /// The view's workspace root, for tests (the mention matcher is
    /// rooted here, the test needs to know what it walks).
    pub fn workspace_root_for_test(&self) -> std::path::PathBuf {
        self.workspace_root.clone()
    }

    /// Sets the endpoint label, for tests: the fixture builds views
    /// without an endpoint, and the empty state has both variants.
    pub fn set_endpoint_label_for_test(&mut self, label: &str, cx: &mut Context<Self>) {
        self.endpoint_label = label.to_string();
        cx.notify();
    }

    /// Opens the find bar (the keymap's find-transcript command).
    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find = Some((String::new(), 0));
        self.find_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    /// Closes the find bar (Escape in the bar).
    fn close_find(&mut self, cx: &mut Context<Self>) {
        self.find = None;
        cx.notify();
    }

    /// The matching turn indices for the query, case-insensitive,
    /// over every turn's visible text (messages, tool labels,
    /// notices: the bar searches what the transcript shows).
    fn find_matches(&self, query: &str) -> Vec<usize> {
        let query = query.to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        self.turns
            .iter()
            .enumerate()
            .filter(|(_, turn)| {
                let text = match turn {
                    Turn::Message { text, .. } => text.to_string(),
                    Turn::Tool { call, .. } => format!("{} {}", call.name, call.arguments),
                    Turn::LoadEarlier => String::new(),
                };
                text.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Walks the matches (forward or back), scrolling the transcript
    /// to the row and flipping the bar's input back to the composer
    /// on close only.
    fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some((query, position)) = self
            .find
            .as_ref()
            .map(|(query, position)| (query.clone(), *position))
        else {
            return;
        };
        let matches = self.find_matches(&query);
        if matches.is_empty() {
            return;
        }
        // Position is an offset into the matches vec, wrapping.
        let position = if forward {
            (position + 1) % matches.len()
        } else {
            (position + matches.len() - 1) % matches.len()
        };
        if let Some((_, slot)) = self.find.as_mut() {
            *slot = position;
        }
        let target = matches[position];
        self.list_state.pause_following_tail();
        self.list_state.scroll_to(gpui_kit::ListOffset {
            item_ix: target,
            offset_in_item: px(0.),
        });
        cx.notify();
    }

    /// The find bar's typed query: update the state and reset the
    /// position (a new query starts at the first match).
    fn find_changed(&mut self, cx: &mut Context<Self>) {
        let Some((query, position)) = &mut self.find else {
            return;
        };
        let value = self.find_input.read(cx).value().to_string();
        *query = value;
        *position = 0;
        cx.notify();
    }

    /// The empty state's headline, for tests.
    pub fn empty_state_headline_for_test(&self) -> &'static str {
        if self.endpoint_label.starts_with("no endpoint configured") {
            "Ready to run, once an endpoint exists."
        } else {
            "Ready."
        }
    }

    /// The memory word's count as the footer computes it (a live
    /// scan), for tests. Zero means the word is hidden.
    pub fn memory_rows_count_for_test(&self) -> usize {
        self.memory_rows.len()
    }

    pub fn memory_count_for_test(&self) -> usize {
        self.memory_store
            .as_ref()
            .map(|store| store.scan().len())
            .unwrap_or(0)
    }

    /// The transcript index of the oldest pending card whose call name
    /// matches, for tests that drive the card's approve/deny buttons
    /// by their rendered ids (`approve-{index}` / `deny-{index}`).
    pub fn pending_card_index_for_test(&self, name: &str) -> Option<usize> {
        self.turns.iter().position(|turn| {
            matches!(
                turn,
                Turn::Tool { call, status, .. }
                    if call.name == name && matches!(status, ToolStatus::Pending(_))
            )
        })
    }

    /// The status line's words, for tests.
    pub fn activity_line_for_test(&self) -> String {
        self.activity_line()
    }

    /// A tool card's cached diff counts, for tests: the (adds, removes)
    /// the face computed for the card at turn `index`. What a reloaded
    /// write card's diff is *of* is only visible here.
    pub fn card_diff_counts_for_test(&self, index: usize) -> Option<(usize, usize)> {
        self.tool_face
            .borrow()
            .get(&index)
            .and_then(|face| face.diff.as_ref())
            .map(|patch| (patch.adds, patch.removes))
    }

    /// Ages the status clocks, for tests, quiet's threshold is real
    /// seconds and no test waits them out. Instant arithmetic, not a
    /// mocked clock: both clocks go back together, as a real quiet
    /// stretch would leave them.
    pub fn age_status_clocks_for_test(&mut self, ago: std::time::Duration) {
        if let Some(past) = Instant::now().checked_sub(ago) {
            self.activity_at = past;
            self.last_event_at = past;
        }
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

    /// The sidebar's settled sessions, for tests, the ids that folded
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

    /// The palette's hit sessions in display order, the MRU boost's
    /// test.
    pub fn palette_hit_sessions_for_test(&self) -> Vec<gertie_core::SessionId> {
        self.palette_hits.iter().map(|hit| hit.session_id).collect()
    }

    /// The `>` mode's matched command names, in row order.
    pub fn palette_command_names_for_test(&self) -> Vec<&'static str> {
        self.palette_commands
            .iter()
            .map(|(name, _, _)| *name)
            .collect()
    }

    /// Which right-column panel is open, for tests.
    pub fn panel_for_test(&self) -> &'static str {
        match self.panel {
            Panel::None => "none",
            Panel::Settings => "settings",
            Panel::Memory => "memory",
        }
    }

    /// The palette state's entity, for tests, the selection mover.
    pub fn palette_entity_for_test(&self) -> Entity<CommandState> {
        self.palette.clone()
    }

    /// Which F6 area holds focus, for the cycle test.
    pub fn focused_area_for_test(&self, window: &Window, cx: &App) -> &'static str {
        let areas = self.focus_areas(cx);
        match self.focused_area(&areas, window, cx) {
            Some(index) => match areas[index].0 {
                FocusArea::Sidebar => "sidebar",
                FocusArea::Transcript => "transcript",
                FocusArea::Composer => "composer",
                FocusArea::SidePanel => "panel",
            },
            None => "none",
        }
    }

    /// Where the transcript's virtual list is scrolled, for tests,
    /// the first visible turn index.
    pub fn scroll_top_for_test(&self) -> usize {
        self.list_state.logical_scroll_top().item_ix
    }

    /// Scrolls the transcript to an absolute row, for tests, the
    /// programmatic stand-in for the user scrolling up (tail-follow
    /// pauses the same way).
    pub fn scroll_to_item_for_test(&self, ix: usize) {
        self.list_state.pause_following_tail();
        self.list_state.scroll_to(gpui_kit::ListOffset {
            item_ix: ix,
            offset_in_item: px(0.),
        });
    }

    /// The turn-diff panel's file count, `None` when closed, `Some(n)`
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

    /// Whether a turn is alive in this chat, for the tab strip, a tab
    /// working in the background reads as busy even when its window is
    /// elsewhere.
    pub fn is_streaming(&self) -> bool {
        self.streaming
    }

    /// Swaps the transcript for a session's stored messages and rebuilds
    /// the canonical history the loop will extend. The transcript takes
    /// a bounded window (the newest `RECENT_WINDOW` rows); the wire
    /// history takes everything, the loop resumes with exactly what
    /// the model saw before.
    /// Reloads the draft stack from the store, the rows are the
    /// truth; the vec is their shadow.
    fn sync_queue(&mut self) {
        self.queued = match self.session_id {
            Some(session_id) => self.store.queued_followups(session_id).unwrap_or_default(),
            None => Vec::new(),
        };
    }

    fn reload_session(&mut self, cx: &mut Context<Self>) {
        let (turns, history, last_db_id, archived_session) = match self.session_id {
            Some(session_id) => {
                // One page of rows renders the transcript, however deep
                // the archive grows. `limit + 1` detects a truncation
                // without a COUNT query.
                let mut rows = self
                    .store
                    .load_recent_messages(session_id, RECENT_WINDOW + 1)
                    .unwrap_or_default();
                let older_rows = rows.len() > RECENT_WINDOW;
                // The wire history starts as the page's rows, opening
                // a chat is one page of work, and the archive behind
                // the page loads in the background (below).
                let seed = SessionStore::wire_history_from_rows(&rows);
                if older_rows {
                    rows.remove(0);
                }
                let mut turns: Vec<Turn> = rows.iter().map(turn_from_row).collect();
                if older_rows {
                    turns.insert(0, Turn::LoadEarlier);
                }
                let last_db_id = rows.last().map(|row| row.id);
                (turns, seed, last_db_id, Some(session_id))
            }
            None => (Vec::new(), Vec::new(), None, None),
        };
        self.turns = turns;
        // The turn indexes name different turns now, no cached parse
        // or card face may survive the reload. The running clocks go
        // with them: a reloaded transcript has no live cards.
        self.markdown_cache.borrow_mut().clear();
        self.tool_face.borrow_mut().clear();
        self.tool_starts.clear();
        self.history = Arc::new(tokio::sync::Mutex::new(history));
        self.last_db_id = last_db_id;
        // The archive behind the page, every row, every image
        // re-read, loads off the UI thread and lands here: the seed
        // is replaced wholesale, unless the in-memory history diverged
        // meanwhile (a compaction shrank it: never re-inflate it; a
        // send grew it: its new wires ride along).
        if let Some(session_id) = archived_session {
            let store = self.store.clone();
            let history = self.history.clone();
            let seed_len = history.blocking_lock().len();
            cx.spawn(async move |this, cx| {
                let full = store.load_wire_history(session_id).unwrap_or_default();
                let _ = this.update(cx, |_this, cx| {
                    let mut current = history.blocking_lock();
                    if current.len() == seed_len {
                        *current = full;
                    } else if current.len() > seed_len {
                        let grown = current.split_off(seed_len);
                        let mut merged = full;
                        merged.extend(grown);
                        *current = merged;
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        // A rewind confirm names turns of THIS transcript, switching
        // sessions invalidates the question.
        self.pending_rewind = None;
        self.recall = None;
        self.streaming = false;
        self.thinking = false;
        self.streaming_assistant = None;
        self.expanded_tools.clear();
        self.expanded_runs.clear();
        self.approval_replies.clear();
        // Queued follow-ups are rows: they come back with the session.
        self.sync_queue();
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
    /// refused mid-stream, a live turn is still appending and would
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
            // The row that got us here was stale, retire it.
            let had_row = matches!(self.turns.first(), Some(Turn::LoadEarlier));
            if had_row {
                self.turns.remove(0);
                self.list_state.splice(0..1, 0);
            }
            cx.notify();
            return;
        }
        // Reading history pauses tail-following, the next append must
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
        // keeps the list's height cache and scroll position in step,
        // the reader holds their place, no snap.
        self.list_state.splice(0..1, added);
        // Every turn's index just shifted by the page's size, all
        // index-keyed state is stale at once.
        self.expanded_tools.clear();
        self.expanded_runs.clear();
        self.approval_replies.clear();
        self.markdown_cache.borrow_mut().clear();
        self.tool_face.borrow_mut().clear();
        cx.notify();
    }

    /// The transcript's item count: one row per turn, plus the activity
    /// row while the model is chewing and nothing else is on screen.
    fn item_count(&self) -> usize {
        self.turns.len() + usize::from(self.show_activity_row())
    }

    /// The status line's text: the label itself is the news, and the
    /// step's clock restarts with it (the activity row renders the
    /// pair). Setting it marks the loop event (the quiet gauge reads
    /// off this) and resets the spinner's step; the run clock, if a
    /// run is live, keeps accumulating untouched in the footer.
    fn set_activity(&mut self, label: impl Into<String>) {
        self.activity = label.into();
        self.activity_at = Instant::now();
        self.last_event_at = Instant::now();
        if self.run_started.is_none() {
            self.run_started = Some(Instant::now());
        }
    }

    /// The run clock's read: accumulated time plus the live segment.
    fn run_elapsed_total(&self) -> std::time::Duration {
        self.run_elapsed
            + self
                .run_started
                .map(|started| started.elapsed())
                .unwrap_or_default()
    }

    /// The run is over: the number leaves the footer ledger (the next
    /// set_activity starts a fresh run). Human stops and rewinds force
    /// this too: a stopped run's clock does not carry into new work.
    fn end_run(&mut self) {
        self.run_elapsed = self.run_elapsed_total();
        self.run_started = None;
    }

    /// Whether an approval card waits on the human. Quiet's gauge
    /// holds its tongue then: the silence is ours to answer, not the
    /// agent's to explain, and the ask's own clock already tells it.
    fn waiting_on_human(&self) -> bool {
        self.turns.iter().any(|turn| {
            matches!(
                turn,
                Turn::Tool {
                    status: ToolStatus::Pending(_),
                    ..
                }
            )
        })
    }

    /// Whether the "Thinking…" row belongs at the bottom right now: the
    /// turn is alive, but no card, prompt, or streaming text is telling
    /// the story, the exact gaps that used to read as a hang.
    fn show_activity_row(&self) -> bool {
        self.streaming && self.thinking && !self.waiting_on_human()
    }

    /// The composer chip's click: the mode advances around the ring,
    /// and persists with the session. It applies from the next turn on;
    /// a running turn keeps the posture it was built with, and the
    /// notice says so when it matters.
    ///
    /// The ring is de-escalation-only: a click never lands on full
    /// access (auto wraps to supervised), and leaving full access
    /// returns to supervised. Reaching full access costs intent: the
    /// palette command opens a confirm card (M7 plan, PR 1) that says
    /// exactly what it removes. Escalation costs a keystroke of
    /// intent; de-escalation is frictionless.
    fn cycle_mode(&mut self, cx: &mut Context<Self>) {
        let next = match self.mode {
            PermissionMode::Supervised => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::Auto,
            PermissionMode::Auto => PermissionMode::Supervised,
            PermissionMode::FullAccess => PermissionMode::Supervised,
        };
        self.set_mode(next, cx);
    }

    /// The palette's "full access" command: the only path to the mode
    /// that removes every guardrail. A confirm card states exactly
    /// what it removes; approve applies (next turn), deny dismisses.
    pub fn full_access_confirm(&mut self, cx: &mut Context<Self>) {
        if self.mode == PermissionMode::FullAccess {
            return;
        }
        let call = gertie_core::ToolCall {
            id: "mode-full-access-ask".into(),
            name: "mode_confirm".into(),
            arguments: "Full access turns off every approval: edits and bash run \
                        unasked, inside the sandbox when it is enforcing, outside it \
                        when it is not. Resume nothing-asks on purpose?"
                .into(),
        };
        let (id, _answer) = self
            .broker
            .request(call.clone(), gertie_core::PromptSource::Leader);
        self.pending_mode_confirm = Some(id);
        self.turns.push(Turn::Tool {
            call,
            status: ToolStatus::Pending(id),
            auto_approved: false,
            worker: false,
            db_id: None,
        });
        self.sync_list(cx);
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
        self.usage = (0, 0, 0);
        self.context_used = 0;
        self.turns.clear();
        self.approval_replies.clear();
        self.last_db_id = None;
        self.streaming_assistant = None;
        self.queued.clear();
        self.history.blocking_lock().clear();
        self.tool_starts.clear();
        self.close_turn_diff(cx);
        self.sync_list(cx);
        window.focus(&self.input.read(cx).focus_handle(cx), cx);
    }

    /// The composer's ghost text follows the turn: at rest it invites a
    /// message; while a turn runs it names what Enter and Ctrl+Enter
    /// do, steer the running turn, or queue for the next one. The old
    /// static hint beside the attach button left the composer row
    /// before the eyes ever got there; a placeholder renders exactly
    /// where the user is typing, and only while the input is empty, so
    /// the hint vanishes with anything typed. Swapped through the
    /// window handle captured at construction: both call sites (turn
    /// start, turn end) live in windowless async contexts.
    fn sync_composer_placeholder(&mut self, cx: &mut Context<Self>) {
        let hint = if self.streaming {
            STEER_PLACEHOLDER
        } else {
            IDLE_PLACEHOLDER
        };
        if self.input.read(cx).presentation().placeholder().as_ref() == hint {
            return;
        }
        let input = self.input.clone();
        let handle = self.window_handle;
        let app: &mut App = cx;
        let _ = handle.update(app, move |_, window, cx| {
            input.update(cx, |input, cx| input.set_placeholder(hint, window, cx));
        });
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

    /// The window-free body of a session switch, the palette's confirm
    /// callback runs with an App handle, not a window, and jumps here.
    fn switch_session_state(&mut self, session_id: SessionId, cx: &mut Context<Self>) {
        if self.streaming {
            return;
        }
        // Pending attachments are composer state, not session state,
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
        // The cross-folder check, decided before `summary` is
        // consumed below: a session rooted elsewhere (or pre-dating
        // the root column) is foreign.
        let foreign_root = summary
            .as_ref()
            .and_then(|summary| summary.workspace_root.clone())
            .filter(|stored| !same_workspace(stored, &self.workspace_root));
        self.usage = summary
            .as_ref()
            .map(|summary| {
                (
                    summary.input_tokens,
                    summary.output_tokens,
                    summary.cached_input_tokens,
                )
            })
            .unwrap_or((0, 0, 0));
        // The chip follows the session: its stored posture comes back
        // with the rest of it.
        self.mode = summary
            .as_ref()
            .map(|summary| summary.mode)
            .unwrap_or(self.default_mode);
        self.context_used = summary.map(|summary| summary.context_tokens).unwrap_or(0);
        self.reload_session(cx);
        // A conversation that started in another folder: the
        // transcript's paths are about to say so, better the tab says
        // it first. The tools stay rooted here; /new is the fresh
        // chat. After the reload on purpose: the notice is a transcript
        // row, and the reload just rebuilt the transcript.
        if let Some(stored) = foreign_root {
            self.push_notice(
                format!(
                    "This conversation started in {}, this tab's tools run in {}. \
                     /new starts a fresh chat.",
                    tildify(std::path::Path::new(&stored)),
                    tildify(&self.workspace_root)
                ),
                cx,
            );
        }
    }

    /// Ctrl+K (or the sidebar's search button): open the palette over
    /// the store's memory. A fresh query, last time's hits are last
    /// time's; and focus the field so typing starts immediately.
    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = true;
        self.palette_hits.clear();
        self.palette_commands.clear();
        self.palette_query.clear();
        self.palette.update(cx, |palette, cx| {
            palette.set_query("", window, cx);
        });
        let handle = Focusable::focus_handle(self.palette.read(cx), cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.palette_open {
            return;
        }
        self.palette_open = false;
        // Focus comes home: the hidden palette field must not keep it,
        // a dead focus strands the next Ctrl+K (no dispatch path) and
        // every keystroke in between.
        window.focus(&self.input.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// The palette's query callback. Two modes: `>` filters the
    /// command list (case-insensitive substring over name and
    /// description); anything else answers from the store's FTS index,
    /// bounded, twenty hits is a page a human can actually scan,
    /// with the query's past picks boosted to the top. Sessions inside
    /// their undo window never show up here.
    fn palette_search(&mut self, query: &str, cx: &mut Context<Self>) {
        self.palette_hits.clear();
        self.palette_commands.clear();
        self.palette_query = query.trim().to_string();
        if let Some(term) = query.trim().strip_prefix('>') {
            self.palette_commands = crate::palette::command_matches(term);
        } else if !query.trim().is_empty() {
            let mut hits: Vec<_> = self
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
            // The MRU boost: threads this query led to before rank
            // first, in pick-recency order; the rest keep the FTS
            // rank. Stable sort, both orders survive inside their
            // class.
            let boosts = self.store.palette_boosts(query.trim());
            hits.sort_by_key(|hit| {
                boosts
                    .iter()
                    .position(|picked| *picked == hit.session_id)
                    .unwrap_or(usize::MAX)
            });
            self.palette_hits = hits;
        }
        cx.notify();
    }

    /// Confirming a palette row. Command mode runs the action; a hit
    /// lands on that turn, a hit from another session switches first,
    /// either way the matching row scrolls into view, and the
    /// (query, session) pair lands in the pick ledger for next time.
    fn palette_confirm(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.palette_commands.is_empty() {
            let command = self
                .palette_commands
                .get(row)
                .map(|(_, _, command)| *command);
            self.close_palette(window, cx);
            if let Some(command) = command {
                self.run_palette_command(command, window, cx);
            }
            return;
        }
        let Some(hit) = self.palette_hits.get(row) else {
            self.close_palette(window, cx);
            return;
        };
        let session_id = hit.session_id;
        let message_id = hit.message_id;
        let query = self.palette_query.clone();
        if !query.is_empty() {
            self.store.record_palette_pick(&query, session_id);
        }
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
        self.close_palette(window, cx);
    }

    /// A `>`-picked command, run. Same actions the slash commands
    /// drive, one vocabulary, two doors.
    fn run_palette_command(
        &mut self,
        command: crate::palette::PaletteCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            crate::palette::PaletteCommand::NewChat => {
                if self.streaming {
                    self.push_notice("A run is in flight, a new chat waits for it to finish.", cx);
                    return;
                }
                self.new_session(window, cx);
            }
            crate::palette::PaletteCommand::Compact => self.compact_now(cx),
            crate::palette::PaletteCommand::Export => self.export_now(cx),
            crate::palette::PaletteCommand::Settings => self.toggle_settings(cx),
            crate::palette::PaletteCommand::FullAccess => self.full_access_confirm(cx),
            crate::palette::PaletteCommand::Memory => self.toggle_memory(cx),
        }
    }

    /// "turn diff" on a done tool card: anchor to the card's turn,
    /// the latest user message at or before it, and open the ledger's
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

    /// The panel body, window-free, tests call this directly. The
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
                "No checkpoint ledger for this turn, it predates turn diffs.",
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
                    // against the workspace the turn ran in, the
                    // ledger's record, not necessarily this chat's
                    // current root.
                    AfterState::Disk => {
                        let root = snapshots
                            .workspace_of(change.checkpoint)
                            .unwrap_or_else(|| self.workspace_root.clone());
                        std::fs::read(resolve_tool_path(&root, &change.path)).unwrap_or_default()
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

    /// Restore: every touched path back to its pre-turn content, the
    /// turn's own undo. Bounded local writes; gated on a quiet turn
    /// because files changing under a running loop is chaos.
    fn restore_turn(&mut self, cx: &mut Context<Self>) {
        if self.streaming {
            self.push_notice("A run is in flight, restoring waits for it.", cx);
            return;
        }
        let Some(panel) = self.turn_diff.take() else {
            return;
        };
        self.turn_diff_open = None;
        let mut restored = 0;
        if let Some(snapshots) = &self.snapshots {
            // The writes land where the turn ran, the ledger's
            // recorded root, not this chat's current one, so that is
            // the checkout the guard must see.
            let restore_root = panel
                .files
                .first()
                .and_then(|file| snapshots.workspace_of(file.checkpoint))
                .unwrap_or_else(|| self.workspace_root.clone());
            let caller = cx.entity();
            let guard = self.live_elsewhere.clone();
            if let Err(reason) = guard.as_ref()(cx, &caller, &restore_root) {
                self.push_notice(format!("Files kept, {reason}."), cx);
                self.turn_diff = Some(panel);
                return;
            }
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
        // view (elements are rebuilt per paint, the transcript works
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
                        rows.push(TurnDiffRow::Note("binary file, contents not shown".into()))
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
                                                            "Git's own churn (pulls, merges) never appears here, the diff is what the tools did.",
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

    /// Paints one panel row from its data. Runs per paint, per row,
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

    /// The settings panel toggle: the right column's only panel now,
    /// the toggle opens and closes it.
    fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        self.panel = if self.panel == Panel::Settings {
            Panel::None
        } else {
            Panel::Settings
        };
        self.total_usage = self.store.total_usage();
        cx.notify();
    }

    /// The memory panel toggle: entries from both tiers as rows, each
    /// with its votes. Refreshed on every open, votes write through.
    fn toggle_memory(&mut self, cx: &mut Context<Self>) {
        self.panel = if self.panel == Panel::Memory {
            Panel::None
        } else {
            Panel::Memory
        };
        self.refresh_memory_rows();
        cx.notify();
    }

    /// The panel's data: a fresh scan, project tier first. Cheap
    /// (a directory read per tier) and always current, so the panel
    /// never shows a stale count after a vote or a dream.
    fn refresh_memory_rows(&mut self) {
        self.memory_rows = self
            .memory_store
            .as_ref()
            .map(|store| store.scan())
            .unwrap_or_default();
    }

    /// One vote: writes through the store, refreshes the rows. The
    /// vote is a real write to the entry's file line, so the sidecar,
    /// the index, and dreaming all see it.
    fn vote_memory(&mut self, id: String, helpful: bool, cx: &mut Context<Self>) {
        let Some(store) = self.memory_store.as_ref() else {
            return;
        };
        match store.vote(&id, helpful) {
            // Success is visible where you're looking: the row's own
            // ▲/▼ labels carry the new counts (the rows just
            // refreshed). A notice per vote would pile one line onto
            // the transcript per click, saying what the panel already
            // shows. Errors still speak.
            Ok(_) => {
                self.refresh_memory_rows();
            }
            Err(error) => self.push_notice(format!("memory vote failed: {error}"), cx),
        }
    }

    /// The memory panel: both tiers, entries as rows with their votes.
    /// Read-and-vote, not editing: the files stay the editor's surface.
    /// The header says where the files live, so "what does it remember"
    /// and "how do I fix it" are both one glance apart.
    fn render_memory_panel(&self, cx: &Context<Self>) -> AnyElement {
        let rows: Vec<AnyElement> = match self.memory_rows.is_empty() {
            true => vec![
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child("Nothing remembered yet. Memory fills on settle (a proposal card) or when you ask the model to remember something.")
                    .into_any_element(),
            ],
            false => self
                .memory_rows
                .iter()
                .map(|entry| {
                    let id = entry.id.clone();
                    let vote_up = format!("memory-vote-up-{id}");
                    let vote_down = format!("memory-vote-down-{id}");
                    let provenance = match (&entry.source_session, &entry.added_at) {
                        (Some(source), Some(added)) => format!("{source}, {added}"),
                        (Some(source), None) => source.clone(),
                        (None, Some(added)) => format!("added {added}"),
                        (None, None) => String::new(),
                    };
                    h_flex()
                        .id(SharedString::from(format!("memory-row-{id}")))
                        .gap_2()
                        .px_1()
                        .py_0p5()
                        .rounded_sm()
                        .hover(|style| style.bg(cx.theme().secondary))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(cx.theme().muted_foreground)
                                .min_w_10()
                                .child(entry.id.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(12.))
                                .truncate()
                                .child(entry.text.clone()),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .id(SharedString::from(vote_up.clone()))
                                        .cursor_pointer()
                                        .text_size(px(11.))
                                        .text_color(cx.theme().muted_foreground)
                                        .hover(|style| style.text_color(cx.theme().foreground))
                                        .on_click(cx.listener({
                                            let id = id.clone();
                                            move |this, _, _, cx| this.vote_memory(id.clone(), true, cx)
                                        }))
                                        .child(format!("▲ {}", entry.helpful))
                                        .test_support(),
                                )
                                .child(
                                    div()
                                        .id(SharedString::from(vote_down.clone()))
                                        .cursor_pointer()
                                        .text_size(px(11.))
                                        .text_color(cx.theme().muted_foreground)
                                        .hover(|style| style.text_color(cx.theme().foreground))
                                        .on_click(cx.listener({
                                            let id = id.clone();
                                            move |this, _, _, cx| this.vote_memory(id.clone(), false, cx)
                                        }))
                                        .child(format!("▼ {}", entry.harmful))
                                        .test_support(),
                                ),
                        )
                        .when(!provenance.is_empty(), |row| {
                            row.child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(cx.theme().muted_foreground)
                                    .child(provenance),
                            )
                        })
                        .into_any_element()
                })
                .collect(),
        };
        let count = self.memory_rows.len();
        let project = self
            .memory_store
            .as_ref()
            .map(|store| store.root(gertie_core::Tier::Project).display().to_string());
        // The settings panel's sizing exactly: the right column's
        // sibling slot gives flex children nothing unless they size
        // themselves; without the width the panel mounts invisible
        // (the vote-row test proved it).
        div()
            .id("memory-panel")
            .w(px(300.))
            .h_full()
            .flex_shrink_0()
            .border_l_1()
            .border_color(cx.theme().border)
            .pt_3()
            .px_3()
            .pb_3()
            .gap_1()
            .overflow_y_scroll()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .pb_2()
                    .child(format!(
                        "memory ({count}) · {} · editing is the files' job",
                        project.as_deref().unwrap_or("no memory store")
                    )),
            )
            .children(rows)
            .into_any_element()
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
    /// Empty input cancels, an unnamed session is worse than a stale title.
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

    /// Session-end memory extraction (M6 PR 5): digest the session's
    /// record, propose delta ops to the store, and put them on a
    /// review card. Runs on settle (the human's "done") and after
    /// compaction (the context boundary), debounced once an hour.
    /// Every failure is a notice, never an error: memory must not
    /// become the reason a session feels broken.
    fn maybe_extract(&mut self, session_id: SessionId, cx: &mut Context<Self>) {
        if !self.memory_enabled {
            return;
        }
        let Some(store) = self.memory_store.clone() else {
            return;
        };
        let Some(provider) = self.memory_extract_provider.clone() else {
            return;
        };
        // Debounce: one run per hour per session, the compaction path
        // can fire repeatedly in a long session.
        if let Some(last) = self.last_memory_extract.get(&session_id.0)
            && last.elapsed() < std::time::Duration::from_secs(3600)
        {
            return;
        }
        // Eligibility: skip short sessions (a 2-turn hello holds no
        // durable facts) and anything still streaming.
        if self.streaming {
            return;
        }
        let messages = match self.store.load_messages(session_id) {
            Ok(messages) => messages,
            Err(_) => return,
        };
        let user_turns = messages
            .iter()
            .filter(|message| message.role == gertie_core::Role::User)
            .count();
        if user_turns < 4 {
            return;
        }
        let digest = gertie_core::memory_extract::digest_from(&messages, 12_000);
        let digest_hash = format!("{:x}", sha2::Sha256::digest(digest.as_bytes()));
        // The once-per-digest gate: the same record never proposes
        // twice. A denied proposal is marked consumed too: a deny is
        // a decision, not a nag.
        match store.extracted(&digest_hash) {
            Ok(true) => return,
            Ok(false) => {}
            Err(_) => return,
        }
        let index = self.memory_index.clone().unwrap_or_default();
        let flagged = messages
            .iter()
            .filter(|message| message.role == gertie_core::Role::User)
            .count()
            > 0
            && messages.iter().any(|message| {
                message.role == gertie_core::Role::Tool && message.content.contains("web_fetch")
            });
        self.last_memory_extract
            .insert(session_id.0, Instant::now());
        let store = store.clone();
        cx.spawn(async move |this, cx| {
            let raw = gertie_core::memory_extract::run_extraction(provider, &digest, &index).await;
            let proposal: Result<Option<gertie_core::memory_extract::Proposal>, String> = match raw
            {
                Ok(raw) => match gertie_core::memory_extract::parse_ops(&raw) {
                    Ok(ops) if ops.is_empty() => Ok(None),
                    Ok(ops) => Ok(Some(gertie_core::memory_extract::Proposal {
                        ops,
                        flagged,
                        digest_hash: digest_hash.clone(),
                    })),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            };
            let _ = this.update(cx, |this, cx| match proposal {
                Err(error) => {
                    let _ = store.mark_extracted(&digest_hash, Some(session_id.0 as i64));
                    this.push_notice(format!("memory: extraction skipped ({error})"), cx);
                }
                Ok(None) => {
                    // Silence is a valid extraction outcome; the gate
                    // still consumes the digest, a retry-loop is not.
                    let _ = store.mark_extracted(&digest_hash, Some(session_id.0 as i64));
                    this.push_notice("memory: nothing proposed this session", cx);
                }
                Ok(Some(proposal)) => {
                    this.memory_card(proposal, store, session_id, cx);
                }
            });
        })
        .detach();
    }

    /// The review card: one pending card in the transcript, approved
    /// ops apply to the store, a deny marks the digest consumed.
    fn memory_card(
        &mut self,
        proposal: gertie_core::memory_extract::Proposal,
        store: Arc<gertie_core::MemoryStore>,
        session_id: SessionId,
        cx: &mut Context<Self>,
    ) {
        let summary = proposal
            .ops
            .iter()
            .map(|op| match op {
                gertie_core::Op::Add { file, text, .. } => {
                    format!("add {} :: {text}", file)
                }
                gertie_core::Op::Update { id, text } => format!("update {id} :: {text}"),
                gertie_core::Op::Supersede { id } => format!("supersede {id}"),
                gertie_core::Op::Archive { id } => format!("archive {id}"),
                gertie_core::Op::Merge { keep, drop, .. } => {
                    format!("merge {drop} into {keep}")
                }
                gertie_core::Op::Link { from, to, relation } => {
                    format!("link {from} -{relation}- {to}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let header = if proposal.flagged {
            "memory proposal (session used untrusted web content; review closely)"
        } else {
            "memory proposal"
        };
        let call = gertie_core::ToolCall {
            id: format!("memory-proposal-{}", proposal.digest_hash),
            name: "memory_proposal".into(),
            arguments: format!("{{\"{header}\": {summary:?}}}"),
        };
        // The card is an ask on the shared broker: the same approve/
        // deny UI, the same card lifecycle, and the answer routes back
        // through decide_tool. The proposal rides the pending map so
        // the decision site can apply it.
        let (id, _answer) = self
            .broker
            .request(call.clone(), gertie_core::PromptSource::Leader);
        self.memory_proposals.insert(id, proposal.clone());
        let _ = (&store, session_id);
        self.pending_memory_apply
            .insert(call.id.clone(), (proposal, store, session_id));
        self.turns.push(Turn::Tool {
            call,
            status: ToolStatus::Pending(id),
            auto_approved: false,
            worker: false,
            db_id: None,
        });
        self.sync_list(cx);
    }

    /// but the store row survives a five-second undo window, the toast
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
            // not after the purge, the view never contradicts ✕.
            self.session_id = None;
            self.turns.clear();
            self.approval_replies.clear();
            self.last_db_id = None;
            self.streaming_assistant = None;
            self.history.blocking_lock().clear();
        }
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
        cx.notify();
    }

    /// Undo on the toast (button or Ctrl+Z outside the composer): pop
    /// the most recent pending delete, the row returns to its lists
    /// untouched, the store row was never gone.
    fn undo_delete_clicked(&mut self, cx: &mut Context<Self>) {
        if self.pending_deletes.pop().is_some() {
            self.sessions = self.store.list_sessions().unwrap_or_default();
            cx.notify();
        }
    }

    /// The ✓ on a sidebar row: settle, the human put the work down.
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
            self.maybe_extract(session_id, cx);
            self.run_reclaim(session_id, cx);
        }
        cx.notify();
    }

    /// The settle_command (config), run in the settling session's
    /// recorded workspace, the reclaim hook (`cargo clean` in a
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
                "no workspace recorded for this session, settle command skipped",
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
                    this.push_notice(format!("settle: `{notice_command}`, done"), cx);
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let tail = stderr.lines().next_back().unwrap_or("no output");
                    this.push_notice(format!("settle: `{notice_command}` failed, {tail}"), cx);
                }
                Err(error) => {
                    this.push_notice(format!("settle: `{notice_command}` failed, {error}"), cx)
                }
            });
        })
        .detach();
    }

    /// The attach affordance, three ways in, one destination:
    /// the "attach file" paperclip opens a native file picker;
    /// pasting intercepts images and copied files before the
    /// textarea sees them; both land in the same pending set as
    /// removable chips.
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

    /// The footer's workspace word, clicked: a folder picker, then the
    /// re-root. The rebuild (registry, prompt, overlay answers) is the
    /// bootstrap closure's; the apply is atomic here, between turns,
    /// a running turn keeps the root it started with, the same
    /// doctrine as the permission mode, and the notice says so when
    /// one is in flight.
    fn switch_workspace(&mut self, cx: &mut Context<Self>) {
        if self.branch_naming.is_some() {
            // A worktree tab's root IS its checkout; moving it would
            // break the branch-rename promise the tab was opened for.
            self.push_notice("This tab works in its own worktree, it doesn't move.", cx);
            return;
        }
        let picker = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a workspace folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let paths = match picker.await {
                Ok(Ok(Some(paths))) => paths,
                _ => return, // cancelled, or the portal is missing
            };
            let Some(root) = paths.first().cloned() else {
                return;
            };
            let switched = match this.update(cx, |this, _| (this.switch)(&root)) {
                Ok(Ok(switched)) => switched,
                Ok(Err(error)) => {
                    let _ = this.update(cx, |this, cx| {
                        this.push_notice(format!("Workspace stays, {error}."), cx)
                    });
                    return;
                }
                Err(_) => return, // the chat is gone
            };
            let _ = this.update(cx, |this, cx| this.apply_switch(switched, cx));
        })
        .detach();
    }

    /// Lands a rebuilt workspace in this chat. The running turn is
    /// safe by construction, its spec cloned the old root and
    /// registry at send, so the swap is plain field assignment, and
    /// the next send runs re-rooted. Steers included in the *old*
    /// root: they join the running turn, which is what the notice's
    /// "when this turn ends" owns.
    fn apply_switch(&mut self, switched: crate::bootstrap::Switched, cx: &mut Context<Self>) {
        self.workspace_root = switched.workspace_root;
        self.tools = switched.tools;
        self.system_prompt = switched.system_prompt;
        self.instructions = switched.instructions;
        self.memory_index = switched.memory_index;
        self.templates = switched.templates;
        self.sandbox_bash = switched.sandbox_bash;
        for warning in switched.warnings {
            self.push_notice(warning, cx);
        }
        // The one notice that states the move in the transcript's own
        // sequence: the turns after this row ran there. It dies with
        // the session's UI life (never stored), the ledger's
        // per-checkpoint workspace is the durable record.
        self.push_notice(
            format!(
                "Workspace switched to {}, the next turn runs there{}.",
                tildify(&self.workspace_root),
                if self.streaming {
                    " when this turn ends"
                } else {
                    ""
                }
            ),
            cx,
        );
        cx.notify();
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
                // Text entries paste as text, as always, unless the
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
            format!("Big paste ({kb} KB) filed as {name}, it rides as a path the agent reads with read_file, not as message text."),
            cx,
        );
        true
    }

    /// One file from the picker or a copied path: read it, classify it
    /// by extension, park it as a chip. Refusals speak, an unsupported
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
                    "Can't attach {name}, models read images, PDFs, and text files; \
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
    /// their content hash; the DB keeps only the path, a reload reads
    /// the file back. A write failure degrades to wire-only: the live
    /// turn still sees the image, it just won't survive a restart.
    /// Text attachments never touch the dir, their content joins the
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
                    // Filed into the workspace, the only place the
                    // agent's read_file can reach, and cited by path.
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
                            "\n\n--- pasted text: {name} ({} KB), too large for the message, \
                             saved to .gertie/pastes/{name}; read it with read_file ---",
                            content.len() / 1024
                        ));
                    } else {
                        // Couldn't file it, the paste still rides, inline.
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
    /// composer for editing, args ride along. Returns false when no
    /// popup is open, so Enter falls through to sending.
    fn accept_slash_top(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let value = self.input.read(cx).value().to_string();
        let matches = slash_matches(&value, &self.templates);
        // No walked selection: the top row is the default (the popup
        // is open, Enter means "accept what's highlighted", and the
        // top row is highlighted by default).
        let selected = self
            .popup_selection
            .take()
            .filter(|(selection, _)| *selection < matches.len())
            .map(|(selection, _)| selection)
            .unwrap_or(0);
        let Some(first) = matches.get(selected) else {
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

    /// Enter's mention acceptance: the popup's top row attaches.
    fn accept_mention_top(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let value = self.input.read(cx).value().to_string();
        let mut matches = mention_matches(&value, &self.workspace_root);
        let selected = self
            .popup_selection
            .take()
            .filter(|(selection, _)| *selection < matches.len())
            .map(|(selection, _)| selection)
            .unwrap_or(0);
        let Some(matched) = matches.drain(..).nth(selected) else {
            return false;
        };
        self.accept_mention(matched, window, cx);
        true
    }

    /// Accepts one @-mention row: the trailing `@query` token leaves
    /// the composer, the file attaches through the normal path (text
    /// inlines, oversized pastes file into .gertie/pastes, the chip
    /// shows what rides), and the agent reads it by its relative path.
    fn accept_mention(
        &mut self,
        matched: MentionMatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = self.input.read(cx).value().to_string();
        // The textarea keeps a trailing newline the buffer owns; the
        // token sits at the end of the user's text, before it. Empty
        // means set_value(""), not a bare newline.
        let body = value.trim_end_matches('\n');
        let trimmed = body.strip_suffix(&matched.token).unwrap_or(body).trim_end();
        self.input.update(cx, |input, cx| {
            input.set_value(trimmed.to_string(), window, cx)
        });
        let path = self.workspace_root.join(&matched.path);
        self.attach_file(path, cx);
    }

    /// Accepts one popup row: builtins execute through the normal send
    /// path; templates expand into the composer, you see the real
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
        // Invisible characters never reach a prompt, or a title. Bidi
        // controls and zero-width letters can't be typed but paste in
        // fine; stripped here at the system's entrance, so the record,
        // the transcript, and the model all read the same words.
        let typed = gertie_core::strip_invisible(&raw);
        if typed.is_empty() {
            return;
        }
        // Whatever leaves the composer ends a recall walk, the walk's
        // place in history is superseded by the send itself.
        self.recall = None;
        // Slash commands act on the session, not through the model:
        // builtins first, then file templates. An unknown name reports
        // and keeps the input, the user fixes the spelling instead of
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
                    // actually happens, a run in flight keeps its
                    // turn's state, and the user hears why.
                    if self.streaming {
                        self.push_notice("A run is in flight, /new waits for it to finish.", cx);
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
                _ => {
                    let Some(template) = gertie_tools::templates::lookup(&self.templates, &name)
                    else {
                        // The same list the bare `/` popup shows,
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
                            format!("No template `/{name}`, available: {}.", names.join(", ")),
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
        // removed, the message itself proceeds cleaned.
        let cleaned = gertie_core::strip_invisible(&text);
        if cleaned != text || typed != raw {
            self.push_notice("Invisible characters were removed from the message.", cx);
        }
        let text = cleaned;
        // The run is busy: the words still land. Enter steers the
        // running turn (the model reads it at the next round
        // boundary); the secondary Enter queues a follow-up that
        // runs as its own turn when this one ends. The queue is rows
        // in the store, so a crash keeps the words; a write that
        // can't land hands the message back, it never pretends.
        if self.streaming {
            if queue_only || self.steer_tx.is_none() {
                match self
                    .session_id
                    .map(|session_id| self.store.enqueue_followup(session_id, &text))
                {
                    Some(Ok(())) => self.sync_queue(),
                    _ => {
                        self.input
                            .update(cx, |input, cx| input.set_value(raw, window, cx));
                        self.push_notice(
                            "The queue didn't take the message, it's back in the composer.",
                            cx,
                        );
                        return;
                    }
                }
            } else if let Some(steer_tx) = &self.steer_tx {
                let _ = steer_tx.send(text);
            }
            self.input
                .update(cx, |input, cx| input.set_value("", window, cx));
            window.focus(&self.input.read(cx).focus_handle(cx), cx);
            cx.notify();
            return;
        }
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        window.focus(&self.input.read(cx).focus_handle(cx), cx);

        // Attachments split three ways at send: text files join the
        // message text (record and wire stay identical), binary bytes
        // become storage markers for the DB and data URLs for the wire
        //, the wire text itself stays clean, the model gets files as
        // typed blocks.
        let (inline, markers, images) = self.take_attachments();
        let text = format!("{text}{inline}");
        let stored = format!("{text}{markers}");
        // First message of a chat: the session is born here, titled by
        // what was typed, the invocation, not the expansion.
        let session_id = match self.session_id {
            Some(session_id) => session_id,
            None => {
                let title: String = typed.chars().take(48).collect();
                let session_id = self.store.create_session(&title).expect("create session");
                // The chip's posture at birth is the session's record,
                // config default unless the chip moved first. The
                // workspace rides in with `deliver_message`'s re-root
                // check (a fresh session's root reads None → written).
                let _ = self.store.set_session_mode(session_id, self.mode);
                self.session_id = Some(session_id);
                self.sessions = self.store.list_sessions().unwrap_or_default();
                session_id
            }
        };
        self.deliver_message(session_id, text, stored, images, cx);
    }

    /// The common tail of a real send: the row persists, the ledger
    /// mints, the loop runs. Queue drains and force-sends land here
    /// too, with no attachments of their own (stored == text, no
    /// images). The session already exists by the time this runs.
    fn deliver_message(
        &mut self,
        session_id: SessionId,
        text: String,
        stored: String,
        images: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        // The session runs where this tab runs: a workspace switched
        // from the footer re-roots the session at the next send, so
        // launch resume and the settle hook follow the conversation
        // rather than the folder it was born in. Checked per send, not
        // assumed, the common case (no switch) costs one small read.
        let stored_root = self.store.session_workspace(session_id);
        let moved = match stored_root.as_deref() {
            Some(stored) => !same_workspace(stored, &self.workspace_root),
            None => true,
        };
        if moved {
            self.store
                .set_session_workspace(session_id, &self.workspace_root);
        }
        // The session's first message names the worktree's branch,
        // computed from the same text that's about to be persisted,
        // before any state moves. Only the first: the branch name is
        // the opening request's slug, fixed once. Failures are
        // swallowed (the temporary tab-N name remains, the work goes
        // on), the branch name is a courtesy, never a gate. The git
        // subprocesses run on the background pool: the send must not
        // wait on `git branch -m`.
        let is_first = self.last_db_id.is_none();
        if is_first && let Some(naming) = &self.branch_naming {
            let naming = crate::worktree::BranchNaming {
                root: naming.root.clone(),
                prefix: naming.prefix.clone(),
            };
            let stored = stored.clone();
            cx.background_executor()
                .spawn(async move {
                    let _ = crate::worktree::name_branch_from_first_message(&naming, &stored);
                })
                .detach();
        }
        let db_id = self
            .store
            .append_message(session_id, self.last_db_id, Role::User, &stored)
            .ok();
        if db_id.is_some() {
            self.last_db_id = db_id;
        }
        self.recall = None;
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

    /// "Send now" on a queued draft: out of the queue, into the live
    /// turn as a steer, or its own turn when none runs.
    fn force_send_followup(&mut self, id: gertie_core::QueuedId, cx: &mut Context<Self>) {
        let Some(text) = self.store.take_followup(id).ok().flatten() else {
            // The row vanished under us: refresh the stack and move on.
            self.sync_queue();
            cx.notify();
            return;
        };
        self.sync_queue();
        if self.streaming
            && let Some(steer_tx) = &self.steer_tx
        {
            let _ = steer_tx.send(text);
            cx.notify();
            return;
        }
        let Some(session_id) = self.session_id else {
            return;
        };
        self.deliver_message(session_id, text.clone(), text, Vec::new(), cx);
    }

    /// "Edit" on a queued draft: pulled back into the composer (under
    /// any draft already typed there) and out of the queue, editing a
    /// queued message is just drafting it again.
    fn edit_followup(
        &mut self,
        id: gertie_core::QueuedId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.queued.iter().find(|draft| draft.id == id) else {
            return;
        };
        let text = draft.content.clone();
        let _ = self.store.remove_followup(id);
        self.sync_queue();
        let existing = self.input.read(cx).value().to_string();
        let value = if existing.trim().is_empty() {
            text
        } else {
            format!("{text}\n\n{existing}")
        };
        self.input
            .update(cx, |input, cx| input.set_value(value, window, cx));
        window.focus(&self.input.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// "✕" on a queued draft: the row goes from the store, the queue
    /// is rows, so removal is real in every direction.
    fn remove_followup(&mut self, id: gertie_core::QueuedId, cx: &mut Context<Self>) {
        let _ = self.store.remove_followup(id);
        self.sync_queue();
        cx.notify();
    }

    /// `/compact`: replace the in-memory history with its selection now
    /// instead of waiting for the context ceiling. Shapes the next
    /// request, not the record, the DB keeps every row, so a reload
    /// restores full history. No model is called; this works with no
    /// endpoint configured at all.
    fn compact_now(&mut self, cx: &mut Context<Self>) {
        if self.streaming {
            self.push_notice("A run is in flight, wait for it to finish.", cx);
            return;
        }
        let history = self.history.clone();
        // Below twice the kept tail there's nothing to give back,
        // selecting would shrink nothing.
        let big_enough = {
            let current = history.blocking_lock();
            current.len() >= 2 * gertie_core::agent_loop::KEEP_TAIL
        };
        if !big_enough {
            self.push_notice("Nothing to compact, the history is already short.", cx);
            return;
        }
        let budget = gertie_core::agent_loop::selection_budget(self.context_window);
        cx.spawn(async move |this, cx| {
            let result = gertie_core::agent_loop::compact_history(history, budget).await;
            let _ = this.update(cx, |this, cx| match result {
                Some((kept, total)) => {
                    this.on_compacted(kept, total, cx);
                    this.set_activity("Context compacted");
                }
                None => this.push_notice("Nothing to compact, history is small.", cx),
            });
        })
        .detach();
    }

    /// `/export`: write the session's persisted rows as JSONL, one
    /// object per message, the shape the model saw. Lands in the data
    /// dir's `exports/`; the notice reports the path.
    fn export_now(&mut self, cx: &mut Context<Self>) {
        let Some(session_id) = self.session_id else {
            self.push_notice("Nothing to export, this chat has no messages yet.", cx);
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

    /// A muted system row in the transcript, notices, command results.
    /// Display-only: never persisted, never sent.
    /// Re-render the frozen index (post-apply): the new content is
    /// visible next session by design, but the current session's view
    /// of "what memory holds" should not be a lie either.
    fn refresh_memory_index(&mut self, cx: &mut Context<Self>) {
        if let Some(store) = &self.memory_store {
            let rendered = store.render_index(600 * 4);
            self.memory_index = (!rendered.is_empty()).then_some(rendered);
            cx.notify();
        }
    }

    fn push_notice(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.turns.push(Turn::Message {
            role: Role::System,
            text: text.into(),
            db_id: None,
        });
        self.sync_list(cx);
    }

    /// A notice from the harness side, the window-close gate is the
    /// one caller, saying why something the user just did did
    /// nothing. Display-only, like every notice row.
    pub fn warn(&mut self, text: &str, cx: &mut Context<Self>) {
        self.push_notice(text, cx);
    }

    /// Hands the turn to the agent loop and renders its event stream:
    /// deltas stream into an assistant row, tool calls become cards, and
    /// results persist as tool rows the model also sees on reload.
    /// `user_text` None means *resume*, continue from history as it
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
        // turn, read at their spawn, never mutated mid-turn.
        self.mode_scope.set(self.mode);
        // Steering: the channel lives exactly as long as the turn,
        // Enter while streaming lands here; at the turn's end the
        // sender drops and Enter queues instead.
        let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel();
        let stream = run_turn(TurnSpec {
            provider,
            tools: self.tools.clone(),
            history: self.history.clone(),
            user_message: user_text,
            user_attachments,
            max_tool_rounds: self.max_tool_rounds,
            grace_rounds: self.grace_rounds,
            snapshots: self.snapshots.clone(),
            checkpoint_session: self.session_id,
            workspace_root: Some(self.workspace_root.clone()),
            context_window,
            steer: Some(steer_rx),
            permission_broker: Some(self.broker.clone()),
            permission_mode: self.mode,
            sandbox_bash: self.sandbox_bash,
            worker_progress: self.worker_progress.clone(),
            prompt_source: gertie_core::PromptSource::Leader,
            system_prompt: self.system_prompt.clone(),
            instructions: self.instructions.clone(),
            memory_index: self.memory_index.clone(),
        });
        // Subagent asks arrive here, not on the leader's stream, a
        // worker's loop runs inside a tool, so nobody drains its events.
        let mut worker_asks = self.broker.subscribe();

        self.streaming = true;
        self.streaming_assistant = None;
        // Turn start: fresh narration is not yet available (the model
        // has not spoken this round), so the plain label until its
        // first words stream or a tool runs.
        self.set_activity("Thinking…");
        self.thinking = true;
        self.steer_tx = Some(steer_tx);
        // The composer's ghost text names the steer/queue verbs while
        // the turn runs.
        self.sync_composer_placeholder(cx);
        self.sync_list(cx);

        // The working tick: half a second, only while this turn runs.
        // Deltas repaint by themselves; the quiet stretches, a model
        // thinking, a two-minute test run, are what the tick serves,
        // stepping the spinner and rolling the status line's clock.
        // It dies with the turn, so nothing animates at rest.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(WORKING_TICK).await;
                let alive = this
                    .update(cx, |this, cx| {
                        if !this.streaming {
                            return false;
                        }
                        // A worker's steps surface in the leader's line:
                        // the leader's stream parks inside the agent
                        // tool, so nothing else speaks while the worker
                        // runs. Each new step restarts the step clock
                        // with it; a pending ask keeps the line it has.
                        let step = match &this.active_worker_call {
                            Some(id) if !this.waiting_on_human() => this.worker_progress.get(id),
                            _ => None,
                        };
                        if let Some(step) = step {
                            let label = format!(
                                "worker: {}",
                                crate::tools_ui::action_label(
                                    &step.name,
                                    &step.arguments,
                                    step.running,
                                )
                            );
                            if this.activity != label {
                                this.set_activity(label);
                            }
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let mut stream = stream;
            // The leader stream parks inside agent tools while a worker
            // runs, its asks would be invisible. Selecting over both
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
                        arguments,
                        result,
                    } => {
                        let _ = this.update(cx, |this, cx| {
                            this.on_tool_result(&call_id, &name, &arguments, &result, cx)
                        });
                    }
                    LoopEvent::Usage {
                        input_tokens,
                        output_tokens,
                        cached_input_tokens,
                    } => {
                        let _ = this.update(cx, |this, cx| {
                            // A round boundary landed: the quiet gauge
                            // restarts even though the activity string
                            // may not change.
                            this.last_event_at = Instant::now();
                            this.usage.0 += input_tokens;
                            this.usage.1 += output_tokens;
                            // The last request's prompt + completion is
                            // the context the next request carries, the
                            // live number the compaction check watches.
                            this.context_used = input_tokens + output_tokens;
                            // The ledger outlives the view: persist per
                            // request so a resumed session shows its
                            // whole spend, not just since reopen.
                            if let Some(session) = this.session_id {
                                let _ = this
                                    .store
                                    .add_usage(session, input_tokens, output_tokens, cached_input_tokens);
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
                    LoopEvent::Nudged => {
                        // The wrap-up nudge entered history; the human
                        // hears it too, quietly, local notice only.
                        let _ = this.update(cx, |this, cx| {
                            this.push_notice(
                                "Tool-round budget reached, the model is wrapping up.",
                                cx,
                            )
                        });
                    }
                    LoopEvent::Budgeted { message } => {
                        let _ = this.update(cx, |this, cx| this.on_budgeted(&message, cx));
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
                // The run's clock closes here. A queued follow-up
                // reopens it (accumulated); a plain completion leaves
                // the total showing until the next send starts fresh.
                this.end_run();
                // The turn is over: steering closes, the queue takes
                // over. A queued follow-up runs as its own turn, right
                // now, the composer keeps whatever was typed meanwhile.
                this.steer_tx = None;
                // No running cards survive the turn, and the composer's
                // ghost text goes back to inviting a message, unless a
                // queued follow-up re-opens the turn below, which swaps
                // it again.
                this.tool_starts.clear();
                this.sync_composer_placeholder(cx);
                this.sessions = this.store.list_sessions().unwrap_or_default();
                this.sync_list(cx);
                if !this.queued.is_empty() {
                    let first = this.queued[0].id;
                    match this.store.take_followup(first) {
                        Ok(Some(text)) => {
                            this.sync_queue();
                            let session_id = match this.session_id {
                                Some(session_id) => session_id,
                                None => return,
                            };
                            this.deliver_message(session_id, text.clone(), text, Vec::new(), cx);
                        }
                        // The row vanished under us (another surface
                        // took it): drop the stale draft, run nothing.
                        _ => this.sync_queue(),
                    }
                }
            });
        })
        .detach();
    }

    /// Streams a delta into the round's assistant turn, creating its row
    /// (and database row) on the first delta.
    fn on_delta(&mut self, delta: &str, cx: &mut Context<Self>) {
        self.set_activity("Writing a reply…");
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
                // added, without a reset, scroll_to_end targets the old
                // end and the new turn renders below the viewport (the
                // "reply appeared only after I typed again" bug).
                self.list_state.reset(self.item_count());
                index
            }
        };

        // Tail mode handles following (no per-delta scroll_to_end, it
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
        // Repaints coalesce to the frame cadence: the growing row's
        // parse is real work, and deltas arrive in bursts. The turn's
        // transitions flush and notify regardless, so nothing sits
        // stale.
        let now = std::time::Instant::now();
        let due = match self.stream_notify {
            Some(last) => now.duration_since(last) >= STREAM_NOTIFY_CADENCE,
            None => true,
        };
        if due {
            self.stream_notify = Some(now);
            cx.notify();
        }
    }

    /// Writes the streaming row's unsaved tail to the store. Called when
    /// the stream changes shape, a tool starts, a prompt appears, the
    /// turn ends, so a reload always sees the full text.
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
        self.set_activity(crate::tools_ui::action_label(
            &call.name,
            &call.arguments,
            true,
        ));
        // The write's before-content: captured here, before the tool
        // can run, so the card's diff is the real change however many
        // frames later it renders.
        if call.name == "write_file"
            && let Some(content) = self.current_file_content(&call)
        {
            self.before_contents.insert(call.id.clone(), content);
        }
        // The card's clock starts here: read-only tools never prompt,
        // so their whole visible life is this card.
        self.tool_starts.insert(call.id.clone(), Instant::now());
        if call.name.starts_with("agent_") {
            // The leader's stream parks inside this call while its
            // worker runs; the tick reads the worker's steps from the
            // progress handle and speaks them on the line.
            self.active_worker_call = Some(call.id.clone());
        }
        self.thinking = false;
        self.flush_stream(cx);
        self.streaming_assistant = None;
        self.turns.push(Turn::Tool {
            call,
            status: ToolStatus::Running,
            auto_approved: true,
            worker: false,
            db_id: None,
        });
        self.sync_list(cx);
    }

    /// A mutating tool wants to run: park the request id on the card
    /// until the human answers through the broker. The status names
    /// what it waits on (a pending card must never read as a stall),
    /// and the write's before-content is captured now, the file
    /// untouched, so the review diff is the real change.
    fn on_tool_requested(&mut self, id: u64, call: ToolCall, cx: &mut Context<Self>) {
        let label = crate::tools_ui::action_label(&call.name, &call.arguments, true);
        if call.name == "ask_user" {
            // The ask's card is the question itself; "waiting on you"
            // on top would read "waiting on you: asking you".
            self.set_activity(label);
        } else {
            self.set_activity(format!("Waiting on you: {label}"));
        }
        if call.name.starts_with("agent_") {
            // Once approved, the worker's steps surface through the
            // tick; while the ask pends, waiting_on_human keeps the
            // ask's own line up.
            self.active_worker_call = Some(call.id.clone());
        }
        if call.name == "write_file"
            && let Some(content) = self.current_file_content(&call)
        {
            self.before_contents.insert(call.id.clone(), content);
        }
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

    /// A subagent's ask, arriving via the broker subscription, same
    /// card, labeled.
    fn on_worker_ask(&mut self, id: u64, call: ToolCall, cx: &mut Context<Self>) {
        let label = crate::tools_ui::action_label(&call.name, &call.arguments, true);
        self.set_activity(format!("A worker needs you: {label}"));
        if call.name == "write_file"
            && let Some(content) = self.current_file_content(&call)
        {
            self.before_contents.insert(call.id.clone(), content);
        }
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
        self.sync_list(cx);
    }

    /// The tool finished; show the outcome and persist the row so reloads
    /// and the next request see the same thing the model did.
    fn on_tool_result(
        &mut self,
        call_id: &str,
        name: &str,
        arguments: &str,
        result: &str,
        cx: &mut Context<Self>,
    ) {
        // The tool is done: the status says what was done, in the
        // card's own words. The arguments ride the event (the loop
        // owns the call when it emits), so the label is always
        // specific, never the bare verb a transcript scrape degrades
        // to when it misses. The model chewing on the result is the
        // thinking row's job; the model's narration, when it comes,
        // lives in the transcript where prose belongs.
        self.set_activity(crate::tools_ui::action_label(name, arguments, false));
        if name.starts_with("agent_") {
            // The worker's run is over: its steps leave the progress
            // map, the line belongs to the leader again.
            self.active_worker_call = None;
            self.worker_progress.clear(call_id);
        }
        self.thinking = true;
        // The card's clock retires with the card.
        self.tool_starts.remove(call_id);
        self.before_contents.remove(call_id);
        self.streaming_assistant = None;

        // Mark the card done, remembering which one it is, the
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
                arguments,
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
        // reset, not just a scroll, keeps the tail pinned.
        self.sync_list(cx);
    }

    /// Loop-level failures (provider exhausted retries, round budget hit)
    /// surface in the transcript like any other turn.
    /// History shrank mid-turn: tell the user honestly, a muted notice
    /// card, not hidden magic. Not persisted: DB rows stay whole, the
    /// compaction is the in-memory view (a reload restores full history
    /// and the loop re-compacts if the threshold trips again).
    fn on_compacted(&mut self, kept: usize, total: usize, cx: &mut Context<Self>) {
        let session_id = self.session_id;
        if let Some(session_id) = session_id {
            self.maybe_extract(session_id, cx);
        }
        self.set_activity("Compacting context…");
        self.turns.push(Turn::Message {
            role: Role::System,
            text: format!(
                "⟳ context compacted, {kept} of {total} messages kept: the original \
                 request, prefix lines of the omitted turns, and the live tail. \
                 read_session retrieves any omitted message in full."
            ),
            db_id: None,
        });
        self.sync_list(cx);
    }

    /// A steered message joined the running turn at a round boundary:
    /// persist and bubble it exactly like a send-time row, the loop
    /// owns the in-memory history, the DB row is ours.
    fn on_steered(&mut self, text: &str, cx: &mut Context<Self>) {
        // No activity change, a steer doesn't alter what's happening,
        // but the quiet gauge restarts all the same: something happened.
        self.last_event_at = Instant::now();
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
        self.set_activity("Something went wrong");
        self.thinking = false;
        self.persist_stream_note(format!("[error] {message}"));
        self.list_state.reset(self.item_count());
        self.list_state.scroll_to_end();
        cx.notify();
    }

    /// The turn ended on the provider's rate limit: like an error, but
    /// the state persists, the ask is "wait, then continue", so the
    /// session goes Limited (badge on the row, Resume affordance) and
    /// comes back clear on resume or the next send.
    fn on_limited(&mut self, message: &str, cx: &mut Context<Self>) {
        self.flush_stream(cx);
        self.set_activity("Rate limited, pick it up when you're ready");
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

    /// The turn ended on the round budget's grace window closing:
    /// identical shape to a rate limit (pause, persist, resume bar),
    /// different words. Like the limit, a fresh budget pause clears any
    /// stale snooze; if the model wraps up in the grace window instead,
    /// the turn ends Done and nothing here ever runs.
    fn on_budgeted(&mut self, message: &str, cx: &mut Context<Self>) {
        self.flush_stream(cx);
        self.set_activity("Round budget reached, pick it up when you're ready");
        self.thinking = false;
        self.snoozed = None;
        self.persist_stream_note(format!("[paused] {message}"));
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

    /// Resume on a limited or budget-paused session: a new turn with no
    /// new user message, history already ends with what the model
    /// should continue from (the user's ask, the tool result of the
    /// round that got rate-limited, or the budget's wrap-up nudge). The
    /// flag clears up front; if the resume hits the limit again, the
    /// turn ends limited once more.
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

    /// Snooze on the limited bar: the human asks for the resume later,
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
    /// is still the open one, still limited, and not mid-turn, a
    /// manual resume, a new send, a switch away, or a fresh limit all
    /// dissolved it already, and then the fire is a quiet no-op.
    fn snooze_fired(&mut self, session_id: SessionId, cx: &mut Context<Self>) {
        if self.snoozed != Some(session_id) {
            return;
        }
        self.snoozed = None;
        if self.session_id == Some(session_id) && !self.streaming && self.open_session_limited() {
            self.push_notice(
                format!("Snooze is up, resuming the limited turn ({SNOOZE_MINUTES} min)."),
                cx,
            );
            self.resume_clicked(cx);
        }
    }

    /// Cancel the snooze: the session stays limited and quiet, the
    /// pending timer no-ops when it fires. The Resume button stays.
    fn cancel_snooze_clicked(&mut self, cx: &mut Context<Self>) {
        self.snoozed = None;
        cx.notify();
    }

    /// Answers a pending permission card through the broker. A dropped
    /// request (UI closed) counts as a deny in the loop; here the human
    /// did answer.
    fn decide_tool(&mut self, index: usize, decision: PermissionDecision, cx: &mut Context<Self>) {
        if let Some(Turn::Tool { call, status, .. }) = self.turns.get_mut(index) {
            // The mode confirm routes here first: the ask id lives in
            // pending_mode_confirm, approve applies the mode, deny
            // dismisses. Nothing executes either way.
            if call.name == "mode_confirm" {
                let confirm_id = self.pending_mode_confirm.take();
                if let ToolStatus::Pending(id) = std::mem::replace(status, ToolStatus::Running) {
                    debug_assert_eq!(Some(id), confirm_id, "confirm card id matches");
                    match decision {
                        PermissionDecision::Approve => {
                            self.set_mode(PermissionMode::FullAccess, cx);
                            self.push_notice("Full access from the next turn on.", cx);
                        }
                        PermissionDecision::Deny { .. } => {
                            self.push_notice("Full access not applied.", cx);
                        }
                    }
                }
                return;
            }
            match std::mem::replace(status, ToolStatus::Running) {
                ToolStatus::Pending(id) => {
                    // The card is settled; its reply input goes with it.
                    self.approval_replies.remove(&index);
                    // A memory proposal routes through here: approve
                    // applies the ops, deny marks the digest consumed.
                    if let Some((proposal, store, session_id)) =
                        self.pending_memory_apply.remove(&call.id)
                    {
                        let digest_hash = proposal.digest_hash.clone();
                        match decision {
                            PermissionDecision::Approve => {
                                let budget = 600;
                                let report = store.apply(&proposal.ops, budget);
                                let _ =
                                    store.mark_extracted(&digest_hash, Some(session_id.0 as i64));
                                let applied = report
                                    .map(|report| {
                                        format!(
                                            "memory: {} applied, {} skipped",
                                            report.applied.len(),
                                            report.skipped.len()
                                        )
                                    })
                                    .unwrap_or_else(|error| {
                                        format!("memory: apply failed ({error})")
                                    });
                                self.push_notice(applied, cx);
                                self.refresh_memory_index(cx);
                            }
                            PermissionDecision::Deny { .. } => {
                                let _ =
                                    store.mark_extracted(&digest_hash, Some(session_id.0 as i64));
                                self.push_notice("memory: proposal denied", cx);
                            }
                        }
                        return;
                    }
                    // The approved call starts executing now, its
                    // clock starts here, not when the ask arrived.
                    self.tool_starts.insert(call.id.clone(), Instant::now());
                    if self.broker.respond(id, decision) {
                        // The human's answer is an event: the status
                        // line stops saying "waiting" and its clock
                        // hands over to the call, otherwise a long
                        // wait would surface, un-reset, as instant
                        // quiet against a stale label.
                        let label =
                            crate::tools_ui::action_label(&call.name, &call.arguments, true);
                        self.set_activity(label);
                    } else {
                        // Unknown id (already answered): put nothing
                        // back; the card was settled elsewhere.
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

    /// The reply input for a pending card, created at first render,
    /// that's where a window handle exists, and remembered until the
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
                    InputState::new(window, cx).placeholder("…or reply to the model, why deny?")
                })
            })
            .clone()
    }

    /// "Fork from here" on a persisted user turn: branch the conversation
    /// into a new session seeded with everything up to that message, and
    /// switch to it. The original keeps running its own life.
    fn fork_from(&mut self, upto: MessageId, window: &mut Window, cx: &mut Context<Self>) {
        if self.streaming {
            self.push_notice("A run is in flight, wait for it to finish.", cx);
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

    /// The folder this chat's tools run in, the guard's identity of
    /// "the same checkout".
    pub fn workspace_root(&self) -> &std::path::Path {
        &self.workspace_root
    }

    /// Whether a turn joins a collapsible run: a finished tool card.
    /// Anything alive, pending, running, stands alone: an approval
    /// hiding inside a summary is a trap.
    fn joins_run(turn: &Turn) -> bool {
        matches!(
            turn,
            Turn::Tool {
                status: ToolStatus::Done(_),
                ..
            }
        )
    }

    /// The collapsible run at `index`: 0 = this card hides inside a
    /// run that started earlier; 1 = no collapse (stand-alone card or
    /// not a tool at all); n ≥ 2 = a run starts here.
    fn tool_run_at(&self, index: usize) -> usize {
        let turn = &self.turns[index];
        if !Self::joins_run(turn) {
            return 1;
        }
        if index > 0 && Self::joins_run(&self.turns[index - 1]) {
            // A run began earlier, this card hides inside it, unless
            // the reader expanded the run to see its cards.
            let mut first = index;
            while first > 0 && Self::joins_run(&self.turns[first - 1]) {
                first -= 1;
            }
            if !self.expanded_runs.contains(&first) {
                return 0;
            }
        }
        let mut count = 1;
        while index + count < self.turns.len() && Self::joins_run(&self.turns[index + count]) {
            count += 1;
        }
        count
    }

    /// The collapsed face of a run of finished tool calls: one button
    /// stating what happened in the cards' own counts, failures on
    /// the line, never rounded away. Clicking expands to the full
    /// cards.
    fn render_tool_run(
        &self,
        index: usize,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut names = Vec::with_capacity(count);
        let mut failed = 0;
        for turn in &self.turns[index..index + count] {
            if let Turn::Tool {
                call,
                status: ToolStatus::Done(result),
                ..
            } = turn
            {
                names.push(call.name.as_str());
                if crate::tools_ui::result_failed(result) {
                    failed += 1;
                }
            }
        }
        let expanded = self.expanded_runs.contains(&index);
        let mut col = v_flex().gap_2().items_start().child(
            Button::new(SharedString::from(format!("tool-run-{index}")))
                .label(crate::tools_ui::tool_run_summary(&names, failed))
                .xsmall()
                .ghost()
                .tooltip(if expanded {
                    "Hide the calls"
                } else {
                    "Show every call in full"
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.expanded_runs.remove(&index) {
                        this.expanded_runs.insert(index);
                    }
                    cx.notify();
                })),
        );
        if expanded {
            for at in index..index + count {
                if let Turn::Tool {
                    call,
                    status,
                    auto_approved,
                    worker,
                    ..
                } = &self.turns[at]
                {
                    col = col.child(self.render_tool(
                        at,
                        call,
                        status,
                        *auto_approved,
                        *worker,
                        window,
                        cx,
                    ));
                }
            }
        }
        div().w_full().px_4().child(col).into_any_element()
    }

    /// "Edit from here" on a sent user turn: the transcript rewinds to
    /// the turn's question, which returns to the composer as a draft.
    /// The rewind is real on the next send, until then nothing is
    /// lost, and the confirm row (kept changes vs. files too) waits
    /// above the queue.
    fn begin_rewind(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.streaming {
            self.push_notice("A run is in flight, stop it before rewinding.", cx);
            return;
        }
        let Some(Turn::Message {
            role: Role::User,
            text,
            db_id: Some(edit_id),
        }) = self.turns.get(index)
        else {
            return;
        };
        let edit_id = *edit_id;
        let text = text.clone();
        let keep_point = self.store.message_parent(edit_id).ok().flatten();
        let restore_points = self.rewind_restore_points(keep_point);
        self.pending_rewind = Some(PendingRewind {
            turn_index: index,
            edit_id,
            keep_point,
            restore_points,
            text,
        });
        cx.notify();
    }

    /// What restoring "files too" would rewrite, oldest first: every
    /// turn after the keep-point, its first checkpoint per path. No
    /// ledger or no later turns means the files choice has nothing to
    /// offer, the keep-changes rewind still stands.
    fn rewind_restore_points(
        &self,
        keep_point: Option<MessageId>,
    ) -> Vec<(gertie_core::CheckpointId, std::path::PathBuf)> {
        let Some(snapshots) = &self.snapshots else {
            return Vec::new();
        };
        let Some(session_id) = self.session_id else {
            return Vec::new();
        };
        let turns = match snapshots.turns_for_session(session_id) {
            Ok(turns) => turns,
            Err(_) => return Vec::new(),
        };
        let mut seen = std::collections::HashSet::new();
        let mut points = Vec::new();
        for turn in turns
            .iter()
            .filter(|turn| keep_point.is_none_or(|keep| turn.user_message_id > keep))
        {
            let Ok(changes) = snapshots.turn_changes(turn) else {
                continue;
            };
            for change in changes {
                if seen.insert(change.path.clone()) {
                    points.push((change.checkpoint, change.path));
                }
            }
        }
        points
    }

    /// Runs the confirmed rewind. Files restore first, any failure
    /// aborts the whole thing with the view untouched (never
    /// files-without-history, never the reverse either), then the
    /// transcript and the loop's history truncate to the kept prefix.
    fn complete_rewind(
        &mut self,
        restore_files: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Peeking, not taking: a refused rewind keeps the confirm row
        // open, the user's question doesn't vanish because the files
        // were busy. Only the commit point consumes it.
        let Some(pending) = self.pending_rewind.clone() else {
            return;
        };
        if self.streaming {
            self.push_notice("A run is in flight, stop it before rewinding.", cx);
            return;
        }
        // The turn we're rewinding must still be the one the user
        // pointed at (pagination can have reshuffled the list in
        // between), a stale confirm closes itself.
        let still_there = matches!(
            self.turns.get(pending.turn_index),
            Some(Turn::Message {
                db_id: Some(id),
                ..
            }) if *id == pending.edit_id
        );
        if !still_there {
            self.pending_rewind = None;
            self.push_notice("The conversation moved, rewind cancelled.", cx);
            return;
        }
        if restore_files {
            let Some(snapshots) = &self.snapshots else {
                self.push_notice("No checkpoint ledger, files can't be restored.", cx);
                return;
            };
            // The restores write where the turn ran (the ledger's
            // recorded root), so that's the checkout the guard must
            // see, not the live root, which a moved-on chat no
            // longer points at.
            let restore_root = pending
                .restore_points
                .first()
                .and_then(|(checkpoint, _)| snapshots.workspace_of(*checkpoint))
                .unwrap_or_else(|| self.workspace_root.clone());
            let caller = cx.entity();
            let guard = self.live_elsewhere.clone();
            if let Err(reason) = guard.as_ref()(cx, &caller, &restore_root) {
                self.push_notice(format!("Files kept, {reason}."), cx);
                return;
            }
            for (checkpoint, path) in &pending.restore_points {
                if let Err(error) = snapshots.restore_under(*checkpoint, &self.workspace_root) {
                    self.push_notice(
                        format!(
                            "Rewind stopped, {} could not be restored: {error}. \
                             The choice stays open; fix the file and try again.",
                            path.display()
                        ),
                        cx,
                    );
                    return;
                }
            }
        }
        let PendingRewind {
            turn_index,
            keep_point,
            text,
            ..
        } = pending;
        // Commit point. View surgery now that every fallible step is
        // behind us. The next send parents to the keep point, the
        // abandoned branch stays in the ledger as audit, off every
        // read.
        self.pending_rewind = None;
        self.last_db_id = keep_point;
        self.turns.truncate(turn_index);
        // The kept prefix, rebuilt as the loop's wire history. Rows at
        // or before the keep point are identical on every branch, so
        // filtering the canonical path by id is branch-independent.
        let prefix = match (self.session_id, pending.keep_point) {
            (Some(session_id), Some(keep)) => self
                .store
                .load_messages(session_id)
                .unwrap_or_default()
                .into_iter()
                .filter(|row| row.id <= keep)
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        *self.history.blocking_lock() = SessionStore::wire_history_from_rows(&prefix);
        // The expanded tool cards and unanswered reply boxes belonged
        // to turns the transcript no longer shows.
        self.expanded_tools.clear();
        self.expanded_runs.clear();
        self.approval_replies.clear();
        // The edited question returns as the composer's draft.
        self.input
            .update(cx, |input, cx| input.set_value(text, window, cx));
        window.dispatch_action(Box::new(MoveToEnd), cx);
        window.focus(&self.input.read(cx).focus_handle(cx), cx);
        self.sync_list(cx);
        cx.notify();
    }

    /// "Never mind" on the rewind confirm: nothing was changed by
    /// asking.
    fn cancel_rewind(&mut self, cx: &mut Context<Self>) {
        self.pending_rewind = None;
        cx.notify();
    }

    /// "Copy" on an assistant turn: the raw markdown rides the
    /// clipboard, out of the transcript and into wherever the user
    /// actually needs it (a PR description, a commit body, a ticket).
    fn copy_turn_to_clipboard(&mut self, text: std::sync::Arc<str>, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text.to_string()));
        self.push_notice("copied", cx);
    }

    /// "Quote" on an assistant turn: the response lands in the composer
    /// as a blockquote and your comment types after it, sending shows
    /// the model exactly which words you're talking about. A long
    /// response quotes bounded: enough to pin the referent, never the
    /// whole answer back into the prompt.
    fn quote_into_composer(
        &mut self,
        text: std::sync::Arc<str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        const QUOTE_LIMIT: usize = 800;
        let mut quoted = text.trim().to_string();
        if quoted.len() > QUOTE_LIMIT {
            let mut cut = QUOTE_LIMIT;
            while !quoted.is_char_boundary(cut) {
                cut -= 1;
            }
            quoted = format!("{}… [truncated]", &quoted[..cut]);
        }
        let quoted = quoted
            .lines()
            .map(|line| format!("> {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let draft = self.input.read(cx).value().to_string();
        let value = if draft.trim().is_empty() {
            format!("{quoted}\n\n")
        } else {
            // The comment you already typed rides under the quote,
            // the quote is context, the draft is the point.
            format!("{quoted}\n\n{draft}")
        };
        self.input
            .update(cx, |input, cx| input.set_value(value, window, cx));
        window.focus(&self.input.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// The sent prompts recall walks, newest first: user turns that
    /// actually landed in the ledger. A queued draft was never sent,
    /// and a rewound-away branch is no longer this view's history.
    fn sent_user_texts(&self) -> Vec<String> {
        self.turns
            .iter()
            .rev()
            .filter_map(|turn| match turn {
                Turn::Message {
                    role: Role::User,
                    text,
                    db_id: Some(_),
                } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// One ArrowUp/Down step in the composer. Recall claims the
    /// keystroke only when the composer is focused and empty (or
    /// already riding the walk); every other case hands the cursor
    /// move back, a half-typed draft never gets clobbered, and other
    /// inputs on the surface keep their own arrow behavior.
    fn recall_step(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        let value = self.input.read(cx).value().to_string();
        // A composer popup (slash commands or @-mentions) owns the
        // arrows while it's open: the walk moves its selection, the
        // composer's text is the query, recall waits. The count is
        // the popup's, recomputed the same way the render computes it.
        // Same trailing-newline trim the matcher does: the buffer's
        // newline is the textarea's, not the user's.
        let query = value.trim_end_matches('\n');
        let slash_rows = slash_matches(query, &self.templates).len();
        let popup_rows = if slash_rows == 0 {
            mention_matches(query, &self.workspace_root).len()
        } else {
            slash_rows
        };
        if focused && popup_rows > 0 {
            let walked = self.popup_selection.take();
            let current = match walked {
                Some((index, _)) => index,
                None if up => popup_rows,
                None => usize::MAX,
            };
            let step = if up { -1isize } else { 1isize };
            let next = (current as isize + step).rem_euclid(popup_rows as isize) as usize;
            // The token is the query the walk happened under: the
            // reset-on-Change comparison uses it.
            let token = if slash_rows == 0 {
                format!("@{}", query.rsplit('@').next().unwrap_or(""))
            } else {
                query.to_string()
            };
            self.popup_selection = Some((next, token));
            cx.notify();
            return;
        }
        if !focused || (!value.trim().is_empty() && self.recall.is_none()) {
            let move_cursor: Box<dyn Action> = if up {
                Box::new(MoveUp)
            } else {
                Box::new(MoveDown)
            };
            window.dispatch_action(move_cursor, cx);
            return;
        }
        let entries = self.sent_user_texts();
        if entries.is_empty() {
            // Nothing sent yet, nothing to recall. (The composer is
            // empty; the cursor move this keystroke would have done
            // has nothing to move past either.)
            self.recall = None;
            return;
        }
        let Some(active) = self.recall.take() else {
            // Starting the walk: save the (empty) draft, land on the
            // newest sent prompt. Down with nothing riding is just a
            // cursor move.
            if !up {
                window.dispatch_action(Box::new(MoveDown), cx);
                return;
            }
            let text = entries[0].clone();
            self.recall = Some(Recall {
                index: 0,
                saved: value,
                set: text.clone(),
            });
            self.input
                .update(cx, |input, cx| input.set_value(text, window, cx));
            window.dispatch_action(Box::new(MoveToEnd), cx);
            return;
        };
        // Riding the walk: Up goes older (clamped at the oldest sent
        // prompt), Down goes newer.
        let index = if up {
            (active.index + 1).min(entries.len() - 1)
        } else {
            active.index.saturating_sub(1)
        };
        // Down past the newest hands the composer back its saved draft
        //, the walk ends where it started.
        if !up && active.index == 0 {
            self.input
                .update(cx, |input, cx| input.set_value(active.saved, window, cx));
            return;
        }
        let text = entries[index].clone();
        self.recall = Some(Recall {
            index,
            saved: active.saved,
            set: text.clone(),
        });
        self.input
            .update(cx, |input, cx| input.set_value(text, window, cx));
        window.dispatch_action(Box::new(MoveToEnd), cx);
    }

    /// The area cycle's ordered list: sidebar, transcript, composer,
    /// and the right column when one is open. The composer's handle is
    /// the input's own, its dominant control and its identity.
    fn focus_areas(&self, cx: &App) -> Vec<(FocusArea, gpui_kit::FocusHandle)> {
        let mut areas = vec![
            (FocusArea::Sidebar, self.sidebar_focus.clone()),
            (FocusArea::Transcript, self.transcript_focus.clone()),
            (FocusArea::Composer, self.input.read(cx).focus_handle(cx)),
        ];
        if self.panel != Panel::None {
            areas.push((FocusArea::SidePanel, self.side_focus.clone()));
        }
        areas
    }

    /// The area (if any) that currently holds focus, a container or
    /// any control inside it counts.
    fn focused_area(
        &self,
        areas: &[(FocusArea, gpui_kit::FocusHandle)],
        window: &Window,
        cx: &App,
    ) -> Option<usize> {
        areas
            .iter()
            .position(|(_, handle)| handle.contains_focused(window, cx))
    }

    /// F6 / Shift+F6: move focus between the surface's areas, wrapping
    /// at the ends. The control that held focus inside an area is
    /// remembered when the cycle leaves it and restored when it comes
    /// back (a stale restore, a control since unmounted, falls back
    /// to the area's container; the next press self-heals from
    /// wherever focus really sits). A modal on screen (palette, turn
    /// diff) owns focus exclusively; the cycle stands down.
    fn cycle_focus(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.turn_diff.is_some() || self.palette_open {
            return;
        }
        let areas = self.focus_areas(cx);
        let current = self.focused_area(&areas, window, cx);
        // Leaving: remember the exact control.
        if let Some(index) = current
            && let Some(focused) = window.focused(cx)
        {
            self.focus_memory.insert(areas[index].0, focused);
        }
        let next = match current {
            Some(index) => {
                if backwards {
                    (index + areas.len() - 1) % areas.len()
                } else {
                    (index + 1) % areas.len()
                }
            }
            // Focus sits nowhere the cycle knows: forward enters at
            // the sidebar, backward at the last area.
            None if backwards => areas.len() - 1,
            None => 0,
        };
        let (area, container) = &areas[next];
        let remembered = self.focus_memory.get(area).cloned();
        match remembered {
            Some(handle) if !handle.is_focused(window) => {
                window.focus(&handle, cx);
                // A stale restore leaves focus where it was, fall
                // back to the container so the cycle always lands.
                if !handle.is_focused(window) {
                    window.focus(container, cx);
                }
            }
            _ => window.focus(container, cx),
        }
        cx.notify();
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
        // Runs of finished tool calls collapse into one honest summary
        // (the counts are the cards', failures stay counted); a hidden
        // member renders nothing, the list's index space is the
        // turns'.
        let run = self.tool_run_at(index);
        if run == 0 {
            return div().w_full().into_any_element();
        }
        if run > 1 {
            return self.render_tool_run(index, run, window, cx);
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
            Turn::Message { role, text, .. } => self.render_message(role, text, index as u64, cx),
            Turn::Tool {
                call,
                status,
                auto_approved,
                worker,
                ..
            } => self.render_tool(index, call, status, *auto_approved, *worker, window, cx),
            Turn::LoadEarlier => {
                // The pagination row: one quiet button, centered,
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
        // Fork affordance: persisted user turns only, "branch from this
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
        // The rewind affordance: the same persisted user turns,
        // "edit from here" truncates this conversation back to the
        // question, which returns to the composer as a draft.
        let rewind_button = match &self.turns[index] {
            Turn::Message {
                role: Role::User,
                db_id: Some(_),
                ..
            } => Some(
                Button::new(format!("rewind-{index}"))
                    .label("rewind")
                    .small()
                    .ghost()
                    .tooltip(
                        "Edit from here, the conversation after this message leaves the transcript",
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.begin_rewind(index, cx);
                    })),
            ),
            _ => None,
        };
        // The quote affordance: an assistant turn worth commenting on,
        // dropped into the composer as a blockquote (the citation is
        // exact, the agent reads back the same words you sent).
        let quote_button = match &self.turns[index] {
            Turn::Message {
                role: Role::Assistant,
                text,
                ..
            } if !text.trim().is_empty() => {
                let text: std::sync::Arc<str> = text.as_str().into();
                let copy_text = text.clone();
                Some(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new(format!("copy-{index}"))
                                .label("copy")
                                .small()
                                .ghost()
                                .tooltip("Copy the raw markdown of this turn")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.copy_turn_to_clipboard(copy_text.clone(), cx);
                                })),
                        )
                        .child(
                            Button::new(format!("quote-{index}"))
                                .label("quote")
                                .small()
                                .ghost()
                                .tooltip(
                                    "Quote this into the composer, comment under it, send it back",
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.quote_into_composer(text.clone(), window, cx);
                                })),
                        ),
                )
            }
            _ => None,
        };
        // One affordance row under the bubble. Visibility rule: user
        // turns show fork/rewind persistently (they mark the
        // conversation's decision points; hide-behind-hover made an
        // old turn's rewind undiscoverable), assistant turns
        // hover-reveal copy/quote (per-reading conveniences, not
        // landmarks) via the group idiom. Tests click both kinds: a
        // hover-revealed row stays clickable for the harness
        // (invisible is not display:none) but an explicit
        // visibility assertion would need a hover first.
        let user_turn = fork_button.is_some();
        let mut buttons: Vec<AnyElement> = Vec::new();
        if let Some(button) = fork_button {
            buttons.push(button.into_any_element());
        }
        if let Some(button) = rewind_button {
            buttons.push(button.into_any_element());
        }
        if let Some(button) = quote_button {
            buttons.push(button.into_any_element());
        }
        let inner = if buttons.is_empty() {
            inner
        } else {
            div()
                .w_full()
                .id(format!("turn-{index}"))
                .test_support()
                .group(format!("turn-{index}"))
                .flex()
                .flex_col()
                .when(user_turn, |col| col.items_end())
                .when(!user_turn, |col| col.items_start())
                .child(inner)
                .child(
                    // Assistant rows are ghosted rather than hidden:
                    // 35% opacity at rest, full on the turn's hover.
                    // Visibility-hidden made an old turn's affordances
                    // undiscoverable (you must know to hover) and the
                    // harness's synthetic hover can't drive gpui's
                    // group-hover state; ghosting keeps the row
                    // present (and clickable) while clearly chrome.
                    h_flex()
                        .gap_1()
                        .when(!user_turn, |row| row.opacity(0.35))
                        .when(!user_turn, |row| {
                            row.group_hover(format!("turn-{index}"), |style| style.opacity(1.0))
                        })
                        .children(buttons),
                )
                .into_any_element()
        };
        // Horizontal padding lives on each item, not the list: gpui's
        // `list` paints items at the container's left edge and ignores
        // `px_*` on the list element, so without this the transcript
        // butts against the window border. The find bar's current
        // match marks its row: which row "3/7" refers to is the
        // highlight's whole job (a span-level marker inside the
        // markdown is the deferred bigger version).
        let current_match = self
            .find
            .as_ref()
            .and_then(|(query, position)| {
                let matches = self.find_matches(query);
                matches.get(*position).copied()
            })
            .map(|row| row == index)
            .unwrap_or(false);
        let inner = if current_match {
            // The types differ (Div vs AnyElement); going through
            // any_element makes both arms one type.
            div()
                .w_full()
                .rounded_sm()
                .bg(cx.theme().secondary)
                .child(inner)
                .into_any_element()
        } else {
            inner.into_any_element()
        };
        // Turn rhythm: the boundary between turns must read stronger
        // than the paragraph gaps inside a turn, or the transcript
        // reads as one continuous column (the screenshot showed the
        // inversion). Markdown paragraphs carry mt_2; a turn carries
        // the turn's own bottom padding plus this margin.
        div().w_full().px_4().mt_2().child(inner).into_any_element()
    }

    /// The status line's words, spinner aside: the action, the step's
    /// own elapsed clock, and, past [`QUIET_AFTER`] of no loop events,
    /// the quiet gauge. The clock is the step's, not the run's: it
    /// starts over when the activity changes, so the number answers
    /// "how long has THIS been going on", "is it stuck or just slow",
    /// the question the words alone answer too late. The run's total
    /// lives in the footer. Quiet stays silent while an approval card
    /// waits on the human: that silence is the human's, and the ask's
    /// own clock tells it.
    fn activity_line(&self) -> String {
        let mut line = format!("{} · {}", self.activity, clock(self.activity_at.elapsed()));
        let quiet = self.last_event_at.elapsed();
        if quiet >= QUIET_AFTER && !self.waiting_on_human() {
            line.push_str(&format!(" · quiet {}", clock(quiet)));
        }
        line
    }

    /// The empty transcript's center. Three honest variants, quiet
    /// to loud: a configured endpoint gets the two-line "Ready." state;
    /// a fresh install (no chats ever, this is the sidebar's test)
    /// gets the one-minute tour with its two actions; a missing
    /// endpoint says plainly what's missing and offers the fix. No
    /// wizard: three lines, two buttons, done.
    fn render_empty_state(&self, cx: &Context<Self>) -> AnyElement {
        let no_endpoint = self.endpoint_label.starts_with("no endpoint configured");
        let fresh = self.sessions.is_empty();
        let headline: &str = if no_endpoint {
            "Ready to run, once an endpoint exists."
        } else {
            "Ready."
        };
        let subline: &str = if no_endpoint {
            "Gertie runs local and API models; add one endpoint and it works."
        } else {
            "Ask, paste, or point me at a file."
        };
        v_flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(cx.theme().foreground)
                    .child(headline),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child(subline),
            )
            .when(no_endpoint, |state| {
                state.child(
                    Button::new("empty-state-configure")
                        .label("Add an endpoint")
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_settings(cx))),
                )
            })
            .when(fresh && !no_endpoint, |state| {
                state.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(cx.theme().muted_foreground)
                        .pt_1()
                        .child("Ctrl+K finds anything you've ever asked; memory remembers what matters."),
                )
            })
            .into_any_element()
    }

    /// The quiet-state row: what's happening now, the spinner the
    /// no-animation doctrine's one carve-out, time-derived so it
    /// needs no state of its own: it steps at 2 fps on the working
    /// tick (which only runs while a turn does, nothing animates at
    /// rest), because "is it stuck or just slow" is a question the
    /// words alone answer too late.
    fn render_activity_row(&self, cx: &Context<Self>) -> AnyElement {
        let frame = SPINNER_FRAMES
            [(self.activity_at.elapsed().as_millis() as usize / 500) % SPINNER_FRAMES.len()];
        let line = self.activity_line();
        h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    .child(frame),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(cx.theme().muted_foreground)
                    // Activity strings carry `code` spans (tool labels
                    // quote paths and commands), same treatment as
                    // the cards.
                    .child(crate::markdown::inline_element(&line, cx)),
            )
            .into_any_element()
    }

    /// The footer line under the composer. Left: this session's spend,
    /// the run's total while a run is live, and the live context
    /// gauge, `12.3k (23%)`, so the distance to the compaction
    /// threshold is visible before it happens. Right: what you're
    /// talking to, `gertie · ppq · glm-5.3-flash`, or, when the chat
    /// is blocked on you, that fact in warning color. Quiet until
    /// there is something to say.
    fn render_token_footer(&self, status: Option<&str>, cx: &Context<Self>) -> AnyElement {
        let parts = self.footer_parts();
        let near_compaction = self
            .context_window
            .is_some_and(|window| self.context_used * 100 >= window.max(1) * 75);
        // The permission posture leads the quiet line: a word, not a
        // chip, click-to-cycle with one asymmetry: the ring is
        // de-escalation-only, full access costs a palette command
        // plus a confirm card (M7 PR 1). The tooltip names the
        // current mode's meaning, read before the div builds.
        let mode_text = mode_tooltip(self.mode);
        // The posture's color is its weight: quiet gray for the modes
        // where gertie asks, warning for the mode that removes every
        // guardrail. The loudest state must not render like the rest.
        let mode_color = match self.mode {
            PermissionMode::FullAccess => cx.theme().warning,
            _ => cx.theme().muted_foreground,
        };
        let mode = div()
            .id("mode-chip")
            .cursor_pointer()
            .text_size(px(11.))
            .text_color(mode_color)
            .hover(|style| style.text_color(cx.theme().foreground))
            .tooltip(move |window, cx| Tooltip::new(mode_text).build(window, cx))
            .on_click(cx.listener(|this, _, _, cx| this.cycle_mode(cx)))
            .child(mode_label(self.mode))
            // Observed so the test harness can click the word; a
            // no-op outside test-support builds.
            .test_support();
        // The memory word: how much this workspace remembers. Shown
        // when the store holds *entries*; when it only holds files
        // (prose notes, not yet indexed entries) the chip still shows,
        // with the file count: an existing store with a blank chip
        // reads as "memory does not exist", and the chip is the door
        // to the panel either way. Truly empty stores hide it (a
        // memoryless session pays zero attention).
        let memory = self.memory_store.as_ref().map(|store| {
            let entries = store.scan().len();
            let files = store.topic_file_count();
            if entries > 0 {
                format!("memory {entries}")
            } else if files > 0 {
                format!("memory ({files} files)")
            } else {
                String::new()
            }
        });
        let memory = memory.filter(|label| !label.is_empty()).map(|label| {
            let tooltip =
                "memory: what gertie recalls in this repo and for you; click to read and vote.";
            div()
                .id("memory-chip")
                .cursor_pointer()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .hover(|style| style.text_color(cx.theme().foreground))
                .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_memory(cx)))
                .child(label)
                .test_support()
        });
        let mut ledger = h_flex().gap_2().child(mode);
        if let Some(memory) = memory {
            ledger = ledger.child(memory);
        }
        // The run's total, the one clock that answers "how long has
        // this piece of work taken me" (pauses included, the wait is
        // part of the work): it lives here, where the counts live,
        // not taped to a status line that changes every few seconds.
        if self.run_started.is_some() {
            ledger = ledger.child(
                div()
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("run {}", clock(self.run_elapsed_total()))),
            );
        }
        if !parts.is_empty() {
            // The ledger line in two groups with a real gap: what the
            // work costs (spend, cache share) reads separately from
            // where the context stands (the gauge). One joined string
            // made three kinds of information read as one.
            let mut gauge = parts.into_iter();
            let spend: Vec<String> = gauge
                .by_ref()
                .take_while(|part| !part.starts_with("ctx "))
                .collect();
            ledger = ledger
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(cx.theme().muted_foreground)
                        .child(spend.join("  ·  ")),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(cx.theme().muted_foreground)
                        .opacity(0.6)
                        .child("│"),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(if near_compaction {
                            cx.theme().warning
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(gauge.collect::<Vec<_>>().join("  ·  ")),
                );
        }
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
                // else, thinking, writing, tool work, lives in the
                // transcript, where the user is looking anyway. At
                // rest, the identity line leads with the folder the
                // agent's tools run in, "which project am I talking
                // about" is answered without opening anything.
                Some(status) => div().child(status.to_string()),
                None => {
                    // The word is the switch: click it, pick a folder,
                    // the agent moves. A worktree tab's word stays
                    // text, its root is the checkout it was opened
                    // for, and the move would break the tab's
                    // branch-rename promise.
                    let word: AnyElement = if self.branch_naming.is_none() {
                        div()
                            .id("workspace-word")
                            .cursor_pointer()
                            .hover(|style| style.text_color(cx.theme().foreground))
                            .tooltip(|window, cx| {
                                Tooltip::new("Workspace, click to move the agent to another folder")
                                    .build(window, cx)
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.switch_workspace(cx)))
                            .child(tildify(&self.workspace_root))
                            .test_support()
                            .into_any_element()
                    } else {
                        div()
                            .child(tildify(&self.workspace_root))
                            .into_any_element()
                    };
                    h_flex()
                        .min_w_0()
                        .gap_2()
                        .justify_end()
                        .child(word)
                        .child("·")
                        .child(div().truncate().child(self.endpoint_label.clone()))
                }
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
                cx.listener(|this, _, window, cx| this.close_palette(window, cx)),
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
                                    .items(if self.palette_commands.is_empty() {
                                        crate::palette::search_items(&self.palette_hits)
                                    } else {
                                        crate::palette::command_items(&self.palette_commands)
                                    })
                                    .filterable(false)
                                    .placeholder("Search your history, > for commands…")
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
                                    .on_confirm(move |path, window, cx| {
                                        jump_view.update(cx, |chat, cx| {
                                            chat.palette_confirm(path.row, window, cx)
                                        });
                                    })
                                    .on_cancel(move |window, cx| {
                                        cancel_view
                                            .update(cx, |chat, cx| chat.close_palette(window, cx));
                                    }),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_message(&self, role: &Role, text: &str, key: u64, cx: &App) -> AnyElement {
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
                        .bg(cx.theme().secondary)
                        .border_1()
                        .border_color(cx.theme().border)
                        .text_color(cx.theme().foreground)
                        .child(display_text(text)),
                )
                .into_any_element(),
            Role::System => {
                // Loop notices (compaction): quiet, left on the same
                // margin as everything else, not a chat message from
                // anyone and not a billboard either.
                div()
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
                // Full-width prose: the reading cap shipped once and
                // didn't survive contact with the workbench (the
                // article measure cramped wide windows); a prior
                // "reverted" claim was never actually shipped.
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
                        // Error text is for the human, verbatim, don't
                        // run it through the markdown parser.
                        row.child(text.to_string())
                    })
                    .when(!is_error, |row| {
                        // The memoized parse: settled rows parse once,
                        // the streaming row once per throttled frame.
                        row.child(self.markdown_cache.borrow_mut().render(key, text, cx))
                    })
                    .into_any_element()
            }
            Role::Tool => div().into_any_element(),
        }
    }

    /// One tool row: a friendly statement of what the model is doing
    /// (not the wire name), the diff for file changes, a one-line
    /// outcome when finished, and approve/deny while pending. The raw
    /// tool output is one click away; it's the exception, not the face.
    #[allow(clippy::too_many_arguments)]
    fn render_tool(
        &self,
        index: usize,
        call: &ToolCall,
        status: &ToolStatus,
        _auto_approved: bool,
        worker: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let running = matches!(status, ToolStatus::Running);
        // Label and diff data parse once per card (see ToolFace); the
        // elements rebuild per frame from the memoized data.
        let (label, diff) = {
            let mut cache = self.tool_face.borrow_mut();
            let face = cache.entry(index).or_insert_with(|| {
                // The write's before-content, three sources in honesty
                // order: the pre-run capture when this session saw the
                // request land, the checkpoint the loop recorded when
                // the call was approved (a reloaded card's exact
                // source), else the file as it sits now.
                let before = if call.name == "write_file" {
                    self.before_contents
                        .get(&call.id)
                        .cloned()
                        .or_else(|| self.checkpointed_before(call))
                        .or_else(|| self.current_file_content(call))
                } else {
                    None
                };
                ToolFace {
                    label_running: crate::tools_ui::action_label(&call.name, &call.arguments, true),
                    label_done: crate::tools_ui::action_label(&call.name, &call.arguments, false),
                    diff: diff_data(call, before.as_deref()),
                }
            });
            (
                if running {
                    face.label_running.clone()
                } else {
                    face.label_done.clone()
                },
                face.diff.clone(),
            )
        };

        let mut inner = v_flex().gap_2().child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(cx.theme().accent)
                        // Labels carry `code` spans (paths, commands),
                        // through the inline renderer they become chips
                        // instead of literal backticks.
                        .child(crate::markdown::inline_element(&label, cx)),
                )
                // The running tag carries the doctrine's one carve-out
                // too: a spinner stepping on the working tick and the
                // call's own clock, a two-minute test run says how
                // long it has been saying nothing.
                .when(running, |row| {
                    let started = self.tool_starts.get(&call.id);
                    let step = started
                        .map(|clock_at| clock_at.elapsed().as_millis() as usize / 500)
                        .unwrap_or(0);
                    let frame = SPINNER_FRAMES[step % SPINNER_FRAMES.len()];
                    let words = started
                        .map(|clock_at| format!("running · {}", clock(clock_at.elapsed())))
                        .unwrap_or_else(|| "running".to_string());
                    row.child(
                        div()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{frame} {words}")),
                    )
                })
                .when(worker, |row| {
                    // A worker run's own elapsed time, next to the
                    // badge: the run clock answers "how long has the
                    // worker's work taken", distinct from the leader's
                    // turn clock and the tool's own card clock.
                    let worker_clock = self
                        .tool_starts
                        .get(&call.id)
                        .map(|started| clock(started.elapsed()))
                        .unwrap_or_default();
                    row.child(
                        div()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(if worker_clock.is_empty() {
                                "worker".to_string()
                            } else {
                                format!("worker · {worker_clock}")
                            }),
                    )
                }),
        );

        // The structured question: the face is the question text, the
        // pending actions are the options themselves (clicking one
        // answers the ask; Deny dismisses; the reply field is the
        // free-text path when the options miss). The options are the
        // model's words, sanitized at the loop's parse, before the
        // card ever sees them.
        if call.name == "ask_user"
            && let Some(ask) = ask_face(call)
        {
            inner =
                inner
                    .child(
                        div()
                            .text_size(px(13.))
                            .child(SharedString::from(ask.question)),
                    )
                    .children(ask.options.iter().cloned().enumerate().map(
                        |(option_index, option)| {
                            Button::new(SharedString::from(format!("ask-{index}-{option_index}")))
                                .label(option.clone())
                                .small()
                                .outline()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.decide_tool(
                                        index,
                                        PermissionDecision::Deny {
                                            feedback: Some(option.clone()),
                                        },
                                        cx,
                                    )
                                }))
                        },
                    ));
            if ask.allow_custom {
                // The input exists only if the user focused the
                // field once; an ask answered by its buttons never
                // needs it. The render pass creates it on the
                // next frame after focus via the same map the
                // mutating cards use (see approval_reply_for).
                let reply = self.approval_replies.get(&index);
                inner = inner.children(reply.map(|reply| {
                    h_flex()
                        .gap_2()
                        .child(Input::new(reply).small().flex_1())
                        .child(
                            Button::new(SharedString::from(format!("ask-send-{index}")))
                                .label("Send")
                                .small()
                                .primary()
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.reply_tool(index, cx)),
                                ),
                        )
                }));
            }
            inner = inner.child(
                Button::new(SharedString::from(format!("deny-{index}")))
                    .label("Dismiss")
                    .small()
                    .ghost()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.decide_tool(index, PermissionDecision::Approve, cx)
                    })),
            );
            return v_flex()
                .w_full()
                .pr_8()
                .pb_2()
                .child(ask_group(inner, cx))
                .into_any_element();
        }

        let face = match status {
            ToolStatus::Pending(_) => {
                let reply = self.approval_replies.get(&index);
                inner = inner
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().warning)
                            .child(if call.name == "bash" {
                                "This runs a shell command, your OK first."
                            } else {
                                "This changes files in the workspace, your OK first."
                            }),
                    )
                    .children(diff.as_ref().map(|diff| diff_element(diff, cx)))
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
                    // AND goes back to the model, "no, use the other
                    // file" becomes a correction, not a dead end.
                    .children(reply.map(|reply| {
                        h_flex()
                            .w_full()
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
                ask_group(inner, cx)
            }
            ToolStatus::Running => {
                // The label and the spinner say it all; a second
                // "running" line says nothing new, and the old
                // "read-only, so it runs without asking" was worse
                // than noise, it lied for an auto-approved write.
                inner.into_any_element()
            }
            ToolStatus::Done(result) => {
                let failed = result.starts_with("error:")
                    || result.starts_with("exit ")
                    || result.starts_with("blocked by policy")
                    || result.ends_with("denied by user");
                // The diff tells the story for file changes, but never
                // for a call that didn't happen (denied or failed).
                if !failed && let Some(diff) = &diff {
                    inner = inner.child(diff_element(diff, cx));
                }
                let summary = crate::tools_ui::result_summary(&call.name, result);
                let line_count = result.lines().count();
                let expanded = self.expanded_tools.contains(&index);

                // Default face: the one-line outcome. The raw result,
                // file contents, match dumps, exit noise, is what the
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
                if expanded && line_count > 1 {
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
                // The affordances share one quiet row: both are
                // furniture, not content, and a centered pill per
                // link made the card read as chrome.
                let mut affordances = h_flex().gap_2();
                if line_count > 1 {
                    let (label, tooltip) = if expanded {
                        (
                            "hide raw output".to_string(),
                            "Collapse the raw tool output".to_string(),
                        )
                    } else {
                        (
                            format!("show raw output ({} lines)", line_count),
                            "What the model actually received".to_string(),
                        )
                    };
                    affordances = affordances.child(
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
                // the ledger answers "what did the tools do", the
                // agent's work, not git's.
                if self.snapshots.is_some() {
                    affordances = affordances.child(
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
                inner = inner.child(affordances);
                inner.into_any_element()
            }
        };

        v_flex()
            .w_full()
            .pr_8()
            .pb_2()
            .child(face)
            .into_any_element()
    }
}

/// Which right-column panel is open, if any: settings alone now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Panel {
    #[default]
    None,
    Settings,
    /// The memory surface: both tiers, entries as rows, voting. A
    /// read-and-vote view; editing stays the files' job (M7 PR 2).
    Memory,
}

/// Bubbles out of the chat: a settings save landed. The workspace
/// (which owns the bootstrap) reloads and fans the result to every tab.
pub enum ChatViewEvent {
    ConfigSaved,
}

impl EventEmitter<ChatViewEvent> for ChatView {}

impl ChatView {
    /// Applies a fresh bootstrap snapshot, what a settings save
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

/// The waiting-on-you group's face: a left accent edge, not a box.
/// An approval is the transcript's landmark while it waits, and the
/// mark should say "this needs you", not "here is a panel". The
/// content stays uncrated: the row, the diff, the buttons.
fn ask_group(inner: impl IntoElement, cx: &App) -> AnyElement {
    v_flex()
        .gap_2()
        .mt_1()
        .pl_3()
        .border_l_2()
        .border_color(cx.theme().warning)
        .child(inner)
        .into_any_element()
}

/// Where a stored tool path lives on disk: absolute as-is, relative
/// against the workspace root, the same rule the loop's checkpoint
/// hook applies before snapshotting.
fn resolve_tool_path(root: &std::path::Path, path: &std::path::Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

impl ChatView {
    /// What the workspace file holds right now, for a write card's
    /// before-content. The exact sources are the pre-run capture taken
    /// when the call arrived (`before_contents`) and, for a reloaded
    /// card, the checkpoint the loop recorded for the call; this disk
    /// read is the last fallback, for a card with neither, and it can
    /// only guess: the file has usually moved on.
    fn current_file_content(&self, call: &ToolCall) -> Option<String> {
        if call.name != "write_file" {
            return None;
        }
        let value: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
        let path = value.get("path")?.as_str()?;
        let resolved = resolve_tool_path(&self.workspace_root, std::path::Path::new(path));
        std::fs::read_to_string(resolved).ok()
    }

    /// The ledger's before-content for a write card: the checkpoint
    /// the loop recorded the moment this very call was approved,
    /// keyed by the call's own id. The exact source for a reloaded
    /// card, where the live capture is long gone and the file has
    /// usually moved on. Legacy ledgers (rows older than call ids)
    /// and calls that never ran answer None, the disk fallback
    /// follows.
    fn checkpointed_before(&self, call: &ToolCall) -> Option<String> {
        let store = self.snapshots.as_ref()?;
        let session_id = self.session_id?;
        let bytes = store.before_content_of_call(session_id, &call.id).ok()??;
        String::from_utf8(bytes).ok()
    }
}

/// The structured question's card data, unpacked from the synthetic
/// call the loop built. The loop already sanitized every string; this
/// only unpacks. Pure: the card cache holds it, elements rebuild per
/// frame.
fn ask_face(call: &ToolCall) -> Option<AskFace> {
    let value: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
    let question = value.get("question")?.as_str()?.to_string();
    let options = value
        .get("options")
        .and_then(|options| options.as_array())
        .map(|options| {
            options
                .iter()
                .filter_map(|option| option.as_str().map(str::to_string))
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();
    Some(AskFace {
        question,
        options,
        allow_custom: value
            .get("allow_custom")
            .and_then(|flag| flag.as_bool())
            .unwrap_or(false),
    })
}

struct AskFace {
    question: String,
    options: Vec<String>,
    allow_custom: bool,
}

/// Context lines a card's diff keeps on each side of a change when it
/// trims long stretches of unchanged lines.
const HUNK_CONTEXT: usize = 3;

/// One display row of a card's diff: a line, or an elision where a run
/// of unchanged lines was trimmed.
#[derive(Debug)]
enum DiffRow<'a> {
    Line(&'a crate::diff::Line),
    Elided,
}

/// Trims runs of unchanged lines to [`HUNK_CONTEXT`] beside each
/// change. Every added or removed line stays; only context collapses,
/// and the counts above the diff were whole before this ran, so the
/// trim costs nothing honest.
fn hunk_rows(lines: &[crate::diff::Line]) -> Vec<DiffRow<'_>> {
    let kept: Vec<bool> = lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            if !matches!(line, crate::diff::Line::Same(_)) {
                return true;
            }
            let window = &lines
                [index.saturating_sub(HUNK_CONTEXT)..(index + HUNK_CONTEXT + 1).min(lines.len())];
            window
                .iter()
                .any(|neighbor| !matches!(neighbor, crate::diff::Line::Same(_)))
        })
        .collect();
    let mut rows = Vec::new();
    let mut eliding = false;
    for (index, line) in lines.iter().enumerate() {
        if kept[index] {
            eliding = false;
            rows.push(DiffRow::Line(line));
        } else if !eliding {
            eliding = true;
            rows.push(DiffRow::Elided);
        }
    }
    rows
}

/// The diff's data, a real line patch: longest-common-subsequence
/// with context, capped, the counts whole regardless of the cap.
/// `before` is the file's pre-write content for write cards (edit
/// cards carry both sides in their arguments). Pure: the card cache
/// holds this; element construction (`diff_element`) rebuilds per
/// frame from it.
fn diff_data(call: &ToolCall, before: Option<&str>) -> Option<crate::diff::Patch> {
    let value: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
    match call.name.as_str() {
        "edit_file" => {
            let old = value.get("old_text")?.as_str()?;
            let new = value
                .get("new_text")
                .and_then(|text| text.as_str())
                .unwrap_or("");
            Some(crate::diff::lines(old, new))
        }
        "write_file" => Some(crate::diff::lines(
            before.unwrap_or(""),
            value.get("content")?.as_str()?,
        )),
        _ => None,
    }
}

/// The card's diff face, reading exactly like the turn-diff modal's
/// rows (same marks, same colors, mono): the counts lead, the hunks
/// follow with context trimmed around the changes, and a tail note
/// names what the display cap dropped. No box: the diff hangs under
/// its row, indented, the way a diff belongs to the line that
/// caused it.
fn diff_element(patch: &crate::diff::Patch, cx: &App) -> AnyElement {
    let mut container = v_flex().pl_3().py_1().overflow_hidden();

    // The counts lead: what changed, before any line of it.
    container = container.child(
        h_flex()
            .gap_2()
            .text_size(px(11.))
            .child(
                div()
                    .text_color(cx.theme().success)
                    .child(format!("+{}", patch.adds)),
            )
            .child(
                div()
                    .text_color(cx.theme().danger)
                    .child(format!("\u{2212}{}", patch.removes)),
            ),
    );

    if patch.adds + patch.removes == 0 {
        return container
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child("unchanged"),
            )
            .into_any_element();
    }

    for row in hunk_rows(&patch.lines) {
        let element = match row {
            DiffRow::Elided => div()
                .font_family(cx.theme().mono_font_family.clone())
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child("\u{22ef}")
                .into_any_element(),
            DiffRow::Line(line) => {
                let (text, color) = match line {
                    crate::diff::Line::Same(text) => {
                        (format!("  {text}"), cx.theme().muted_foreground)
                    }
                    crate::diff::Line::Add(text) => (format!("+ {text}"), cx.theme().success),
                    crate::diff::Line::Remove(text) => {
                        (format!("\u{2212} {text}"), cx.theme().danger)
                    }
                };
                div()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_size(px(11.))
                    .text_color(color)
                    .child(text)
                    .into_any_element()
            }
        };
        container = container.child(element);
    }

    if patch.omitted > 0 {
        container = container.child(
            div()
                .text_size(px(11.))
                .text_color(cx.theme().muted_foreground)
                .child(format!("\u{2026} {} more lines", patch.omitted)),
        );
    }
    container.into_any_element()
}

impl ChatView {
    /// The sessions the UI may show: everything in the store except
    /// rows inside their undo window, ✕ already answered, the purge
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

    /// Whether the open session's last turn ended on a rate limit,
    /// the resume bar's condition. Test-visible: the limited state is
    /// the §2.3 contract.
    /// Whether this tab finished unseen work: a turn just ended
    /// (streaming flipped off) and the tab hasn't been focused since.
    /// The tab bar's done-unseen dot reads this; the workspace clears
    /// it on activation. Tracked by the caller (it owns the previous
    /// streaming state), this is only the completion probe.
    pub fn turn_just_finished(&self, prev_streaming: bool) -> bool {
        prev_streaming && !self.streaming
    }

    pub fn open_session_limited(&self) -> bool {
        self.session_id
            .and_then(|id| self.sessions.iter().find(|summary| summary.id == id))
            .is_some_and(|summary| summary.limited)
    }

    /// The sidebar's active rows: every session not yet settled, in the
    /// store's attention order (most recent activity first). The
    /// caller's `visible_sessions` snapshot is shared: one clone per
    /// render feeds the rows, the fold's guard, and its count.
    fn active_session_rows(
        &self,
        sessions: &[gertie_core::SessionSummary],
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        sessions
            .iter()
            .filter(|summary| !summary.settled)
            .map(|summary| self.session_row(summary, cx))
            .collect()
    }

    /// The fold's rows: settled work, muted, restore affordance up front.
    fn settled_session_rows(
        &self,
        sessions: &[gertie_core::SessionSummary],
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        sessions
            .iter()
            .filter(|summary| summary.settled)
            .map(|summary| self.session_row(summary, cx))
            .collect()
    }

    /// One sidebar row, active or settled; the settled variant renders
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
            .tooltip("Delete this conversation, five seconds to undo");
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
                        // The open conversation is tinted, not loud, the
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
            // provider's rate limit. Static text, warning tint, the
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
                        "Un-settle, back to the active list"
                    } else {
                        "Settle, done; folds out of the list"
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
        // The composer's slash and mention matches, computed once per
        // frame: the popups' guards and rows share the one computation.
        let value = self.input.read(cx).value().to_string();
        let slash = slash_matches(&value, &self.templates);
        let mentions = if slash.is_empty() {
            mention_matches(&value, &self.workspace_root)
        } else {
            Vec::new()
        };
        let selected_popup_index = self.popup_selection.as_ref().map(|(index, _)| *index);
        // The sidebar's session snapshot, same contract: one clone
        // feeds the rows, the fold's guard, and its count.
        let visible = self.visible_sessions();
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
            // The turnRunning `when` context, present only while a
            // turn drives, so keymap rules can gate on it.
            .when(self.streaming, |root| root.key_context("TurnRunning"))
            // The palette's trigger fires from wherever focus sits,
            // the root catches it on the way up the dispatch path.
            .on_action(cx.listener(
                |this, _: &crate::palette::OpenPalette, window, cx| {
                    this.open_palette(window, cx);
                },
            ))
            // Prompt recall: ArrowUp/Down in an empty composer walks
            // the session's sent prompts (see recall_step).
            .on_action(cx.listener(|this, _: &RecallUp, window, cx| {
                this.recall_step(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &RecallDown, window, cx| {
                this.recall_step(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &FindInTranscript, window, cx| {
                this.open_find(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FindClose, _, cx| {
                this.close_find(cx);
            }))
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
            // F6 / Shift+F6: the focus cycle between areas.
            .on_action(cx.listener(|this, _: &CycleFocus, window, cx| {
                this.cycle_focus(false, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CycleFocusBack, window, cx| {
                this.cycle_focus(true, window, cx);
            }))
            // h_flex centers children on the cross axis by default,
            // stretch instead, so the main row fills the window height
            // and the transcript list keeps its flex sizing.
            .items_stretch()
            .relative()
            .child(
                // Session sidebar: sessions scroll, the utility
                // buttons stay put at the foot. A focus area (F6):
                // the container tracks focus so the cycle can find it.
                v_flex()
                    .w(px(240.))
                    .h_full()
                    .flex_shrink_0()
                    .key_context("Sidebar")
                    .track_focus(&self.sidebar_focus)
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
                            .children(self.active_session_rows(&visible, cx))
                            // The settled fold: put-down work, collapsed
                            // by default, findable, never in the way.
                            .when(
                                visible.iter().any(|summary| summary.settled),
                                |col| {
                                    col.child(
                                        Button::new("settled-fold")
                                            .label(format!(
                                                "{} SETTLED ({})",
                                                if self.settled_open { "▾" } else { "▸" },
                                                visible
                                                    .iter()
                                                    .filter(|summary| summary.settled)
                                                    .count()
                                            ))
                                            .xsmall()
                                            .ghost()
                                            .tooltip("Conversations put down, expand to bring one back")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.settled_open = !this.settled_open;
                                                cx.notify();
                                            })),
                                    )
                                    .when(self.settled_open, |col| {
                                        col.children(self.settled_session_rows(&visible, cx))
                                    })
                                },
                            ),
                    )
                    .child(
                        // The utilities: stacked at the
                        // sidebar's foot, findable, never
                        // in the way.
                        v_flex()
                            .gap_1()
                            .px_3()
                            .pt_2()
                            .pb_3()
                            // The undo toast: sessions inside their
                            // five-second window, newest first. One
                            // line, one button, the whole mechanism.
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
                                    .label("Search (Ctrl+K)")
                                    .small()
                                    .ghost()
                                    .w_full()
                                    .tooltip("Search every session, Ctrl+K")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_palette(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("toggle-settings")
                                    .label("Settings")
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
                        // The transcript: horizontal padding is
                        // per-item (see render_turn), gpui's list
                        // paints items at the container edge and would
                        // ignore px_* here. The pt/pb DO apply.
                        v_flex()
                            .flex_1()
                            .min_h_0()
                            .relative()
                            .child(
                                // The transcript area's container wraps
                                // exactly the list, not the composer
                                // chrome below it: F6's detection walks
                                // ancestors, and a container over both
                                // would read a focused composer as
                                // transcript. The wrapper carries the
                                // flex sizing; the list fills it.
                                div()
                                    .id("transcript-area")
                                    .flex_1()
                                    .min_h_0()
                                    .key_context("Transcript")
                                    .track_focus(&self.transcript_focus)
                                    .child(
                                        list(self.list_state.clone(), move |index, window, cx| {
                                            view.update(cx, |view, cx| {
                                                view.render_turn(index, window, cx)
                                            })
                                            .unwrap_or_else(|_| div().into_any_element())
                                        })
                                        .h_full()
                                        .overflow_hidden()
                                        .pt_3()
                                        .pb_4(),
                                    ),
                            )
                            // Empty state: the transcript is quiet until
                            // it isn't, say so, quietly. The state says
                            // more when a first-run needs to know it
                            // (M7 PR 3): no endpoint is the one blocking
                            // fact, a fresh install gets the one-minute
                            // tour.
                            .when(self.turns.is_empty(), |col| {
                                col.child(
                                    div()
                                        .absolute()
                                        .inset_0()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(self.render_empty_state(cx)),
                                )
                            })
                            .child(
                                // Composer + footer: the input and the
                                // token ledger live under the
                                // conversation, the sidebar owns the
                                // full window height, so nothing here
                                // spans the window.
                                v_flex()
                                    .border_t_1()
                                    .border_color(cx.theme().border)
                                    .when(self.find.is_some(), |composer| {
                                        // The find bar: query, match
                                        // count, walk, close. Focus sits
                                        // here while open; Escape (the
                                        // find-close rule, FindBar
                                        // context) hands focus back.
                                        let (query, position) = self
                                            .find
                                            .clone()
                                            .unwrap_or_default();
                                        let count = self.find_matches(&query).len();
                                        composer.child(
                                            h_flex()
                                                .id("find-bar")
                                                .key_context("FindBar")
                                                .track_focus(&self.find_input.read(cx).focus_handle(cx))
                                                .px_4()
                                                .py_1p5()
                                                .gap_2()
                                                .items_center()
                                                .border_b_1()
                                                .border_color(cx.theme().border)
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .child(Input::new(&self.find_input).small()),
                                                )
                                                .child(
                                                    div()
                                                        .text_size(px(11.))
                                                        .text_color(cx.theme().muted_foreground)
                                                        .child(if count == 0 {
                                                            "no matches".to_string()
                                                        } else {
                                                            format!("{}/{count}", position + 1)
                                                        }),
                                                )
                                                .child(
                                                    Button::new("find-prev")
                                                        .label("↑")
                                                        .small()
                                                        .ghost()
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.find_step(false, cx);
                                                        })),
                                                )
                                                .child(
                                                    Button::new("find-next")
                                                        .label("↓")
                                                        .small()
                                                        .ghost()
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.find_step(true, cx);
                                                        })),
                                                )
                                                .child(
                                                    Button::new("find-close")
                                                        .label("✕")
                                                        .small()
                                                        .ghost()
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.close_find(cx);
                                                        })),
                                                ),
                                        )
                                    })
                                    // The resume bar: the open session's
                                    // last turn paused (the provider's
                                    // rate limit, or the round budget's
                                    // grace window closing). One static
                                    // line, one button, the way back in
                                    // without retyping the ask; the
                                    // transcript's note says which
                                    // pause it was. Snoozed, the line
                                    // says the resume will happen by
                                    // itself, and "now" plus "cancel"
                                    // replace the snooze.
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
                                                            "the turn paused, snoozed, it \
                                                             resumes by itself ({SNOOZE_MINUTES} min)"
                                                        )
                                                    } else {
                                                        "the turn paused, it can pick up \
                                                         where it stopped"
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
                                    .when(self.pending_rewind.is_some(), |composer| {
                                        // The rewind confirm: which
                                        // flavor, keep the files as
                                        // they are, or restore their
                                        // pre-turn content from the
                                        // checkpoint ledger. The
                                        // files choice only shows
                                        // when the ledger holds
                                        // something to restore.
                                        let pending = self
                                            .pending_rewind
                                            .as_ref()
                                            .expect("just checked");
                                        let files = pending.restore_points.len();
                                        composer.child(
                                            v_flex()
                                                .mx_4()
                                                .mt_2()
                                                .gap_1()
                                                .border_1()
                                                .border_dashed()
                                                .border_color(cx.theme().border)
                                                .rounded_md()
                                                .px_3()
                                                .py_2()
                                                .child(
                                                    div()
                                                        .text_size(px(12.))
                                                        .text_color(cx.theme().muted_foreground)
                                                        .child(
                                                            "Rewind to here, the conversation \
                                                             after this question leaves the \
                                                             transcript; the question returns \
                                                             as a draft.",
                                                        ),
                                                )
                                                .child(
                                                    h_flex()
                                                        .gap_2()
                                                        .child(
                                                            Button::new("rewind-keep")
                                                                .label("Revert and keep changes")
                                                                .xsmall()
                                                                .outline()
                                                                .tooltip(
                                                                    "Keep every file as it is, \
                                                                     only the conversation rewinds",
                                                                )
                                                                .on_click(cx.listener(
                                                                    |this, _, window, cx| {
                                                                        this.complete_rewind(
                                                                            false, window, cx,
                                                                        );
                                                                    },
                                                                )),
                                                        )
                                                        .when(files > 0, |row| {
                                                            row.child(
                                                                Button::new("rewind-files")
                                                                    .label(format!(
                                                                        "Revert files too ({files})"
                                                                    ))
                                                                    .xsmall()
                                                                    .outline()
                                                                    .tooltip(
                                                                        "Restore each touched \
                                                                         file to its content \
                                                                         before these turns",
                                                                    )
                                                                    .on_click(cx.listener(
                                                                        |this, _, window, cx| {
                                                                            this.complete_rewind(
                                                                                true, window, cx,
                                                                            );
                                                                        },
                                                                    )),
                                                            )
                                                        })
                                                        .child(
                                                            Button::new("rewind-cancel")
                                                                .label("Cancel")
                                                                .xsmall()
                                                                .ghost()
                                                                .on_click(cx.listener(
                                                                    |this, _, _, cx| {
                                                                        this.cancel_rewind(cx);
                                                                    },
                                                                )),
                                                        ),
                                                ),
                                        )
                                    })
                                    .when(!self.queued.is_empty(), |composer| {
                                        // Queued follow-ups as dashed
                                        // drafts: visible words, not a
                                        // count. Each can be sent now,
                                        // pulled back into the composer,
                                        // or dropped, the queue is
                                        // rows, so every action is real
                                        // in both directions.
                                        composer.child(
                                            v_flex().gap_1().children(
                                                self.queued.clone()
                                                    .into_iter()
                                                    .map(|draft| {
                                                        let id = draft.id.0;
                                                        let first_line = draft
                                                            .content
                                                            .lines()
                                                            .next()
                                                            .unwrap_or_default()
                                                            .to_string();
                                                        h_flex()
                                                            .mx_4()
                                                            .mt_2()
                                                            .gap_2()
                                                            .items_center()
                                                            .border_1()
                                                            .border_dashed()
                                                            .border_color(cx.theme().border)
                                                            .rounded_md()
                                                            .px_2()
                                                            .py_1()
                                                            .child(
                                                                div()
                                                                    .flex_1()
                                                                    .text_size(px(12.))
                                                                    .text_color(
                                                                        cx.theme()
                                                                            .muted_foreground,
                                                                    )
                                                                    .truncate()
                                                                    .child(first_line),
                                                            )
                                                            .child(
                                                                Button::new(format!(
                                                                    "queue-send-{id}"
                                                                ))
                                                                .label("send now")
                                                                .xsmall()
                                                                .ghost()
                                                                .tooltip(
                                                                    "Send this one now, \
                                                                     steers the running turn",
                                                                )
                                                                .on_click(cx.listener(
                                                                    move |this, _, _, cx| {
                                                                        this
                                                                            .force_send_followup(
                                                                                gertie_core::QueuedId(id),
                                                                                cx,
                                                                            );
                                                                    },
                                                                )),
                                                            )
                                                            .child(
                                                                Button::new(format!(
                                                                    "queue-edit-{id}"
                                                                ))
                                                                .label("edit")
                                                                .xsmall()
                                                                .ghost()
                                                                .tooltip(
                                                                    "Pull back into the composer",
                                                                )
                                                                .on_click(cx.listener(
                                                                    move |this, _, window, cx| {
                                                                        this.edit_followup(
                                                                            gertie_core::QueuedId(id),
                                                                            window,
                                                                            cx,
                                                                        );
                                                                    },
                                                                )),
                                                            )
                                                            .child(
                                                                Button::new(format!(
                                                                    "queue-drop-{id}"
                                                                ))
                                                                .label("✕")
                                                                .xsmall()
                                                                .ghost()
                                                                .tooltip(
                                                                    "Drop this queued message",
                                                                )
                                                                .on_click(cx.listener(
                                                                    move |this, _, _, cx| {
                                                                        this.remove_followup(
                                                                            gertie_core::QueuedId(id),
                                                                            cx,
                                                                        );
                                                                    },
                                                                )),
                                                            )
                                                    }),
                                            ),
                                        )
                                    })
                                    .when(!mentions.is_empty(), |composer| {
                                        // Mention discovery: typing `@`
                                        // offers the workspace's files,
                                        // narrowed by the query; Enter (or
                                        // a click) attaches through the
                                        // normal path, the chip shows what
                                        // rides. The agent reads the file
                                        // by its relative path.
                                        composer.child(
                                            v_flex()
                                                .id("mention-popup")
                                                .mx_4()
                                                .mt_2()
                                                .max_h(px(220.))
                                                .overflow_y_scroll()
                                                .rounded_md()
                                                .border_1()
                                                .border_color(cx.theme().border)
                                                .bg(cx.theme().background)
                                                .py_1()
                                                .children(mentions.iter().take(12).enumerate().map(
                                                    |(row_index, matched)| {
                                                        let path = matched.path.clone();
                                                        let token = matched.token.clone();
                                                        let selected = selected_popup_index
                                                            == Some(row_index);
                                                        h_flex()
                                                            .id(SharedString::from(format!(
                                                                "mention-row-{path}"
                                                            )))
                                                            .w_full()
                                                            .px_3()
                                                            .py_1()
                                                            .rounded_sm()
                                                            .justify_between()
                                                            .when(selected, |row| {
                                                                row.bg(cx.theme().secondary)
                                                            })
                                                            .hover(|style| {
                                                                style.bg(cx.theme().secondary)
                                                            })
                                                            .on_click(cx.listener({
                                                                move |this, _, window, cx| {
                                                                    let matched =
                                                                        MentionMatch {
                                                                            path: path.clone(),
                                                                            token: token.clone(),
                                                                        };
                                                                    this.accept_mention(
                                                                        matched, window, cx,
                                                                    );
                                                                }
                                                            }))
                                                            .child(
                                                                div()
                                                                    .text_size(px(12.5))
                                                                    .child(matched.path.clone()),
                                                            )
                                                            .child(
                                                                div()
                                                                    .text_size(px(10.5))
                                                                    .text_color(cx.theme().muted_foreground)
                                                                    .child("attach"),
                                                            )
                                                    },
                                                )),
                                        )
                                    })
                                    .when(!slash.is_empty(), |composer| {
                                        // Slash discovery: typing `/` lists
                                        // every command; typing narrows it.
                                        // Enter (or a click) accepts, a
                                        // template expands into the composer
                                        // so the real prompt is visible
                                        // before it goes anywhere.
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
                                                    .children(slash.iter().enumerate().map(
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
                                                            let selected = selected_popup_index
                                                                == Some(row_index);
                                                            div()
                                                                .id(("slash-row", row_index))
                                                                .w_full()
                                                                .px_3()
                                                                .py_1()
                                                                .rounded_sm()
                                                                .cursor_pointer()
                                                                .when(selected, |row| {
                                                                    row.bg(
                                                                        cx.theme()
                                                                            .accent
                                                                            .opacity(0.14),
                                                                    )
                                                                })
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
                                            .key_context("Composer")
                                            .px_4()
                                            .py_3()
                                            .gap_2()
                                            // The steer/queue hint lives in the
                                            // composer's placeholder now, the
                                            // sync keeps it true while a turn
                                            // runs, and an empty input is the
                                            // only state that ever shows it.
                                            .child(
                                                Button::new("attach-file")
                                                    // A paperclip, the one word
                                                    // every file-affordance
                                                    // speaks. Bytes, not a
                                                    // font glyph: the emoji
                                                    // label rasterized as
                                                    // nothing on Linux, and
                                                    // the app's asset source
                                                    // ships the kit's default
                                                    // bundle, not the full
                                                    // Lucide catalog this path
                                                    // lives in, `Icon::data`
                                                    // renders it without
                                                    // registering an asset.
                                                    .icon(Icon::default().data(PAPERCLIP_SVG))
                                                    .small()
                                                    .ghost()
                                                    // The label left the
                                                    // screen with the word;
                                                    // the name stays for the
                                                    // reader that never sees
                                                    // pixels.
                                                    .accessibility_label("attach file")
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
                    .when(self.panel == Panel::Settings, |row| {
                        row.child(
                            div()
                                .key_context("SettingsPanel")
                                .track_focus(&self.side_focus)
                                .child(self.settings_panel.clone()),
                        )
                    })
                    .when(self.panel == Panel::Memory, |row| {
                        row.child(self.render_memory_panel(cx))
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

const NO_ENDPOINT_NOTICE: &str = "No endpoint configured, none is built in. Add one in the \
    settings panel (type `/settings` in the composer): any OpenAI-compatible URL, the model id it \
    serves, and the env var holding your key. `gertie doctor` checks the setup.";

/// The composer's paperclip, the Lucide path as raw bytes. The app's
/// asset source is the kit's default icon bundle; the full Lucide
/// catalog it draws from would add megabytes for one glyph, so the
/// path rides here and `Icon::data` paints it without an asset
/// lookup. `currentColor` keeps it in whatever the button's text
/// color is at rest and hovered.
const PAPERCLIP_SVG: &[u8] =
    b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\" \
    viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\" \
    stroke-linecap=\"round\" stroke-linejoin=\"round\"><path d=\"m16 6-8.414 8.586a2 2 0 0 0 \
    2.829 2.829l8.414-8.586a4 4 0 1 0-5.657-5.657l-8.379 8.551a6 6 0 1 0 8.485 8.485l8.379-8.551\" \
    /></svg>";

/// A stored row as a transcript turn: tool rows render as finished
/// cards, everything else as its role's bubble. The one mapping both
/// the reload and the "load earlier" pagination share, a row must
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

/// The footer's word for a mode, the enum's snake_case config
/// spelling, spoken the way the UI speaks.
fn mode_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Supervised => "supervised",
        PermissionMode::AcceptEdits => "accept edits",
        PermissionMode::Auto => "auto",
        PermissionMode::FullAccess => "full access",
    }
}

/// The footer chip's hover text: what the current mode asks, and how
/// the ring moves. The one control that can remove every guardrail
/// says plainly what it does.
fn mode_tooltip(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Supervised => {
            "supervised: every edit and command asks. Click: accept edits (edits run unasked, reversible; bash asks). Full access is in the palette."
        }
        PermissionMode::AcceptEdits => {
            "accept edits: edits run unasked and reversible, bash asks. Click: auto (sandboxed bash unasked). Click again: back to supervised. Full access is in the palette."
        }
        PermissionMode::Auto => {
            "auto: edits and sandboxed bash run unasked. Click: back to supervised. Full access (nothing asks) is in the palette."
        }
        PermissionMode::FullAccess => "full access: nothing asks. Click: back to supervised.",
    }
}

/// Stored text as display text: attachment markers become a quiet note
/// naming the files, the bytes live in the attachments dir, not the
/// transcript. Invisible-character hygiene applies at this render
/// boundary too: stored text predating the tool-seam strip (or from a
/// provider gone strange) renders without bidi or zero-width tricks.
/// File names, not paths: the dir is gertie's own.
fn display_text(text: &str) -> String {
    let (clean, images) = gertie_core::extract_image_markers(&gertie_core::strip_invisible(text));
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

/// `/home/martin/x/y` → `~/x/y`, the footer's folder stays short. No
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

/// Whether a session's stored root and a tab's root are the same
/// folder. Canonical compare when both resolve (symlinks, `..`
/// segments, bind mounts); a folder that no longer exists falls back
/// to the literal strings, the store recorded what the OS handed it.
fn same_workspace(stored: &str, current: &std::path::Path) -> bool {
    let stored = std::path::Path::new(stored);
    match (stored.canonicalize(), current.canonicalize()) {
        (Ok(stored), Ok(current)) => stored == current,
        _ => stored == current,
    }
}

/// Elapsed as the status line speaks it: `14s`, then `2m 05s`. The
/// status line ticks at whole seconds; nobody times a model in
/// milliseconds.
fn clock(elapsed: std::time::Duration) -> String {
    let whole = elapsed.as_secs();
    if whole >= 60 {
        format!("{}m {:02}s", whole / 60, whole % 60)
    } else {
        format!("{whole}s")
    }
}

/// One row of the slash popup: a builtin (executes on accept) or a file
/// template (expands into the composer).
enum SlashMatch {
    Builtin(&'static str, &'static str),
    Template(usize),
}

/// The popup token for a composer value: the trailing `@query` run
/// after the buffer's newline is trimmed. The walk's reset check and
/// the tests share it.
pub fn popup_token_for_test(value: &str) -> Option<String> {
    let trimmed = value.trim_end_matches('\n');
    trimmed.rfind('@').map(|at| trimmed[at..].to_string())
}

/// The @-mention matcher, exposed for tests (doc'ed, not hidden: the
/// integration tests reach it through the crate's public surface).
pub fn mention_matches_for_test(input: &str, root: &std::path::Path) -> Vec<MentionMatch> {
    mention_matches(input, root)
}

/// One row of the @-mention popup: a workspace file the picker offers.
#[derive(Debug, Clone)]
pub struct MentionMatch {
    /// Path relative to the workspace root, what the agent reads by.
    pub path: String,
    /// The trailing `@query` token to erase on acceptance.
    pub token: String,
}

/// The @-mention's candidate list: files under the workspace root
/// whose relative path contains the query, directory scratch
/// (.git, target, node_modules, .gertie) excluded, capped so a big
/// repo renders a popup, not a novel. The bare `@` lists nothing
/// until a character narrows it: with thousands of files the first
/// keystroke should already mean something.
fn mention_matches(input: &str, root: &std::path::Path) -> Vec<MentionMatch> {
    // The token: an `@` that starts the input or follows whitespace,
    // up to the caret (the input state's value is all we see; the
    // token is the whole trailing `@...` run, mentions are typed at
    // the end like slash commands).
    let input = input.trim_end_matches('\n');
    let Some(at) = input.rfind('@') else {
        return Vec::new();
    };
    let at_line_start = at == 0
        || input[..at]
            .chars()
            .last()
            .is_some_and(|c| c.is_whitespace());
    if !at_line_start {
        return Vec::new();
    }
    let query = &input[at + 1..];
    // A query with spaces is a sentence again, not a mention.
    if query.is_empty() || query.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    let token = input[at..].to_string();
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if file_type.is_dir() {
                if matches!(
                    name.as_str(),
                    ".git" | ".gertie" | "target" | "node_modules" | "dist"
                ) {
                    continue;
                }
                if files.len() < 512 {
                    stack.push(path);
                }
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(name.clone());
            if !relative.contains(query) {
                continue;
            }
            files.push(relative);
            if files.len() >= 64 {
                return files
                    .into_iter()
                    .map(|path| MentionMatch {
                        path,
                        token: token.clone(),
                    })
                    .collect();
            }
        }
    }
    files.sort();
    files
        .into_iter()
        .map(|path| MentionMatch {
            path,
            token: token.clone(),
        })
        .collect()
}

/// A tool card's parsed face, memoized per turn index: the label in
/// both tenses and the diff's line data. Element construction stays
/// per-frame, gpui elements are consumed on render, but the JSON
/// parses and label shaping behind a card only need happen once: a
/// card's arguments never change, only its tense. Cleared wherever
/// the turns are replaced or shifted, with the other index-keyed
/// state.
struct ToolFace {
    label_running: String,
    label_done: String,
    diff: Option<crate::diff::Patch>,
}

/// What the current composer text would complete to, for the popup and
/// for Enter routing. Builtins first, they always win over same-named
/// files, then templates, filtered by prefix on the typed name. Empty
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
            "Summarize older history now, frees context, leaves the record whole",
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
    for (index, template) in templates.iter().enumerate() {
        if template.name.starts_with(name.as_str()) {
            matches.push(SlashMatch::Template(index));
        }
    }
    // With args present the name is already chosen, the popup narrows
    // to the exact row, as a confirm-and-expand.
    if !args.is_empty() {
        matches.retain(|matched| match matched {
            SlashMatch::Builtin(found, _) => *found == name,
            SlashMatch::Template(index) => templates[*index].name == name,
        });
    }
    matches
}

/// `1234` → "1.2k", token counts stay scannable: one decimal below
/// 100k (`12.3k`), whole k above (`123k`).
fn human_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.)
    } else if count >= 100_000 {
        format!("{:.0}k", count as f64 / 1000.)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1000.)
    } else {
        format!("{count}")
    }
}

#[cfg(test)]
mod tests {
    //! The diff face's pure layer: what the arguments (and a captured
    //! before-content) produce for a card to render. The rendering
    //! itself is exercised by the fixture tests in chat_input.rs.

    use super::*;
    use crate::diff::Line;

    fn kinds(patch: &crate::diff::Patch) -> Vec<&'static str> {
        patch
            .lines
            .iter()
            .map(|line| match line {
                Line::Same(_) => " ",
                Line::Add(_) => "+",
                Line::Remove(_) => "-",
            })
            .collect()
    }

    #[test]
    fn an_edit_face_diffs_old_against_new_with_context() {
        let call = ToolCall {
            id: "c1".into(),
            name: "edit_file".into(),
            arguments: r#"{"path": "f.txt", "old_text": "a\nb\nc\n", "new_text": "a\nx\nc\n"}"#
                .into(),
        };
        let patch = diff_data(&call, None).expect("edit face");
        assert_eq!(kinds(&patch), vec![" ", "-", "+", " "]);
        assert_eq!((patch.adds, patch.removes), (1, 1));
        assert_eq!(patch.omitted, 0);
    }

    #[test]
    fn a_write_face_diffs_against_the_captured_before() {
        let call = ToolCall {
            id: "c2".into(),
            name: "write_file".into(),
            arguments: r#"{"path": "out.txt", "content": "one\ntwo\n"}"#.into(),
        };
        // The file held one line: the second is the whole change, told
        // as a real diff, not a dump of the new content.
        let patch = diff_data(&call, Some("one\n")).expect("write face");
        assert_eq!(kinds(&patch), vec![" ", "+"]);
        assert_eq!((patch.adds, patch.removes), (1, 0));
        // No capture (a reloaded card): the honest fallback is all
        // additions, the file may as well be new.
        let patch = diff_data(&call, None).expect("write face");
        assert_eq!(kinds(&patch), vec!["+", "+"]);
        assert_eq!((patch.adds, patch.removes), (2, 0));
    }

    #[test]
    fn tools_without_a_natural_diff_have_no_face() {
        let call = ToolCall {
            id: "c3".into(),
            name: "bash".into(),
            arguments: r#"{"command": "cargo test"}"#.into(),
        };
        assert!(diff_data(&call, None).is_none());
    }

    #[test]
    fn hunk_trims_keep_context_beside_changes_and_elide_the_rest() {
        // Nine unchanged lines, one change, nine unchanged: the trim
        // keeps three lines of context on each side of the change and
        // a single elision row for the rest.
        let mut text = String::new();
        text.push_str(&(1..=9).map(|i| format!("top {i}\n")).collect::<String>());
        text.push_str("changed\n");
        text.push_str(&(1..=9).map(|i| format!("end {i}\n")).collect::<String>());
        let before = text.replace("changed\n", "old\n");
        let call = ToolCall {
            id: "c4".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "f.txt", "content": text}).to_string(),
        };
        let patch = diff_data(&call, Some(&before)).expect("write face");
        assert_eq!(
            (patch.adds, patch.removes),
            (1, 1),
            "counts ignore the trim"
        );
        let rows = hunk_rows(&patch.lines);
        let elided = rows
            .iter()
            .filter(|row| matches!(row, DiffRow::Elided))
            .count();
        assert_eq!(elided, 2, "one elision per side: {rows:?}");
        let kept = rows
            .iter()
            .filter(|row| matches!(row, DiffRow::Line(_)))
            .count();
        assert_eq!(kept, 3 + 2 + 3, "context, the change's two rows, context");
        // Every change survives the trim.
        assert!(rows.iter().any(|row| matches!(
            row,
            DiffRow::Line(crate::diff::Line::Add(text)) if text == "changed"
        )));
    }

    #[test]
    fn an_unchanged_write_says_unchanged_not_a_file_dump() {
        // Same content before and after: adds and removes are zero and
        // the face has nothing to show, the render says "unchanged".
        let call = ToolCall {
            id: "c5".into(),
            name: "write_file".into(),
            arguments: r#"{"path": "f.txt", "content": "same\n"}"#.into(),
        };
        let patch = diff_data(&call, Some("same\n")).expect("write face");
        assert_eq!((patch.adds, patch.removes), (0, 0));
    }
}
