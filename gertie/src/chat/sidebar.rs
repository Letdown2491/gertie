//! The session sidebar as its own module: the rows (active and
//! settled), the rename/delete/undo affordances, the utility buttons.
//! One `impl ChatView` block: no new types, the render body moved
//! whole (issue #16 step 3), the methods it calls stay on ChatView.

use super::*;

impl ChatView {
    /// Folds the sidebar to its rail (or back), persisted as one
    /// app-wide store flag: the sidebar is chrome, not session state.
    pub(crate) fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        let _ = self.store.set_store_flag(
            crate::chat::status::SIDEBAR_EXPANDED_FLAG,
            !self.sidebar_collapsed,
        );
        cx.notify();
    }

    /// The rail (#54), the window's default edge: Gertie's mark, one
    /// circle per project (the current one filled, a ring while one of
    /// its chats runs, a vermilion dot when one needs you), and the
    /// utilities at the foot. Any circle opens the full sidebar, where
    /// the chats, renames and moves live.
    fn render_sidebar_rail(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = crate::ink::palette(cx);
        let current_project = self.project_id();
        let mut groups: Vec<(Option<i64>, String)> = self
            .projects
            .iter()
            .map(|project| (Some(project.id), project.name.clone()))
            .collect();
        groups.push((None, "Other".to_string()));
        let circles = groups.into_iter().filter_map(|(project, name)| {
            let sessions: Vec<&gertie_core::SessionSummary> = self
                .sessions
                .iter()
                .filter(|summary| summary.project_id == project)
                .collect();
            if sessions.is_empty() && project.is_none() {
                return None;
            }
            let mark_of = |wanted: SessionMark| {
                sessions
                    .iter()
                    .any(|summary| self.session_marks.get(&summary.id) == Some(&wanted))
            };
            let current = current_project == project;
            let running = mark_of(SessionMark::Running) || (current && self.streaming);
            let needs_you = mark_of(SessionMark::Limited)
                || (current && (self.waiting_on_human() || !self.project_asks.is_empty()));
            let unseen = mark_of(SessionMark::Unseen);
            let letters: String = name
                .chars()
                .filter(|c| c.is_alphanumeric())
                .take(2)
                .collect::<String>()
                .to_lowercase();
            let count = sessions.len();
            let tip = format!(
                "{name} · {count} chat{}{}",
                if count == 1 { "" } else { "s" },
                if needs_you {
                    " · needs you"
                } else if running {
                    " · running"
                } else {
                    ""
                }
            );
            let id = SharedString::from(format!("rail-project-{}", project.unwrap_or(0)));
            Some(
                div()
                    .id(id)
                    .test_support()
                    .relative()
                    .size(px(44.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_size(px(12.))
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .when(current, |circle| circle.bg(pal.text).text_color(pal.paper))
                    .when(!current, |circle| {
                        circle
                            .border_1()
                            .border_color(pal.control)
                            .text_color(pal.text_muted)
                            .hover(|style| style.border_color(pal.text).text_color(pal.text))
                    })
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .on_click(
                        cx.listener(move |this, event: &gpui_kit::ClickEvent, _, cx| {
                            let y: f32 = event.position().y.into();
                            this.rail_popover = match this.rail_popover {
                                Some((open, _)) if open == project => None,
                                _ => Some((project, y)),
                            };
                            cx.notify();
                        }),
                    )
                    .child(letters)
                    .when(running, |circle| {
                        // The breathing ring: a chat in this project runs.
                        let ring = div()
                            .absolute()
                            .top(px(-4.))
                            .left(px(-4.))
                            .size(px(52.))
                            .rounded_full()
                            .border_1()
                            .border_color(pal.text.opacity(0.7));
                        circle.child(crate::chat::panes::pulsing(
                            ring,
                            SharedString::from(format!("rail-breath-{}", project.unwrap_or(0))),
                            2400,
                        ))
                    })
                    .when(needs_you || unseen, |circle| {
                        circle.child(
                            div()
                                .absolute()
                                .top(px(1.))
                                .right(px(1.))
                                .size(px(10.))
                                .rounded_full()
                                .border_2()
                                .border_color(pal.rail)
                                .bg(if needs_you {
                                    pal.vermilion
                                } else {
                                    pal.text_muted
                                }),
                        )
                    }),
            )
        });
        v_flex()
            .id("sidebar-rail")
            .w(px(RAIL_WIDTH))
            .h_full()
            .flex_shrink_0()
            .items_center()
            .border_r_1()
            .border_color(pal.line)
            .bg(pal.rail)
            .pt_4()
            .pb_4()
            .gap_3p5()
            .key_context("Sidebar")
            .track_focus(&self.sidebar_focus)
            .child(
                div()
                    .id("sidebar-expand")
                    .cursor_pointer()
                    .tooltip(|window, cx| {
                        Tooltip::new("Gertie · open the sidebar").build(window, cx)
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)))
                    .child(logo(36.))
                    .test_support(),
            )
            .child(div().h(px(6.)))
            .child(
                v_flex()
                    .id("rail-projects")
                    .flex_1()
                    .min_h_0()
                    .items_center()
                    .gap_3p5()
                    .pt_1()
                    .overflow_y_scroll()
                    .children(circles),
            )
            .child(
                div()
                    .id("rail-new-chat")
                    .size(px(44.))
                    .rounded_full()
                    .border_1()
                    .border_dashed()
                    .border_color(pal.text_muted)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_color(pal.text_muted)
                    .hover(|style| style.text_color(pal.text).border_color(pal.text))
                    .tooltip(|window, cx| Tooltip::new("New chat").build(window, cx))
                    .on_click(cx.listener(|this, _, window, cx| this.new_session(window, cx)))
                    .child(Icon::new(IconName::Plus).with_size(px(16.)))
                    .test_support(),
            )
            .when(self.shell_ask.is_some(), |rail| {
                rail.child(
                    rail_icon(
                        "rail-worktree-chat",
                        IconName::GitBranch,
                        "New worktree chat: its own checkout, so parallel edits never collide",
                        cx,
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(ask) = this.shell_ask.clone() {
                            ask(ShellRequest::NewWorktreeChat, window, cx);
                        }
                    })),
                )
            })
            .child(
                rail_icon("rail-search", IconName::Search, "Search (Ctrl+K)", cx)
                    .on_click(cx.listener(|this, _, window, cx| this.open_palette(window, cx))),
            )
            .child(
                rail_icon("rail-settings", IconName::Settings, "Settings", cx)
                    .when(self.panel == Panel::Settings, |icon| {
                        icon.bg(cx.theme().foreground.opacity(0.09))
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_settings(window, cx))),
            )
            .children(self.render_rail_popover(cx))
            .test_support()
            .into_any_element()
    }

    /// The project popover off the rail (#54): the project's chats,
    /// newest first (title, state, age), the newest twelve and the rest
    /// in Ctrl+K; a new chat filed there; the full sidebar a click away.
    fn render_rail_popover(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (project, y) = self.rail_popover?;
        let pal = crate::ink::palette(cx);
        let name = match project {
            Some(id) => self.projects.iter().find(|p| p.id == id)?.name.clone(),
            None => "Other".to_string(),
        };
        let chats: Vec<&gertie_core::SessionSummary> = self
            .sessions
            .iter()
            .filter(|summary| summary.project_id == project && !summary.settled)
            .collect();
        const SHOWN: usize = 12;
        let more = chats.len().saturating_sub(SHOWN);
        let rows = chats.iter().take(SHOWN).map(|summary| {
            let session_id = summary.id;
            let stored_root = summary.workspace_root.clone();
            let mark = self.session_marks.get(&session_id).copied();
            let active = self.session_id == Some(session_id);
            let running = mark == Some(SessionMark::Running) || (active && self.streaming);
            let needs_you =
                mark == Some(SessionMark::Limited) || (active && self.waiting_on_human());
            let unseen = mark == Some(SessionMark::Unseen);
            let state = div()
                .size(px(8.))
                .flex_shrink_0()
                .rounded_full()
                .map(|dot| {
                    if needs_you {
                        dot.bg(pal.vermilion)
                    } else if running {
                        dot.border_1().border_color(pal.text)
                    } else if unseen {
                        dot.bg(pal.text_muted)
                    } else {
                        dot
                    }
                });
            h_flex()
                .id(SharedString::from(format!("rail-chat-{}", session_id.0)))
                .test_support()
                .gap_2()
                .items_center()
                .px_2()
                .py_1p5()
                .rounded(px(8.))
                .cursor_pointer()
                .when(active, |row| row.bg(pal.paper_raised))
                .hover(|row| row.bg(pal.paper_raised))
                .child(state)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(crate::ink::SERIF)
                        .text_size(px(14.))
                        .text_color(pal.text)
                        .child(summary.title.clone()),
                )
                .children(summary.idle_secs.map(|secs| {
                    div()
                        .flex_shrink_0()
                        .font_family(crate::ink::SANS)
                        .text_size(px(11.5))
                        .text_color(pal.text_muted)
                        .child(age_words(secs))
                }))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.rail_popover = None;
                    this.open_chat(session_id, stored_root.clone(), window, cx);
                }))
        });
        let top = (y - 28.).max(8.);
        let panel = v_flex()
            .id("rail-popover")
            .test_support()
            .occlude()
            .w(px(300.))
            .max_h(px(460.))
            .p_2()
            .gap_0p5()
            .rounded(px(14.))
            .border_1()
            .border_color(pal.line)
            .bg(pal.paper)
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.rail_popover = None;
                cx.notify();
            }))
            .child(
                h_flex()
                    .px_2()
                    .pt_1()
                    .pb_2()
                    .items_baseline()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(crate::ink::SERIF)
                            .text_size(px(17.))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(pal.text)
                            .child(name),
                    )
                    .child(
                        div()
                            .font_family(crate::ink::SANS)
                            .text_size(px(11.5))
                            .text_color(pal.text_muted)
                            .child(match chats.len() {
                                0 => "no open chats".to_string(),
                                1 => "1 chat".to_string(),
                                count => format!("{count} chats"),
                            }),
                    ),
            )
            .child(
                v_flex()
                    .id("rail-popover-chats")
                    .min_h_0()
                    .flex_shrink(1.)
                    .overflow_y_scroll()
                    .children(rows),
            )
            .when(more > 0, |panel| {
                panel.child(
                    div()
                        .px_2()
                        .py_1()
                        .font_family(crate::ink::SANS)
                        .text_size(px(11.5))
                        .text_color(pal.text_muted)
                        .child(format!("and {more} more in Ctrl+K")),
                )
            })
            .child(
                h_flex()
                    .mt_1()
                    .pt_2()
                    .px_1()
                    .gap_1()
                    .border_t_1()
                    .border_color(pal.line)
                    .when_some(project, |row, id| {
                        row.child(
                            Button::new("rail-popover-new")
                                .label("New chat here")
                                .icon(IconName::Plus)
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.rail_popover = None;
                                    this.new_chat_in(id, window, cx);
                                })),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("rail-popover-sidebar")
                            .label("Open the sidebar")
                            .xsmall()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.rail_popover = None;
                                this.toggle_sidebar(cx);
                            })),
                    ),
            );
        Some(
            gpui_kit::deferred(
                gpui_kit::anchored()
                    .position(gpui_kit::point(px(RAIL_WIDTH + 8.), px(top)))
                    .snap_to_window_with_margin(px(8.))
                    .child(panel),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    /// The sidebar column, whole: sessions scroll, the utility
    /// buttons stay put at the foot. A focus area (F6): the
    /// container tracks focus so the cycle can find it.
    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.sidebar_collapsed {
            return self.render_sidebar_rail(cx);
        }
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
                    .track_scroll(&self.sidebar_scroll)
                    // Who this is: the mark and the name, as in
                    // the launcher.
                    .child(
                        h_flex()
                            .gap_2()
                            .px_2()
                            .pb_2()
                            .child(logo(22.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(15.))
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .child("Gertie"),
                            )
                            .child(
                                // The fold affordance, mirroring the
                                // status column's: same button, mirrored
                                // chevron, in the header where folds live.
                                Button::new("sidebar-collapse")
                                    .label("‹")
                                    .small()
                                    .ghost()
                                    .tooltip("Fold the sidebar")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)),
                                    ),
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
                    .children(self.grouped_rows(&visible, cx))
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
                                .child(
                                    div()
                                        .font_family(crate::ink::SERIF)
                                        .text_size(px(13.))
                                        .font_weight(gpui_kit::FontWeight::NORMAL)
                                        .child(format!("settled ({count})")),
                                )
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
                        .on_click(
                            cx.listener(|this, _, window, cx| this.toggle_settings(window, cx)),
                        )
                        .test_support(),
                    ),
            )
            .into_any_element()
    }

    /// The active list: a header with the add-project button, then one
    /// group per project (its chats under a collapsible header) and an
    /// "Other" group for chats outside every project. With no projects
    /// the list is flat, as before projects existed.
    fn grouped_rows(
        &self,
        visible: &[gertie_core::SessionSummary],
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = vec![self.list_header(cx)];
        if self.creating_project {
            rows.push(
                h_flex()
                    .w_full()
                    .mt_1()
                    .child(Input::new(&self.rename_input).small())
                    .into_any_element(),
            );
        }
        let active: Vec<&gertie_core::SessionSummary> =
            visible.iter().filter(|summary| !summary.settled).collect();
        if self.projects.is_empty() {
            rows.extend(active.iter().map(|summary| self.session_row(summary, cx)));
            return rows;
        }
        let owner = |summary: &gertie_core::SessionSummary| {
            gertie_core::project_of(&self.projects, summary).map(|project| project.id)
        };
        for project in &self.projects {
            let chats: Vec<_> = active
                .iter()
                .filter(|summary| owner(summary) == Some(project.id))
                .collect();
            rows.push(self.project_header(project, chats.len(), cx));
            if !project.collapsed {
                rows.extend(chats.iter().map(|summary| {
                    div()
                        .pl_3()
                        .child(self.session_row(summary, cx))
                        .into_any_element()
                }));
            }
        }
        let others: Vec<_> = active
            .iter()
            .filter(|summary| owner(summary).is_none())
            .collect();
        if !others.is_empty() {
            rows.push(self.other_header(others.len(), cx));
            if !self.other_collapsed {
                rows.extend(others.iter().map(|summary| {
                    div()
                        .pl_3()
                        .child(self.session_row(summary, cx))
                        .into_any_element()
                }));
            }
        }
        rows
    }

    fn list_header(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .pl_2()
            .pt_4()
            .pb_1()
            .items_center()
            .child(
                // Lowercase serif section names (#54), never caps.
                div()
                    .flex_1()
                    .font_family(crate::ink::SERIF)
                    .text_size(px(13.5))
                    .text_color(cx.theme().muted_foreground)
                    .child(if self.projects.is_empty() {
                        "chats"
                    } else {
                        "projects"
                    }),
            )
            .child(
                Button::new("add-project")
                    .icon(IconName::FolderPlus)
                    .xsmall()
                    .ghost()
                    .tooltip("New project: a group to file conversations under")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.add_project_clicked(window, cx)),
                    ),
            )
            .into_any_element()
    }

    /// One project's header: the fold chevron, the name (the chat count
    /// while folded), a + for a new chat filed there, and on hover
    /// rename and remove.
    fn project_header(
        &self,
        project: &gertie_core::Project,
        chats: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = project.id;
        if self.renaming_project == Some(id) {
            return h_flex()
                .w_full()
                .mt_1()
                .child(Input::new(&self.rename_input).small())
                .into_any_element();
        }

        let group = SharedString::from(format!("project-row-{id}"));
        let hover_bg = cx.theme().foreground.opacity(0.06);
        let action = |name: String, icon: IconName, tip: &'static str| {
            Button::new(SharedString::from(name))
                .icon(icon)
                .xsmall()
                .ghost()
                .tooltip(tip)
        };
        div()
            .id(SharedString::from(format!("project-{id}")))
            .test_support()
            .group(group.clone())
            .w_full()
            .min_w_0()
            .h(px(28.))
            .mt_1()
            .pl_1()
            .pr_1()
            .flex()
            .items_center()
            .gap_1()
            .rounded(px(6.))
            .cursor_pointer()
            .hover(move |row| row.bg(hover_bg))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_project(id, cx)))
            .child(
                Icon::new(if project.collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .with_size(px(12.))
                .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(project.name.clone()),
            )
            .when(project.collapsed && chats > 0, |row| {
                row.child(
                    div()
                        .text_size(px(11.))
                        .text_color(cx.theme().muted_foreground)
                        .child(chats.to_string()),
                )
            })
            .child(
                h_flex()
                    .flex_shrink_0()
                    .opacity(0.)
                    .group_hover(group, |actions| actions.opacity(1.))
                    .child(
                        action(
                            format!("project-rename-{id}"),
                            IconName::Pencil,
                            "Rename this project",
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.start_project_rename(id, window, cx);
                            },
                        )),
                    )
                    .child(
                        action(
                            format!("project-remove-{id}"),
                            IconName::FolderMinus,
                            "Remove from the list (the folder and its chats stay)",
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.remove_project_clicked(id, cx);
                        })),
                    ),
            )
            .child(
                action(
                    format!("project-new-{id}"),
                    IconName::Plus,
                    "New chat in this project",
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.new_chat_in(id, window, cx);
                })),
            )
            .into_any_element()
    }

    /// The "Other" group's header: chats not filed under any project.
    fn other_header(&self, chats: usize, cx: &mut Context<Self>) -> AnyElement {
        let hover_bg = cx.theme().foreground.opacity(0.06);
        div()
            .id("project-other")
            .test_support()
            .w_full()
            .h(px(28.))
            .mt_1()
            .pl_1()
            .pr_2()
            .flex()
            .items_center()
            .gap_1()
            .rounded(px(6.))
            .cursor_pointer()
            .hover(move |row| row.bg(hover_bg))
            .tooltip(|window, cx| Tooltip::new("Chats not in any project").build(window, cx))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_other(cx)))
            .child(
                Icon::new(if self.other_collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .with_size(px(12.))
                .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .child("Other"),
            )
            .when(self.other_collapsed, |row| {
                row.child(
                    div()
                        .text_size(px(11.))
                        .text_color(cx.theme().muted_foreground)
                        .child(chats.to_string()),
                )
            })
            .into_any_element()
    }
}

