//! The transcript rows and their cards as their own module: the
//! turn renderer (user bubbles, assistant rows, notices), message
//! shaping, the tool card renderer with its faces (ask, diff,
//! previews), and the diff element builder. One impl ChatView block
//! plus the free helper fns (issue #16 step 6, the last).

use super::*;

impl ChatView {
    pub(crate) fn render_turn(
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
                .flex()
                .justify_center()
                .child(
                    div()
                        .w_full()
                        .max_w(px(READING_WIDTH))
                        .child(self.render_activity_row(cx)),
                )
                .into_any_element();
        }
        // The words pane holds words (#54): a call's work lives on the
        // work side, numbered; here a consulted memory becomes a seal
        // note, an approval a pointer to its card, and an ask stays a
        // card (it is a question in the conversation). Everything else
        // renders nothing, the list's index space is the turns'.
        if let Turn::Tool { call, status, .. } = &self.turns[index] {
            match self.kind_of_call(call) {
                super::shape::Kind::Ask => {}
                super::shape::Kind::Memory if call.name == "memory_proposal" => {
                    return words_row(self.render_proposal_seal(index, cx));
                }
                super::shape::Kind::Memory => {
                    return match self.render_memory_seals(index, cx) {
                        Some(seals) => words_row(seals),
                        None => div().w_full().into_any_element(),
                    };
                }
                _ if matches!(status, ToolStatus::Pending(_)) => {
                    return words_row(self.render_waiting_pointer(index, cx));
                }
                _ => return div().w_full().into_any_element(),
            }
        }
        // The reply input for a pending permission card must exist
        // before the turn is borrowed below: creating it takes &mut
        // self (and a window handle, which only render has), the match
        // takes &self for the whole arm. An ask_user card with custom
        // answers allowed gets the same treatment, so the "type your
        // own" field is visible on the card instead of appearing only
        // after some earlier focus.
        if let Some(Turn::Tool {
            status: ToolStatus::Pending(_),
            call,
            ..
        }) = self.turns.get(index)
        {
            let wants_reply =
                call.name == "ask_user" && ask_face(call).is_some_and(|face| face.allow_custom);
            if wants_reply {
                self.approval_reply_for(index, window, cx);
            }
        }
        // An empty assistant row (a reasoning-only reply, a turn that
        // went straight to tools) holds no words: no row, no margin.
        if let Turn::Message {
            role: Role::Assistant,
            text,
            ..
        } = &self.turns[index]
            && text.trim().is_empty()
            && self.streaming_assistant != Some(index)
        {
            return div().w_full().into_any_element();
        }
        let inner = match &self.turns[index] {
            Turn::Message { role, text, .. } => {
                // The riding card (#34) is not here: it is the
                // activity row at the transcript's end, which is the
                // latest turn by construction and leaves with the run.
                self.render_message(role, text, index as u64, cx)
            }
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
        // The thread marks (#54): an assistant paragraph carries the
        // numbers of the work it announced, matching the work side.
        let inner = match &self.turns[index] {
            Turn::Message {
                role: Role::Assistant,
                ..
            } => {
                let marks = self.marks_after(index);
                if marks.is_empty() {
                    inner
                } else {
                    let pal = crate::ink::palette(cx);
                    v_flex()
                        .w_full()
                        .child(inner)
                        .child(h_flex().gap_1p5().pb_2().children(marks.into_iter().map(
                            |(number, live)| {
                                super::panes::live_mark(
                                    number,
                                    live,
                                    false,
                                    pal,
                                    &format!("words-{index}"),
                                )
                            },
                        )))
                        .into_any_element()
                }
            }
            _ => inner,
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
                        .icon(IconName::GitFork)
                        .xsmall()
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
                    .icon(IconName::Undo2)
                    .xsmall()
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
                                .icon(IconName::Copy)
                                .xsmall()
                                .ghost()
                                .tooltip("Copy the raw markdown of this turn")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.copy_turn_to_clipboard(copy_text.clone(), cx);
                                })),
                        )
                        .child(
                            Button::new(format!("quote-{index}"))
                                .icon(IconName::TextQuote)
                                .xsmall()
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
        // The reply that ends its turn: no later assistant message
        // before the next user message.
        let last_reply = self.turns[index + 1..]
            .iter()
            .take_while(|turn| {
                !matches!(
                    turn,
                    Turn::Message {
                        role: Role::User,
                        ..
                    }
                )
            })
            .all(|turn| {
                !matches!(
                    turn,
                    Turn::Message {
                        role: Role::Assistant,
                        ..
                    }
                )
            });
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
                .relative()
                .flex()
                .flex_col()
                .items_start()
                .child(inner)
                .child(
                    // Assistant rows: the reply that ends a turn keeps
                    // its copy and quote ghosted (35%) at rest, where
                    // they are wanted; earlier narration in the turn
                    // ("let me check…") shows them only on hover, so
                    // the transcript is not dotted with icons. Opacity,
                    // not visibility: the row stays present and
                    // clickable (the harness's synthetic hover can't
                    // drive gpui's group-hover state).
                    // Earlier narration's row floats in the right margin
                    // (#54): consecutive paragraphs keep their rhythm,
                    // no hole where hidden icons would sit.
                    h_flex()
                        .gap_1()
                        .when(!user_turn && !last_reply, |row| {
                            row.absolute().top_0().right(px(-52.)).flex_col()
                        })
                        .when(!user_turn, |row| {
                            row.opacity(if last_reply { 0.35 } else { 0. })
                        })
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
        words_row(inner)
    }

    pub(crate) fn render_message(&self, role: &Role, text: &str, key: u64, cx: &App) -> AnyElement {
        match role {
            // The human's words, quiet: the question is context for
            // what follows, the reply is what the eye should land on.
            Role::User => {
                let pal = crate::ink::palette(cx);
                div()
                    .w_full()
                    .pb_1()
                    .font_family(crate::ink::SANS)
                    .text_size(px(14.5))
                    .line_height(gpui_kit::relative(1.6))
                    .text_color(pal.text_past)
                    .child(
                        h_flex()
                            .flex_wrap()
                            .items_baseline()
                            .gap_x_1()
                            .child(
                                div()
                                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                    .text_color(pal.text_muted)
                                    .child("you ·"),
                            )
                            .child(display_text(text)),
                    )
                    .into_any_element()
            }
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
                // The prose face: serif, large, unhurried (#54).
                div()
                    .w_full()
                    .pb_2()
                    .font_family(crate::ink::SERIF)
                    .text_size(px(17.5))
                    .line_height(gpui_kit::relative(1.7))
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
    pub(crate) fn render_tool(
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
        let (label_running, label_done, diff) = self.tool_face_for(index, call);
        let label = if running { label_running } else { label_done };

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
                    let step = self
                        .tool_starts
                        .get(&call.id)
                        .map(|clock_at| clock_at.elapsed().as_millis() as usize / 500)
                        .unwrap_or(0);
                    let frame = SPINNER_FRAMES[step % SPINNER_FRAMES.len()];
                    row.child(
                        div()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{frame} {}", self.running_clock_words(&call.id))),
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
                })
                // Provenance: the answer came from another chat's
                // status column (issue #33), not a click here.
                .when(self.answered_from_column.contains(&call.id), |row| {
                    row.child(
                        div()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child("answered from the status column"),
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
            inner = inner.child(
                div()
                    .text_size(px(13.))
                    .child(SharedString::from(ask.question)),
            );
            // The option chips wrap their full text (Button clips at
            // one line, and a choice you cannot read is not a
            // choice) and stack full width, so the card reads as a
            // form, one decision per row.
            for (option_index, option) in ask.options.iter().cloned().enumerate() {
                let answer = option.clone();
                let chip = div()
                    .id(SharedString::from(format!("ask-{index}-{option_index}")))
                    .test_support()
                    .w_full()
                    .px_2()
                    .py_1p5()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(cx.theme().border)
                    .text_size(px(12.))
                    .text_color(cx.theme().foreground)
                    .cursor_pointer()
                    .hover(|style| {
                        style
                            .text_color(cx.theme().blue)
                            .border_color(cx.theme().blue)
                    })
                    .child(SharedString::from(option));
                inner = inner.child(chip.on_click(cx.listener(move |this, _, _, cx| {
                    this.decide_tool(
                        index,
                        PermissionDecision::Deny {
                            feedback: Some(answer.clone()),
                        },
                        cx,
                    )
                })));
            }
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
                    // Approve is the ask's "closed unanswered" (answers
                    // ride Deny's feedback); the loop words it as a
                    // refusal. Changing this variant changes that
                    // meaning: see the ask arm in agent_loop.rs.
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

/// A row of the words pane: its margins (#54), the reading width.
pub(crate) fn words_row(inner: AnyElement) -> AnyElement {
    div()
        .w_full()
        .pl(px(48.))
        .pr(px(56.))
        .mt_3()
        .flex()
        .child(div().w_full().max_w(px(READING_WIDTH)).child(inner))
        .into_any_element()
}

/// The waiting-on-you group's face: a left accent edge, not a box.
/// An approval is the transcript's landmark while it waits, and the
/// mark should say "this needs you", not "here is a panel". The
/// content stays uncrated: the row, the diff, the buttons.
pub(crate) fn ask_group(inner: impl IntoElement, cx: &App) -> AnyElement {
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
pub(crate) fn resolve_tool_path(
    root: &std::path::Path,
    path: &std::path::Path,
) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

impl ChatView {
    /// A card's label and diff data, parsed once per card and memoized
    /// by row (see ToolFace); the elements rebuild per frame from it.
    pub(crate) fn tool_face_for(
        &self,
        index: usize,
        call: &ToolCall,
    ) -> (String, String, Option<crate::diff::Patch>) {
        let mut cache = self.tool_face.borrow_mut();
        let face = cache.entry(index).or_insert_with(|| {
            // The write's before-content, three sources in honesty
            // order: the pre-run capture when this session saw the
            // request land, the checkpoint the loop recorded when the
            // call was approved (a reloaded card's exact source), else
            // the file as it sits now.
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
            face.label_running.clone(),
            face.label_done.clone(),
            face.diff.clone(),
        )
    }

    /// What the workspace file holds right now, for a write card's
    /// before-content. The exact sources are the pre-run capture taken
    /// when the call arrived (`before_contents`) and, for a reloaded
    /// card, the checkpoint the loop recorded for the call; this disk
    /// read is the last fallback, for a card with neither, and it can
    /// only guess: the file has usually moved on.
    pub(crate) fn current_file_content(&self, call: &ToolCall) -> Option<String> {
        if call.name != "write_file" {
            return None;
        }
        let value: serde_json::Value = serde_json::from_str(&call.arguments).ok()?;
        let path = value.get("path")?.as_str()?;
        // The tools' own rules: inside the workspace, a regular file,
        // under the read cap. The model names this path before anyone
        // approves anything.
        gertie_tools::read_workspace_file(&self.workspace_root, path).ok()
    }

    /// The ledger's before-content for a write card: the checkpoint
    /// the loop recorded the moment this very call was approved,
    /// keyed by the call's own id. The exact source for a reloaded
    /// card, where the live capture is long gone and the file has
    /// usually moved on. Legacy ledgers (rows older than call ids)
    /// and calls that never ran answer None, the disk fallback
    /// follows.
    pub(crate) fn checkpointed_before(&self, call: &ToolCall) -> Option<String> {
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
pub(crate) fn ask_face(call: &ToolCall) -> Option<AskFace> {
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

pub(crate) struct AskFace {
    pub(crate) question: String,
    pub(crate) options: Vec<String>,
    pub(crate) allow_custom: bool,
}

/// Context lines a card's diff keeps on each side of a change when it
/// trims long stretches of unchanged lines.
const HUNK_CONTEXT: usize = 3;

/// One display row of a card's diff: a line, or an elision where a run
/// of unchanged lines was trimmed.
#[derive(Debug)]
pub(crate) enum DiffRow<'a> {
    Line(&'a crate::diff::Line),
    Elided,
}

/// Trims runs of unchanged lines to [`HUNK_CONTEXT`] beside each
/// change. Every added or removed line stays; only context collapses,
/// and the counts above the diff were whole before this ran, so the
/// trim costs nothing honest.
pub(crate) fn hunk_rows(lines: &[crate::diff::Line]) -> Vec<DiffRow<'_>> {
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
pub(crate) fn diff_data(call: &ToolCall, before: Option<&str>) -> Option<crate::diff::Patch> {
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
pub(crate) fn diff_element(patch: &crate::diff::Patch, cx: &App) -> AnyElement {
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
