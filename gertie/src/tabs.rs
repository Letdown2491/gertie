//! The window shell: parallel chats in one window, chosen from the
//! sidebar (there is no tab bar). Each open chat is a full `ChatView`,
//! its own session, transcript, and stream; store, provider, broker,
//! and the root-independent tools are shared, so worker asks land on
//! one approval queue no matter which chat dispatched them. Only the
//! active view is on screen. A view stays alive while it has
//! something to say (a running turn, an unseen finish, a rate-limit
//! pause) and is dropped once it is idle and off screen: the
//! conversation itself lives in the store and the sidebar.
//!
//! A worktree chat additionally gets its own tool registry rooted at
//! a fresh git worktree: its file tools and subagents work in a
//! separate checkout of the same repo, invisible to the chat next
//! door.

// The internals keep the old "tab" names: one TabState per open view.

use crate::bootstrap::Bootstrap;
use crate::chat::{ChatView, ChatViewConfig, ChatViewEvent, SessionMark, ShellRequest};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::v_flex;
use gpui_kit::prelude::*;
use gpui_kit::{Context, Entity, EntityId, IntoElement, Render, Window};
use std::sync::Arc;

pub struct WorkspaceView {
    boot: Arc<Bootstrap>,
    tabs: Vec<TabState>,
    active: usize,
    /// Worktree names are stable across runs (`tab-0`, `tab-1`, …) so
    /// an earlier run's worktree is reused, not duplicated.
    next_worktree: usize,
    /// The view the last frame showed: a change means a switch, and
    /// the newly shown view reloads its sidebar list.
    shown: Option<EntityId>,
    /// When the close gate refused the first close attempt: the second
    /// one inside the confirm window means it.
    armed_close: Option<std::time::Instant>,
}

/// How long a refused close stays armed: the notice says "close
/// again", and for this long, closing again is believed.
const CLOSE_CONFIRM_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

/// What a tab's marker says, loudest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attention {
    None,
    /// A live turn: work running while another tab is up (a static
    /// dot, no repaint to say busy).
    Running,
    /// A run paused on its rate limit: the tab waits for the human.
    Limited,
    /// A turn finished unseen: the blue "something new" dot, cleared
    /// when the tab activates.
    Unseen,
}

/// One tab's attention mark and the state it leaves behind, the tab
/// bar's whole logic as a pure function. The falling streaming edge is
/// how a finish is seen without a callback.
fn tab_attention(
    streaming: bool,
    prev_streaming: bool,
    limited: bool,
    done_unseen: bool,
) -> (Attention, bool, bool) {
    if streaming {
        return (Attention::Running, true, done_unseen);
    }
    let finished_unseen = prev_streaming || done_unseen;
    if limited {
        return (Attention::Limited, false, finished_unseen);
    }
    if finished_unseen {
        return (Attention::Unseen, false, true);
    }
    (Attention::None, false, false)
}

impl Attention {
    fn mark(self) -> Option<SessionMark> {
        match self {
            Attention::None => None,
            Attention::Running => Some(SessionMark::Running),
            Attention::Limited => Some(SessionMark::Limited),
            Attention::Unseen => Some(SessionMark::Unseen),
        }
    }
}

/// Whether a window close goes through. A turn running anywhere holds
/// the first close, an accidental quit kills a running agent turn,
/// and arms the second.
fn close_confirmed(
    any_streaming: bool,
    armed_at: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    if !any_streaming {
        return true;
    }
    match armed_at {
        Some(armed_at) => now.duration_since(armed_at) <= CLOSE_CONFIRM_WINDOW,
        None => false,
    }
}

struct TabState {
    view: Entity<ChatView>,
    /// The previous render's streaming state: the rising/falling edge
    /// is how a finished turn is seen without a callback.
    prev_streaming: bool,
    /// What the shell last saw of this view (streaming, limited, open
    /// session): a repaint that changes none of it is not the shell's
    /// business.
    seen: Option<(bool, bool, Option<gertie_core::SessionId>, usize)>,
    /// A turn finished (or the run limited) while another tab was up.
    /// The dot on this tab's label; cleared when the tab activates.
    done_unseen: bool,
}