/// The folded sidebar's one icon row: a 32px square, the glyph at
/// full foreground strength, the name in the tooltip. Icon-only —
/// a 44px rail cannot show words, and `sidebar_row`'s label would
/// clip.
fn rail_icon(
    id: &'static str,
    icon: IconName,
    tip: &'static str,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let theme = cx.theme();
    let (fg, hover_bg) = (theme.foreground, theme.foreground.opacity(0.06));
    div()
        .id(id)
        .h(px(44.))
        .w(px(44.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .text_color(fg)
        .hover(move |row| row.bg(hover_bg).text_color(fg))
        .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
        .child(Icon::new(icon).with_size(px(18.)))
}

/// A chat's age in a few characters: "now", "5m", "3h", "2d", "6w".
pub(crate) fn age_words(secs: u64) -> String {
    match secs {
        0..60 => "now".to_string(),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        86_400..1_209_600 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
}

/// The rail's width (#54).
pub(crate) const RAIL_WIDTH: f32 = 72.;

#[cfg(test)]
mod tests {
    #[test]
    fn ages_read_in_a_few_characters() {
        assert_eq!(super::age_words(5), "now");
        assert_eq!(super::age_words(300), "5m");
        assert_eq!(super::age_words(7_200), "2h");
        assert_eq!(super::age_words(3 * 86_400), "3d");
        assert_eq!(super::age_words(30 * 86_400), "4w");
    }
}
