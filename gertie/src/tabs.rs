//! Tabs: parallel chats in one window. Each tab is a full `ChatView` —
//! its own session, transcript, and stream; store, provider, broker,
//! and the root-independent tools are shared, so worker asks land on
//! one approval queue no matter which tab dispatched them. A worktree
//! tab (⎇) additionally gets its own tool registry rooted at a fresh
//! git worktree: its file tools and subagents work in a separate
//! checkout of the same repo, invisible to the tab next door.

use crate::bootstrap::Bootstrap;
use crate::chat::ChatView;
use gpui_kit::component::ActiveTheme as _;
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
        let first = cx.new(|cx| {
            ChatView::new(
                boot.store.clone(),
                boot.provider.clone(),
                boot.snapshots.clone(),
                boot.tools.clone(),
                boot.label.clone(),
                boot.system_prompt.clone(),
                boot.broker.clone(),
                boot.session_scope.clone(),
                boot.settings.clone(),
                true,
                window,
                cx,
            )
        });
        Self {
            boot,
            tabs: vec![TabState {
                view: first,
                worktree: None,
            }],
            active: 0,
            next_worktree: 0,
            last_error: None,
        }
    }

    fn make_view(
        &self,
        tools: gertie_core::ToolRegistry,
        system_prompt: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ChatView> {
        cx.new(|cx| {
            ChatView::new(
                self.boot.store.clone(),
                self.boot.provider.clone(),
                self.boot.snapshots.clone(),
                tools,
                self.boot.label.clone(),
                system_prompt,
                self.boot.broker.clone(),
                self.boot.session_scope.clone(),
                self.boot.settings.clone(),
                false,
                window,
                cx,
            )
        })
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
        self.active = self.tabs.len() - 1;
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
                self.active = self.tabs.len() - 1;
                self.last_error = None;
            }
            Err(error) => self.last_error = Some(error),
        }
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
