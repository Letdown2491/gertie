//! The session sidebar as its own module: the rows (active and
//! settled), the rename/delete/undo affordances, the utility buttons.
//! One `impl ChatView` block: no new types, the render body moved
//! whole (issue #16 step 3), the methods it calls stay on ChatView.

use super::*;

impl ChatView {
    /// The sidebar column, whole: sessions scroll, the utility
    /// buttons stay put at the foot. A focus area (F6): the
    /// container tracks focus so the cycle can find it.
    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let visible = self.visible_sessions();

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
                    .px_2()
                    .gap_0p5()
                    .overflow_y_scroll()
                    // Who this is: the mark and the name, as in
                    // the launcher.
                    .child(
                        h_flex().gap_2().px_2().pb_2().child(logo(22.)).child(
                            div()
                                .text_size(px(15.))
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .child("Gertie"),
                        ),
                    )
                    .child(
                        sidebar_row("new-chat", IconName::SquarePen, "New chat", None, false, cx)
                            .tooltip(|window, cx| {
                                Tooltip::new("Start a new conversation").build(window, cx)
                            })
                            .on_click(
                                cx.listener(|this, _, window, cx| this.new_session(window, cx)),
                            )
                            .test_support(),
                    )
                    // A worktree chat: its own checkout of this
                    // repo, for parallel work that must not
                    // collide. Only where a shell can make one.
                    .when(self.shell_ask.is_some(), |col| {
                        col.child(
                            sidebar_row(
                                "new-worktree-chat",
                                IconName::GitBranchPlus,
                                "New worktree chat",
                                None,
                                false,
                                cx,
                            )
                            .tooltip(|window, cx| {
                                Tooltip::new(
                                    "A new chat in its own git worktree of this repo, \
                                     so parallel edits never collide",
                                )
                                .build(window, cx)
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(ask) = this.shell_ask.clone() {
                                    ask(ShellRequest::NewWorktreeChat, window, cx);
                                }
                            }))
                            .test_support(),
                        )
                    })
                    .child(
                        div()
                            .px_2()
                            .pt_4()
                            .pb_1()
                            .text_size(px(11.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(cx.theme().muted_foreground)
                            .child("SESSIONS"),
                    )
                    .children(self.active_session_rows(&visible, cx))
                    // The settled fold: put-down work, collapsed
                    // by default, findable, never in the way.
                    .when(visible.iter().any(|summary| summary.settled), |col| {
                        let count = visible.iter().filter(|summary| summary.settled).count();
                        col.child(
                            div()
                                .id("settled-fold")
                                .test_support()
                                .mt_2()
                                .px_2()
                                .h(px(26.))
                                .flex()
                                .items_center()
                                .gap_1()
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_size(px(11.))
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .text_color(cx.theme().muted_foreground)
                                .hover(|row| row.text_color(cx.theme().foreground))
                                .child(
                                    Icon::new(if self.settled_open {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .with_size(px(12.)),
                                )
                                .child(format!("SETTLED ({count})"))
                                .tooltip(|window, cx| {
                                    Tooltip::new("Conversations put down, expand to bring one back")
                                        .build(window, cx)
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.settled_open = !this.settled_open;
                                    cx.notify();
                                })),
                        )
                        .when(self.settled_open, |col| {
                            col.children(self.settled_session_rows(&visible, cx))
                        })
                    }),
            )
            .child(
                // The utilities: stacked at the
                // sidebar's foot, findable, never
                // in the way.
                v_flex()
                    .gap_0p5()
                    .px_2()
                    .pt_2()
                    .pb_3()
                    // The undo toast: sessions inside their
                    // five-second window, newest first. One
                    // line, one button, the whole mechanism.
                    .when(!self.pending_deletes.is_empty(), |util| {
                        let newest = self.pending_deletes.last().expect("non-empty just checked");
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
                                            format!("deleted “{}” and {} more", newest.title, older)
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
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.undo_delete_clicked(cx)
                                        })),
                                ),
                        )
                    })
                    .child(
                        sidebar_row(
                            "search-history",
                            IconName::Search,
                            "Search",
                            Some("Ctrl+K"),
                            false,
                            cx,
                        )
                        .tooltip(|window, cx| {
                            Tooltip::new("Search every session").build(window, cx)
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.open_palette(window, cx)))
                        .test_support(),
                    )
                    .child(
                        sidebar_row(
                            "toggle-settings",
                            IconName::Settings,
                            "Settings",
                            None,
                            self.panel == Panel::Settings,
                            cx,
                        )
                        .tooltip(|window, cx| {
                            Tooltip::new("Configuration, theme, and token totals").build(window, cx)
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_settings(cx)))
                        .test_support(),
                    ),
            )
            .into_any_element()
    }
}
