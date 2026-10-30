//! Tabs: parallel chats in one window. Each tab is a full `ChatView` —
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
}

struct TabState {
    view: Entity<ChatView>,
    /// The tab's own checkout, if it has one (marks the ⎇ label).
    worktree: Option<PathBuf>,
}

impl WorkspaceView {
    /// The first tab resumes the newest stored session; every later tab
    /// starts blank — two views on one session would diverge their
    /// in-memory histories against one database.
    pub fn new(boot: Arc<Bootstrap>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let panel = cx.new(|_| boot.settings_panel());
        let first = cx.new(|cx| {
            ChatView::new(
                boot.store.clone(),
                boot.provider(),
                boot.snapshots.clone(),
                boot.tools.clone(),
                boot.model_label(),
                boot.system_prompt.clone(),
                boot.templates.clone(),
                boot.workspace_root.clone(),
                boot.broker.clone(),
                boot.session_scope.clone(),
                panel,
                true,
                window,
                cx,
            )
        });
        let mut this = Self {
            boot,
            tabs: vec![TabState {
                view: first,
                worktree: None,
            }],
            active: 0,
            next_worktree: 0,
            last_error: None,
        };
        this.subscribe_config_saved(0, window, cx);
        this
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
            // The written config doesn't parse — leave the live state
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

    fn make_view(
        &self,
        tools: gertie_core::ToolRegistry,
        system_prompt: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ChatView> {
        let panel = cx.new(|_| self.boot.settings_panel());
        let view = cx.new(|cx| {
            ChatView::new(
                self.boot.store.clone(),
                self.boot.provider(),
                self.boot.snapshots.clone(),
                tools,
                self.boot.model_label(),
                system_prompt,
                self.boot.templates.clone(),
                self.boot.workspace_root.clone(),
                self.boot.broker.clone(),
                self.boot.session_scope.clone(),
                panel,
                false,
                window,
                cx,
            )
        });
        // Subscribing needs &mut self — the caller does it (add_tab /
        // add_worktree_tab / new).
        view
    }

    /// A plain tab: parallel chat, shared workspace.
    fn add_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.make_view(
            self.boot.tools.clone(),
            self.boot.system_prompt.clone(),
            window,
            cx,
        );
        self.tabs.push(TabState {
            view,
            worktree: None,
        });
        let index = self.tabs.len() - 1;
        self.active = index;
        self.subscribe_config_saved(index, window, cx);
        cx.notify();
    }

    /// A worktree tab: parallel chat in its own checkout of the repo —
    /// the DESIGN promise, "parallel sessions run against separate
    /// worktrees". The worktree persists on disk after the run.
    fn add_worktree_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = format!("tab-{}", self.next_worktree);
        match crate::worktree::create(&self.boot.workspace_root, &name) {
            Ok(root) => {
                self.next_worktree += 1;
                let (tools, system_prompt) = self.boot.registry_for_root(&root);
                let view = self.make_view(tools.clone(), system_prompt.clone(), window, cx);
                self.tabs.push(TabState {
                    view,
                    worktree: Some(root),
                });
                let index = self.tabs.len() - 1;
                self.active = index;
                self.subscribe_config_saved(index, window, cx);
                self.last_error = None;
            }
            Err(error) => self.last_error = Some(error),
        }
        cx.notify();
    }

    /// Closes the tab at `index`. The last tab stays — an empty window
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
                    .tooltip("New tab — parallel chat, shared workspace")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.add_tab(window, cx);
                    })),
            )
            .child(
                Button::new("add-worktree-tab")
                    .label("⎇")
                    .xsmall()
                    .ghost()
                    .tooltip("New worktree tab — its own git checkout of this repo")
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
                            cx.notify();
                        });
                    })
                    .children(self.tabs.iter().map(|tab| {
                        let title = tab.view.read(cx).tab_title();
                        let label = match &tab.worktree {
                            Some(_) => format!("⎇ {title}"),
                            None => title.to_string(),
                        };
                        Tab::new().label(label)
                    }))
                    .suffix(suffix),
            )
            .child(view)
    }
}