/// The rewind safety guard every chat shares: a file restore refuses
/// while another tab's turn is streaming in the same checkout, the
/// workspace is what knows every tab, so it builds the closure.
/// Fail-closed: if the workspace itself is unreachable, nothing
/// restores.
fn live_elsewhere(cx: &Context<WorkspaceView>) -> crate::chat::LiveElsewhere {
    let workspace = cx.weak_entity();
    Arc::new(move |cx, caller, root| {
        let Some(workspace) = workspace.upgrade() else {
            return Err("the workspace is gone".to_string());
        };
        // Read-only walk over the tab strip. The strong handle means
        // the read can't fail; the Result inside is the guard's own
        // verdict.
        workspace.read_with(cx, |workspace, cx| {
            for tab in &workspace.tabs {
                // The asking tab never guards against itself.
                if tab.view.entity_id() == caller.entity_id() {
                    continue;
                }
                let view = tab.view.read(cx);
                if view.is_streaming() && view.workspace_root() == root {
                    return Err(format!(
                        "'{}' is streaming in this checkout",
                        view.tab_title()
                    ));
                }
            }
            Ok(())
        })
    })
}

/// The switch every chat shares: a bootstrap question, asked by the
/// footer's word, answered with a full re-root. The closure shape
/// keeps the chat free of the bootstrap.
fn switch_to_root(boot: &Arc<Bootstrap>) -> crate::chat::SwitchWorkspace {
    let boot = boot.clone();
    Arc::new(move |root| boot.workspace_for_root(root))
}

