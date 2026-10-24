//! Tabs: parallel chats in one window. Each tab is a full `ChatView` —
//! its own session, transcript, and stream; everything else (store,
//! provider, tools, broker) is shared, so worker asks land on one
//! approval queue no matter which tab dispatched them. Worktrees per
//! tab stay deferred: that wants per-view tool registries, a bigger
//! restructuring.

use crate::chat::ChatView;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::v_flex;
use gpui_kit::prelude::*;
use gpui_kit::{Context, Entity, IntoElement, Render, Window};
use std::sync::Arc;

pub struct WorkspaceView {
    store: Arc<gertie_core::SessionStore>,
    provider: Option<Arc<gertie_providers::OpenAICompatible>>,
    snapshots: Option<Arc<gertie_core::SnapshotStore>>,
    tools: gertie_core::ToolRegistry,
    endpoint_label: String,
    system_prompt: Option<String>,
    broker: Arc<gertie_core::PermissionBroker>,
    session_scope: gertie_core::SessionScope,
    tabs: Vec<TabState>,
    active: usize,
}

struct TabState {
    view: Entity<ChatView>,
}

impl WorkspaceView {
    /// The first tab resumes the newest stored session; every later tab
    /// starts blank — two views on one session would diverge their
    /// in-memory histories against one database.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<gertie_core::SessionStore>,
        provider: Option<Arc<gertie_providers::OpenAICompatible>>,
        snapshots: Option<Arc<gertie_core::SnapshotStore>>,
        tools: gertie_core::ToolRegistry,
        endpoint_label: String,
        system_prompt: Option<String>,
        broker: Arc<gertie_core::PermissionBroker>,
        session_scope: gertie_core::SessionScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let first = cx.new(|cx| {
            ChatView::new(
                store.clone(),
                provider.clone(),
                snapshots.clone(),
                tools.clone(),
                endpoint_label.clone(),
                system_prompt.clone(),
                broker.clone(),
                session_scope.clone(),
                true,
                window,
                cx,
            )
        });
        Self {
            store,
            provider,
            snapshots,
            tools,
            endpoint_label,
            system_prompt,
            broker,
            session_scope,
            tabs: vec![TabState { view: first }],
            active: 0,
        }
    }

    fn add_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.new(|cx| {
            ChatView::new(
                self.store.clone(),
                self.provider.clone(),
                self.snapshots.clone(),
                self.tools.clone(),
                self.endpoint_label.clone(),
                self.system_prompt.clone(),
                self.broker.clone(),
                self.session_scope.clone(),
                false,
                window,
                cx,
            )
        });
        self.tabs.push(TabState { view });
        self.active = self.tabs.len() - 1;
        cx.notify();
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active.min(self.tabs.len().saturating_sub(1));
        let view = self.tabs[active].view.clone();
        let entity = cx.entity().downgrade();

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
                        Tab::new().label(title)
                    }))
                    .suffix(Button::new("add-tab").label("+").xsmall().ghost().on_click(
                        cx.listener(|this, _, window, cx| {
                            this.add_tab(window, cx);
                        }),
                    )),
            )
            .child(view)
    }
}
