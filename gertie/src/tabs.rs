//! Tabs: parallel chats in one window. Each tab is a full `ChatView`,
//! its own session, transcript, and stream; store, provider, broker,
//! and the root-independent tools are shared, so worker asks land on
//! one approval queue no matter which tab dispatched them. A worktree
//! tab (⎇) additionally gets its own tool registry rooted at a fresh
//! git worktree: its file tools and subagents work in a separate
//! checkout of the same repo, invisible to the tab next door.

use crate::bootstrap::Bootstrap;
use crate::chat::{ChatView, ChatViewEvent};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{Context, Entity, IntoElement, Render, Window, div, px};
use std::path::PathBuf;
use std::sync::Arc;

pub struct WorkspaceView {
    boot: Arc<Bootstrap>,
    tabs: Vec<TabState>,
    active: usize,
    /// Worktree names are stable across runs (`tab-0`, `tab-1`, …) so
    /// an earlier run's worktree is reused, not duplicated.
    next_worktree: usize,
    /// Why the last worktree request failed, shown beside the tab bar
    /// until the next attempt. Worktree creation must never be silent.
    last_error: Option<String>,
    /// When the close gate refused the first close attempt: the second
    /// one inside the confirm window means it.
    armed_close: Option<std::time::Instant>,
}

/// How long a refused close stays armed: the notice says "close
/// again", and for this long, closing again is believed.
const CLOSE_CONFIRM_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

/// One tab's attention mark and the state it leaves behind, the
/// tab bar's whole logic as a pure function. Loudest first: ● a live
/// turn (work running while another tab is up, static glyph, no
/// repaint to say busy), ⏸ a run paused on its rate limit (the tab
/// waits for the human), ✓ a turn finished unseen (cleared when the
/// tab activates, the falling streaming edge is how it's seen without
/// a callback).
fn tab_attention(
    streaming: bool,
    prev_streaming: bool,
    limited: bool,
    done_unseen: bool,
    label: &str,
) -> (String, bool, bool) {
    if streaming {
        return (format!("● {label}"), true, done_unseen);
    }
    let finished_unseen = prev_streaming || done_unseen;
    if limited {
        return (format!("⏸ {label}"), false, finished_unseen);
    }
    if finished_unseen {
        return (format!("✓ {label}"), false, true);
    }
    (label.to_string(), false, false)
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
    /// The tab's own checkout, if it has one (marks the ⎇ label).
    worktree: Option<PathBuf>,
    /// The previous render's streaming state: the rising/falling edge
    /// is how a finished turn is seen without a callback.
    prev_streaming: bool,
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
        let panel = cx.new(|_| boot.settings_panel());
        let guard = live_elsewhere(cx);
        let switch = switch_to_root(&boot);
        let shell = cx.entity().downgrade();
        let this_router: crate::chat::SessionRouter =
            Arc::new(move |session_id, session_root, origin, window, cx| {
                let Some(shell) = shell.upgrade() else {
                    return crate::chat::SessionRouting::Elsewhere;
                };
                shell.update(cx, |shell, cx| {
                    shell.route_session(session_id, session_root, origin, window, cx)
                })
            });
        let first = cx.new(|cx| {
            ChatView::new(
                boot.store.clone(),
                boot.provider(),
                boot.snapshots.clone(),
                boot.tools.clone(),
                boot.model_label(),
                boot.system_prompt.clone(),
                boot.instructions.clone(),
                boot.memory_index.clone(),
                boot.memory_enabled(),
                boot.memory.clone(),
                boot.memory_extract_provider(),
                boot.max_tool_rounds(),
                boot.grace_rounds(),
                boot.templates.clone(),
                boot.workspace_root.clone(),
                boot.settle_command(),
                boot.broker.clone(),
                boot.session_scope.clone(),
                boot.mode_scope.clone(),
                boot.sandbox_bash,
                boot.default_mode,
                panel,
                true,
                // The first tab shares the workspace: no worktree
                // branch to name.
                None,
                guard,
                switch,
                Some(this_router.clone()),
                window,
                cx,
            )
        });
        let mut this = Self {
            boot,
            tabs: vec![TabState {
                view: first,
                worktree: None,
                prev_streaming: false,
                done_unseen: false,
            }],
            active: 0,
            next_worktree: 0,
            last_error: None,
            armed_close: None,
        };
        this.subscribe_config_saved(0, window, cx);
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

    /// A settings save anywhere reloads the bootstrap once and fans the
    /// fresh provider/label/panel out to every open tab.
    fn subscribe_config_saved(&mut self, tab: usize, _window: &mut Window, cx: &mut Context<Self>) {
        cx.subscribe_in(
            &self.tabs[tab].view,
            _window,
            |this, _, event, _, cx| match event {
                ChatViewEvent::ConfigSaved => this.reload_and_apply(cx),
            },
        )
        .detach();
    }

    fn reload_and_apply(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.boot.reload() {
            // The written config doesn't parse, leave the live state
            // alone and say so where the worktree errors show.
            self.last_error = Some(format!("config: {error}"));
            cx.notify();
            return;
        }
        self.last_error = None;
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ChatView> {
        let panel = cx.new(|_| self.boot.settings_panel());
        let guard = live_elsewhere(cx);
        let switch = switch_to_root(&self.boot);
        // The sidebar's routing hook: weak self, strong at click time.
        let shell = cx.entity().downgrade();
        let this_router: crate::chat::SessionRouter =
            Arc::new(move |session_id, session_root, origin, window, cx| {
                let Some(shell) = shell.upgrade() else {
                    return crate::chat::SessionRouting::Elsewhere;
                };
                shell.update(cx, |shell, cx| {
                    shell.route_session(session_id, session_root, origin, window, cx)
                })
            });
        let view = cx.new(|cx| {
            ChatView::new(
                self.boot.store.clone(),
                self.boot.provider(),
                self.boot.snapshots.clone(),
                tools,
                self.boot.model_label(),
                system_prompt,
                instructions,
                memory_index,
                memory_enabled,
                memory,
                memory_extract_provider,
                self.boot.max_tool_rounds(),
                self.boot.grace_rounds(),
                self.boot.templates.clone(),
                self.boot.workspace_root.clone(),
                self.boot.settle_command(),
                self.boot.broker.clone(),
                self.boot.session_scope.clone(),
                self.boot.mode_scope.clone(),
                self.boot.sandbox_bash,
                self.boot.default_mode,
                panel,
                false,
                naming,
                guard,
                switch,
                Some(this_router.clone()),
                window,
                cx,
            )
        });
        // Subscribing needs &mut self, the caller does it (add_tab /
        // add_worktree_tab / new).
        view
    }

    /// A plain tab: parallel chat, shared workspace.
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
            window,
            cx,
        );
        self.tabs.push(TabState {
            view,
            worktree: None,
            prev_streaming: false,
            done_unseen: false,
        });
        let index = self.tabs.len() - 1;
        self.active = index;
        self.subscribe_config_saved(index, window, cx);
        cx.notify();
    }

    /// A worktree tab: parallel chat in its own checkout of the repo,
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
                            window,
                            cx,
                        );
                        this.tabs.push(TabState {
                            view,
                            worktree: Some(root),
                            prev_streaming: false,
                            done_unseen: false,
                        });
                        let index = this.tabs.len() - 1;
                        this.active = index;
                        this.subscribe_config_saved(index, window, cx);
                        this.last_error = None;
                        cx.notify();
                    }
                    Err(error) => {
                        this.last_error = Some(error);
                        cx.notify();
                    }
                });
            })
            .detach();
    }

    /// Closes the tab at `index`. The last tab stays, an empty window
    /// is a broken state, not a feature. A worktree tab's checkout
    /// persists on disk (the DESIGN note on worktree tabs).
    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.tabs.len() <= 1 {
            return;
        }
        self.tabs.remove(index);
        self.active = self.active.min(self.tabs.len() - 1);
        cx.notify();
    }
}