impl WorkspaceView {
    /// The first tab resumes the newest stored session; every later tab
    /// starts blank, two views on one session would diverge their
    /// in-memory histories against one database.
    pub fn new(boot: Arc<Bootstrap>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            boot,
            tabs: Vec::new(),
            active: 0,
            next_worktree: 0,
            shown: None,
            armed_close: None,
        };
        let boot = this.boot.clone();
        let first = this.make_view(
            boot.tools.clone(),
            boot.system_prompt.clone(),
            boot.instructions.clone(),
            boot.memory_index.clone(),
            boot.memory_enabled(),
            boot.memory.clone(),
            boot.memory_extract_provider(),
            // The first view shares the workspace: no worktree branch
            // to name.
            None,
            true,
            window,
            cx,
        );
        this.push_view(first, window, cx);
        // The window-close gate: registered once, asked on every close
        // attempt (titlebar ✕, Alt+F4, WM gesture, there is no other
        // quit). Returning false holds the window open.
        let handle = cx.entity();
        window.on_window_should_close(cx, move |_, cx| {
            handle.update(cx, |this, cx| this.confirm_close(cx))
        });
        this
    }

    /// The close gate's body: a turn running anywhere holds the first
    /// close and says so in that tab; a second close inside the
    /// confirm window means it.
    fn confirm_close(&mut self, cx: &mut Context<Self>) -> bool {
        let any_streaming = self.tabs.iter().any(|tab| tab.view.read(cx).is_streaming());
        let now = std::time::Instant::now();
        let confirmed = close_confirmed(any_streaming, self.armed_close, now);
        if confirmed {
            self.armed_close = None;
        } else {
            self.armed_close = Some(now);
            // Say why in every tab that's running, the refused close
            // leaves no other trace, and the running tab may be in
            // the background.
            for tab in &self.tabs {
                if tab.view.read(cx).is_streaming() {
                    tab.view.update(cx, |chat, cx| {
                        chat.warn(
                            "A turn is still running, close the window again to stop it \
                             and quit.",
                            cx,
                        );
                    });
                }
            }
        }
        confirmed
    }

    /// Connects a new view to the shell: a settings save anywhere
    /// reloads the bootstrap once and fans the fresh provider out to
    /// every open view; any repaint of a view repaints the shell (an
    /// off-screen view's finish must reach the sidebar's markers);
    /// and the view gets the hook for the views it cannot make.
    fn wire_view(&mut self, tab: usize, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.tabs[tab].view.clone();
        cx.subscribe_in(&view, window, |this, _, event, _, cx| match event {
            ChatViewEvent::ConfigSaved => this.reload_and_apply(cx),
        })
        .detach();
        // A view repaints on every streamed token; the shell only
        // cares when what the sidebar shows changes (running, limited,
        // which conversation). An off-screen view's other repaints
        // must not redraw the window.
        cx.observe(&view, |this, view, cx| {
            let chat = view.read(cx);
            // The pending-card count too: an ask arriving here must
            // reach the status column of the chat on screen.
            let seen = (
                chat.is_streaming(),
                chat.open_session_limited(),
                chat.session_id(),
                chat.pending_ask_count(),
            );
            if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.view == view)
                && tab.seen != Some(seen)
            {
                tab.seen = Some(seen);
                cx.notify();
            }
        })
        .detach();
        let shell = cx.entity().downgrade();
        let ask: crate::chat::ShellAsk = std::sync::Arc::new(move |request, window, cx| {
            // The asking view is leased by its own click: the shell
            // only builds new views, it never touches the asker.
            let Some(shell) = shell.upgrade() else { return };
            shell.update(cx, |shell, cx| match request {
                ShellRequest::NewChat => shell.add_tab(window, cx),
                ShellRequest::NewWorktreeChat => shell.add_worktree_tab(window, cx),
                ShellRequest::NewChatIn { project } => {
                    shell.add_tab(window, cx);
                    let view = shell.tabs[shell.active].view.clone();
                    view.update(cx, |view, cx| view.new_chat_in(project, window, cx));
                }
                ShellRequest::AnswerAsk {
                    session,
                    ask,
                    answer,
                } => {
                    // The owning chat applies the answer to its own
                    // card: the record is the one a click there makes.
                    if let Some(owner) = shell.view_of(session, cx) {
                        let applied =
                            owner.update(cx, |owner, cx| owner.answer_from_column(ask, answer, cx));
                        if !applied {
                            shell.tell_active("That question was already answered.".into(), cx);
                        }
                    }
                    cx.notify();
                }
                ShellRequest::ShowSession { session } => {
                    if let Some(index) = shell
                        .tabs
                        .iter()
                        .position(|tab| tab.view.read(cx).session_id() == Some(session))
                    {
                        shell.activate(index, cx);
                    }
                }
            });
        });
        view.update(cx, |view, _| view.set_shell_ask(ask));
    }

    /// Says something in the view on screen: the shell has no surface
    /// of its own. Deferred, because the active view may be leased by
    /// the click that led here.
    fn tell_active(&self, text: String, cx: &mut Context<Self>) {
        let view = self.tabs[self.active].view.clone();
        cx.defer(move |cx| view.update(cx, |chat, cx| chat.warn(&text, cx)));
    }

    fn reload_and_apply(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.boot.reload() {
            // The written config doesn't parse, leave the live state
            // alone and say so.
            self.tell_active(format!("The saved config does not load: {error}"), cx);
            return;
        }
        let update = self.boot.snapshot();
        for tab in &self.tabs {
            let update = update.clone();
            tab.view
                .update(cx, |chat, cx| chat.apply_provider(update, cx));
        }
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn make_view(
        &self,
        tools: gertie_core::ToolRegistry,
        system_prompt: Option<String>,
        instructions: Option<String>,
        memory_index: Option<String>,
        memory_enabled: bool,
        memory: Option<Arc<gertie_core::MemoryStore>>,
        memory_extract_provider: Option<Arc<dyn gertie_core::Provider>>,
        naming: Option<crate::worktree::BranchNaming>,
        first: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ChatView> {
        let panel = cx.new(|_| self.boot.settings_panel());
        let guard = live_elsewhere(cx);
        let switch = switch_to_root(&self.boot);
        let router = Self::session_router(cx);
        let view = cx.new(|cx| {
            ChatView::new(
                ChatViewConfig {
                    store: self.boot.store.clone(),
                    provider: self.boot.provider(),
                    snapshots: self.boot.snapshots.clone(),
                    tools,
                    endpoint_label: self.boot.model_label(),
                    system_prompt,
                    instructions,
                    memory_index,
                    memory_enabled,
                    memory_store: memory,
                    memory_extract_provider,
                    max_tool_rounds: self.boot.max_tool_rounds(),
                    grace_rounds: self.boot.grace_rounds(),
                    templates: self.boot.templates.clone(),
                    workspace_root: self.boot.workspace_root.clone(),
                    settle_command: self.boot.settle_command(),
                    broker: self.boot.broker.clone(),
                    session_scope: self.boot.session_scope.clone(),
                    mode_scope: self.boot.mode_scope.clone(),
                    sandbox_bash: self.boot.sandbox_bash,
                    default_mode: self.boot.default_mode,
                    settings_panel: panel,
                    resume: first,
                    branch_naming: naming,
                    live_elsewhere: guard,
                    switch,
                    session_router: Some(router),
                },
                window,
                cx,
            )
        });
        // Wiring needs &mut self, the caller does it (push_view).
        view
    }

    /// The sidebar's routing hook for a view: weak self, strong at
    /// click time.
    fn session_router(cx: &mut Context<Self>) -> crate::chat::SessionRouter {
        let shell = cx.entity().downgrade();
        Arc::new(move |session_id, session_root, origin, window, cx| {
            let Some(shell) = shell.upgrade() else {
                return crate::chat::SessionRouting::Elsewhere;
            };
            shell.update(cx, |shell, cx| {
                shell.route_session(session_id, session_root, origin, window, cx)
            })
        })
    }

    /// Adds a made view to the shell, shows it and wires it.
    fn push_view(&mut self, view: Entity<ChatView>, window: &mut Window, cx: &mut Context<Self>) {
        self.tabs.push(TabState {
            view,
            prev_streaming: false,
            seen: None,
            done_unseen: false,
        });
        self.active = self.tabs.len() - 1;
        self.wire_view(self.active, window, cx);
        cx.notify();
    }

    /// A new view: a parallel chat in the shared workspace (New chat
    /// while a turn runs).
    fn add_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.make_view(
            self.boot.tools.clone(),
            self.boot.system_prompt.clone(),
            self.boot.instructions.clone(),
            self.boot.memory_index.clone(),
            self.boot.memory_enabled(),
            self.boot.memory.clone(),
            self.boot.memory_extract_provider(),
            None,
            false,
            window,
            cx,
        );
        self.push_view(view, window, cx);
    }

    /// A worktree chat: parallel chat in its own checkout of the repo,
    /// the DESIGN promise, "parallel sessions run against separate
    /// worktrees". The worktree persists on disk after the run.
    fn add_worktree_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // `git worktree add` is a subprocess against the object store,
        // hundreds of ms on a big repo, so it runs on the background
        // pool and the tab opens when the checkout exists. The tab-N
        // name is reserved up front: a failed create leaves a gap in
        // the numbering, which is cosmetic.
        let name = format!("tab-{}", self.next_worktree);
        self.next_worktree += 1;
        let repo_root = self.boot.workspace_root.clone();
        let prefix = self.boot.worktree_branch_prefix();
        let this = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let created = cx
                    .background_executor()
                    .spawn(async move { crate::worktree::create(&repo_root, &name) })
                    .await;
                let _ = this.update_in(cx, |this, window, cx| match created {
                    Ok(root) => {
                        let (tools, system_prompt, instructions, memory_index, memory) =
                            this.boot.registry_for_root(&root);
                        let naming = crate::worktree::BranchNaming {
                            root: root.clone(),
                            prefix,
                        };
                        let view = this.make_view(
                            tools.clone(),
                            system_prompt.clone(),
                            instructions.clone(),
                            memory_index.clone(),
                            this.boot.memory_enabled(),
                            memory,
                            this.boot.memory_extract_provider(),
                            Some(naming),
                            false,
                            window,
                            cx,
                        );
                        this.push_view(view, window, cx);
                    }
                    Err(error) => {
                        this.tell_active(format!("No worktree chat: {error}"), cx);
                    }
                });
            })
            .detach();
    }
}