impl WorkspaceView {
    /// Routes a sidebar click to the tab where the session belongs
    /// (the full routing model, four rules):
    /// 1. the session's root matches the clicked tab's → load there;
    /// 2. another open tab already holds the root → activate it and
    ///    load there (a project's sessions belong to its project tab);
    /// 3. no tab holds it and the clicked tab is pristine → that tab
    ///    adopts the root and loads it (a launch-tab is free real
    ///    estate);
    /// 4. no tab holds it and the clicked tab is busy → a new tab
    ///    opens, rooted at the session's workspace. A working
    ///    conversation is never dragged somewhere else.
    ///
    /// The clicked view is leased by its own click, so this method
    /// never reads or updates it: its state arrives in `origin`, and
    /// its share of the work (rule 1's load, rule 3's adoption, the
    /// refusals) leaves as the returned instruction. Whatever the
    /// shell does itself (rule 2's holder, rule 4's new tab) touches
    /// only idle views.
    ///
    /// Streaming tabs are skipped by rule 2's load, not the routing:
    /// loading into a streaming view is refused by the view itself.
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
        // Rule 1: this tab's root matches (or the session predates the
        // root column: its home is wherever it's opened). The clicked
        // view loads it; the instruction is the answer.
        let session_matches_active =
            session_root == active_root || same_root(&session_root, &active_root);
        if session_matches_active {
            return crate::chat::SessionRouting::Here;
        }
        // Rule 2: a tab that already lives at the session's root. The
        // active tab is skipped (rule 1 just proved its root differs),
        // and every remaining tab is idle: reading them is safe here.
        let holder = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != self.active)
            .find(|(_, tab)| {
                let view_root = tab.view.read(cx).workspace_root().to_path_buf();
                view_root == session_root || same_root(&view_root, &session_root)
            });
        if let Some((index, tab)) = holder {
            let view = tab.view.clone();
            self.active = index;
            self.tabs[index].done_unseen = false;
            view.update(cx, |view, cx| {
                view.switch_to_session(session_id, window, cx)
            });
            cx.notify();
            return crate::chat::SessionRouting::Elsewhere;
        }
        // Rule 3: this tab is pristine (no session, no turns): it
        // adopts the root. The adoption is the clicked view's work:
        // the Switched rides back to it.
        if origin.pristine {
            return match self.boot.workspace_for_root(&session_root) {
                Ok(switched) => crate::chat::SessionRouting::Adopt(switched),
                Err(error) => crate::chat::SessionRouting::Refused(format!(
                    "This conversation started in a folder that refuses the switch ({error}); open it from that folder's tab."
                )),
            };
        }
        // Rule 4: open a new tab at the session's root and load there.
        let switched = match self.boot.workspace_for_root(&session_root) {
            Ok(switched) => switched,
            Err(error) => {
                // The refusal rides back to the clicked view, which
                // shows it; the shell builds nothing.
                return crate::chat::SessionRouting::Refused(format!(
                    "This conversation's folder refuses the switch ({error}); open it from that folder's tab."
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
            window,
            cx,
        );
        view.update(cx, |view, cx| {
            view.adopt_switch(switched, cx);
            view.switch_to_session(session_id, window, cx);
        });
        self.tabs.push(TabState {
            view,
            worktree: None,
            prev_streaming: false,
            done_unseen: false,
        });
        let index = self.tabs.len() - 1;
        self.active = index;
        self.subscribe_config_saved(index, window, cx);
        cx.notify();
        crate::chat::SessionRouting::Elsewhere
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active.min(self.tabs.len().saturating_sub(1));
        let view = self.tabs[active].view.clone();
        let entity = cx.entity().downgrade();

        let mut suffix = h_flex().items_center().gap_1();
        if let Some(error) = &self.last_error {
            suffix = suffix.child(
                div()
                    .text_size(px(11.))
                    .text_color(cx.theme().danger)
                    .max_w_96()
                    .truncate()
                    .child(error.clone()),
            );
        }
        suffix = suffix
            .child(
                Button::new("add-tab")
                    .label("+")
                    .xsmall()
                    .ghost()
                    .tooltip("New tab, parallel chat, shared workspace")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.add_tab(window, cx);
                    })),
            )
            .child(
                Button::new("add-worktree-tab")
                    .label("⎇")
                    .xsmall()
                    .ghost()
                    .tooltip("New worktree tab, its own git checkout of this repo")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.add_worktree_tab(window, cx);
                    })),
            )
            .child(
                Button::new("close-tab")
                    .label("✕")
                    .xsmall()
                    .ghost()
                    .disabled(self.tabs.len() <= 1)
                    .tooltip(if self.tabs.len() <= 1 {
                        "The last tab stays"
                    } else {
                        "Close this tab"
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_tab(active, cx);
                    })),
            );

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                TabBar::new("chat-tabs")
                    .selected_index(active)
                    .on_click(move |index, _window, cx| {
                        let Some(this) = entity.upgrade() else { return };
                        this.update(cx, |this, cx| {
                            this.active = *index;
                            if let Some(tab) = this.tabs.get_mut(*index) {
                                tab.done_unseen = false;
                            }
                            cx.notify();
                        });
                    })
                    .children(self.tabs.iter_mut().map(|tab| {
                        let view = tab.view.read(cx);
                        let title = view.tab_title();
                        let label = match &tab.worktree {
                            Some(_) => format!("⎇ {title}"),
                            None => title.to_string(),
                        };
                        // The attention marks, loudest first (see
                        // tab_attention): ● running, ⏸ limited, ✓
                        // finished unseen. The state machine rides
                        // tab_attention so the tests own the logic and
                        // the render only paints it.
                        let (marked, prev_streaming, done_unseen) = tab_attention(
                            view.is_streaming(),
                            tab.prev_streaming,
                            view.open_session_limited(),
                            tab.done_unseen,
                            &label,
                        );
                        tab.prev_streaming = prev_streaming;
                        tab.done_unseen = done_unseen;
                        Tab::new().label(marked)
                    }))
                    .suffix(suffix),
            )
            .child(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_attention_marks_follow_the_work() {
        // Running: the dot, and the rising edge is remembered.
        let (marked, prev, done) = tab_attention(true, false, false, false, "chat");
        assert_eq!(marked, "● chat");
        assert!(prev);
        assert!(!done);

        // The falling edge turns the remembered streaming into a
        // finished-unseen mark.
        let (marked, prev, done) = tab_attention(false, true, false, false, "chat");
        assert_eq!(marked, "✓ chat");
        assert!(!prev);
        assert!(done);

        // The mark survives further quiet renders (a repaint without
        // a focus must not lose the signal).
        let (marked, _, done) = tab_attention(false, false, false, true, "chat");
        assert_eq!(marked, "✓ chat");
        assert!(done);

        // Limited outranks the ✓: the tab waits for the human.
        let (marked, prev, done) = tab_attention(false, true, true, false, "chat");
        assert_eq!(marked, "⏸ chat");
        assert!(!prev);
        assert!(done, "limited clears unseen only on focus");

        // Activating clears the mark: done=false renders plain.
        let (marked, _, done) = tab_attention(false, false, false, false, "chat");
        assert_eq!(marked, "chat");
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