impl WorkspaceView {
    /// Tests: the active tab's chat handle, the tab count, and the
    /// active tab's root. The routing assertions read these.
    pub fn tab_probe(&self, cx: &gpui_kit::App) -> (usize, Entity<ChatView>, std::path::PathBuf) {
        (
            self.tabs.len(),
            self.tabs[self.active].view.clone(),
            self.tabs[self.active]
                .view
                .read(cx)
                .workspace_root()
                .to_path_buf(),
        )
    }

    /// Routes a sidebar click to the view where the session belongs:
    /// 0. a view already has it open → show that view (two views on
    ///    one session would diverge their histories);
    /// 1. the clicked view is idle at the session's root → it loads
    ///    the session;
    /// 2. an idle off-screen view sits at the session's root → show
    ///    it and load there;
    /// 3. the clicked view is idle → it adopts the session's root and
    ///    loads it;
    /// 4. the clicked view is running a turn → a new view opens at the
    ///    session's root. A running turn is never dragged anywhere.
    ///
    /// The clicked view is leased by its own click, so this method
    /// never reads or updates it: its state arrives in `origin`, and
    /// its share of the work (rules 1 and 3, the refusals) leaves as
    /// the returned instruction. Whatever the shell does itself
    /// touches only off-screen views.
    pub fn route_session(
        &mut self,
        session_id: gertie_core::SessionId,
        session_root: Option<String>,
        origin: crate::chat::ClickOrigin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> crate::chat::SessionRouting {
        // The clicked view is leased by its own click: this method may
        // not read or update it (a re-entrant access is the gpui
        // crash). Its state arrives in `origin`, and its share of the
        // work leaves as the return value.
        let active_root = origin.root;
        let session_root = session_root
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| active_root.clone());
        let same_root =
            |a: &std::path::Path, b: &std::path::Path| match (a.canonicalize(), b.canonicalize()) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            };
        // Rule 0: already open. In the clicked view, a reload (it
        // also brings a settled session back); elsewhere, show it.
        if origin.session == Some(session_id) {
            return crate::chat::SessionRouting::Here;
        }
        if let Some(index) = (0..self.tabs.len())
            .filter(|index| *index != self.active)
            .find(|index| self.tabs[*index].view.read(cx).session_id() == Some(session_id))
        {
            self.activate(index, cx);
            return crate::chat::SessionRouting::Elsewhere;
        }
        // Rule 1: the clicked view is idle at the session's root (or
        // the session predates the root column: its home is wherever
        // it's opened).
        let session_matches_active =
            session_root == active_root || same_root(&session_root, &active_root);
        if session_matches_active && !origin.streaming {
            return crate::chat::SessionRouting::Here;
        }
        // Rule 2: an off-screen view at the session's root with nothing
        // to say (idle, nothing unseen) takes it.
        let holder = (0..self.tabs.len())
            .filter(|index| *index != self.active)
            .find(|index| {
                let tab = &self.tabs[*index];
                let view = tab.view.read(cx);
                let view_root = view.workspace_root().to_path_buf();
                !view.is_streaming()
                    && !tab.done_unseen
                    && !view.open_session_limited()
                    && (view_root == session_root || same_root(&view_root, &session_root))
            });
        if let Some(index) = holder {
            let view = self.tabs[index].view.clone();
            self.activate(index, cx);
            view.update(cx, |view, cx| {
                view.switch_to_session(session_id, window, cx)
            });
            return crate::chat::SessionRouting::Elsewhere;
        }
        // Rule 3: the clicked view is idle: it adopts the root. The
        // adoption is the clicked view's work: the Switched rides back
        // to it.
        if !origin.streaming {
            return match self.boot.workspace_for_root(&session_root) {
                Ok(switched) => crate::chat::SessionRouting::Adopt(switched),
                Err(error) => crate::chat::SessionRouting::Refused(format!(
                    "This conversation started in a folder that refuses the switch ({error})."
                )),
            };
        }
        // Rule 4: open a new view at the session's root and load there.
        let switched = match self.boot.workspace_for_root(&session_root) {
            Ok(switched) => switched,
            Err(error) => {
                // The refusal rides back to the clicked view, which
                // shows it; the shell builds nothing.
                return crate::chat::SessionRouting::Refused(format!(
                    "This conversation's folder refuses the switch ({error})."
                ));
            }
        };
        let view = self.make_view(
            switched.tools.clone(),
            switched.system_prompt.clone(),
            switched.instructions.clone(),
            switched.memory_index.clone(),
            self.boot.memory_enabled(),
            switched.memory.clone(),
            switched.memory_extract_provider.clone(),
            None,
            false,
            window,
            cx,
        );
        view.update(cx, |view, cx| {
            view.adopt_switch(switched, cx);
            view.switch_to_session(session_id, window, cx);
        });
        self.push_view(view, window, cx);
        crate::chat::SessionRouting::Elsewhere
    }

    /// The open view holding a conversation, if any.
    fn view_of(
        &self,
        session: gertie_core::SessionId,
        cx: &gpui_kit::App,
    ) -> Option<Entity<ChatView>> {
        self.tabs
            .iter()
            .map(|tab| tab.view.clone())
            .find(|view| view.read(cx).session_id() == Some(session))
    }

    /// Shows the view at `index`: its unseen finish is now seen.
    fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        self.active = index;
        self.tabs[index].done_unseen = false;
        cx.notify();
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        // Each view's attention, by the pure state machine; the shown
        // view has nothing unseen.
        let mut attention = Vec::with_capacity(self.tabs.len());
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            let view = tab.view.read(cx);
            let (mark, prev_streaming, done_unseen) = tab_attention(
                view.is_streaming(),
                tab.prev_streaming,
                view.open_session_limited(),
                tab.done_unseen && index != self.active,
            );
            tab.prev_streaming = prev_streaming;
            tab.done_unseen = done_unseen && index != self.active;
            let mark = match mark {
                Attention::Unseen if index == self.active => Attention::None,
                mark => mark,
            };
            attention.push(mark);
        }
        // Drop the off-screen views with nothing to say and nothing
        // unsaved (holds_unsaved): their
        // conversations are in the store, a sidebar click reopens them.
        let unsaved: Vec<bool> = self
            .tabs
            .iter()
            .map(|tab| tab.view.read(cx).holds_unsaved(cx))
            .collect();
        let mut index = 0;
        let mut kept_attention = Vec::with_capacity(attention.len());
        let active = self.active;
        let mut new_active = 0;
        self.tabs.retain(|_| {
            let keep = index == active || attention[index] != Attention::None || unsaved[index];
            if keep {
                if index == active {
                    new_active = kept_attention.len();
                }
                kept_attention.push(attention[index]);
            }
            index += 1;
            keep
        });
        self.active = new_active;
        // The sidebar's markers, keyed by conversation.
        let marks: std::collections::HashMap<_, _> = self
            .tabs
            .iter()
            .zip(&kept_attention)
            .filter_map(|(tab, attention)| {
                Some((tab.view.read(cx).session_id()?, attention.mark()?))
            })
            .collect();
        let view = self.tabs[self.active].view.clone();
        // The status column's WAITING ON YOU: pending asks in the other
        // open chats of the shown chat's project (issue #33). The shown
        // view's list answers which project each chat is in, its
        // sidebar is where moves happen. No project, no reach: an
        // unfiled chat shows only its own asks, in its transcript.
        let asks = {
            let shown = view.read(cx);
            let others = self
                .tabs
                .iter()
                .filter(|tab| tab.view != view)
                .filter_map(|tab| {
                    let other = tab.view.read(cx);
                    let id = other.session_id()?;
                    Some((shown.project_of(id), other.pending_asks()))
                });
            crate::chat::status::project_asks_for(shown.project_id(), others)
        };
        let switched = self.shown != Some(view.entity_id());
        self.shown = Some(view.entity_id());
        view.update(cx, |chat, cx| {
            chat.set_project_asks(asks);
            chat.set_session_marks(marks);
            if switched {
                chat.refresh_sessions(cx);
            }
        });

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_attention_marks_follow_the_work() {
        // Running: the dot, and the rising edge is remembered.
        let (marked, prev, done) = tab_attention(true, false, false, false);
        assert_eq!(marked, Attention::Running);
        assert!(prev);
        assert!(!done);

        // The falling edge turns the remembered streaming into a
        // finished-unseen mark.
        let (marked, prev, done) = tab_attention(false, true, false, false);
        assert_eq!(marked, Attention::Unseen);
        assert!(!prev);
        assert!(done);

        // The mark survives further quiet renders (a repaint without
        // a focus must not lose the signal).
        let (marked, _, done) = tab_attention(false, false, false, true);
        assert_eq!(marked, Attention::Unseen);
        assert!(done);

        // Limited outranks the unseen dot: the tab waits for the human.
        let (marked, prev, done) = tab_attention(false, true, true, false);
        assert_eq!(marked, Attention::Limited);
        assert!(!prev);
        assert!(done, "limited clears unseen only on focus");

        // Activating clears the mark: done=false renders plain.
        let (marked, _, done) = tab_attention(false, false, false, false);
        assert_eq!(marked, Attention::None);
        assert!(!done);
    }

    #[test]
    fn a_running_turn_holds_the_first_close_the_second_inside_the_window_goes() {
        let now = std::time::Instant::now();
        // Nothing running: a close is a close, armed or not.
        assert!(close_confirmed(false, None, now));
        assert!(close_confirmed(false, Some(now), now));
        // Turn running: the first close holds, the second inside the
        // confirm window means it, and a late one holds again, the
        // arm expires with the notice it came from.
        assert!(!close_confirmed(true, None, now));
        assert!(close_confirmed(true, Some(now), now + CLOSE_CONFIRM_WINDOW));
        assert!(!close_confirmed(
            true,
            Some(now),
            now + CLOSE_CONFIRM_WINDOW + std::time::Duration::from_millis(1)
        ));
    }
}
