//! Turn orchestration as its own module: driving the loop's event
//! stream (streaming, tool cards, compaction, steering, limits),
//! stop/resume/snooze, worker-ask bubbling. One impl ChatView block
//! (issue #16 step 5); the methods it calls stay on ChatView.

use super::*;

impl ChatView {
    /// Hands the turn to the agent loop and renders its event stream:
    /// deltas stream into an assistant row, tool calls become cards, and
    /// results persist as tool rows the model also sees on reload.
    /// `user_text` None means *resume*, continue from history as it
    /// stands, no new user row.
    pub(crate) fn drive_turn(
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
        // The turn's routing token (issue #15): minted before the
        // spec, it rides the spec (worker asks stamp with it) and the
        // view's subscription filter (this view receives only its own
        // worker's asks).
        let turn_token = self.broker.next_turn_token();
        // Worker dispatch reads the posture the same way: set once per
        // turn, read at their spawn, never mutated mid-turn.
        self.mode_scope.set(self.mode);
        // Steering: the channel lives exactly as long as the turn,
        // Enter while streaming lands here; at the turn's end the
        // sender drops and Enter queues instead.
        let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
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
            turn_token: Some(turn_token),
            system_prompt: self.system_prompt.clone(),
            instructions: self.instructions.clone(),
            memory_index: self.memory_index.clone(),
            memory: self.memory_store.clone().filter(|_| self.memory_enabled),
            verify: self.verify.clone(),
        });
        self.streaming = true;
        self.streaming_assistant = None;
        // Turn start: fresh narration is not yet available (the model
        // has not spoken this round), so the plain label until its
        // first words stream or a tool runs.
        self.set_activity("Thinking…");
        // Turn start: fresh narration is not yet available (the model
        // has not spoken this round), so the plain label until its
        // first words stream or a tool runs.
        self.set_activity("Thinking…");
        // Subagent asks arrive here, not on the leader's stream: a
        // worker's loop runs inside a tool, so nobody drains its
        // events. The filter keeps other turns' worker asks away.
        let mut worker_asks = self
            .broker
            .subscribe(gertie_core::AskFilter::Turn(turn_token));
        self.steer_tx = Some(steer_tx);
        self.stop_tx = Some(stop_tx);
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
            let mut stopped = false;
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
                    // The model's view shrank; the transcript keeps
                    // every read whole, so there is nothing to redraw.
                    LoopEvent::Pruned { .. } => {}
                    // The check's bash card and result are already in
                    // the transcript; the verdict needs no extra row.
                    LoopEvent::Checked { .. } => {}
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
                    // Stop: leaving the loop drops the stream, and the
                    // turn's future with it (cancellation by
                    // construction).
                    Some(()) = stop_rx.recv() => {
                        stopped = true;
                        break;
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
            drop(stream);
            let _ = this.update(cx, |this, cx| {
                if stopped {
                    this.on_stopped(cx);
                }
                this.flush_stream(cx);
                this.streaming = false;
                this.stop_tx = None;
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
                // A stopped turn starts nothing: the queue waits for
                // the next send.
                if !stopped && !this.queued.is_empty() {
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
    pub(crate) fn on_delta(&mut self, delta: &str, cx: &mut Context<Self>) {
        self.set_activity("Writing a reply…");
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
    pub(crate) fn flush_stream(&mut self, cx: &mut Context<Self>) {
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
    pub(crate) fn on_tool_started(&mut self, call: ToolCall, cx: &mut Context<Self>) {
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
        // The label just set is this new card's own: the echo flag
        // makes the row yield to the card (while the user is pinned
        // to the bottom), until a worker step or a delta speaks.
        self.activity_echoes_card = true;
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
    pub(crate) fn on_tool_requested(&mut self, id: u64, call: ToolCall, cx: &mut Context<Self>) {
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
    pub(crate) fn on_worker_ask(&mut self, id: u64, call: ToolCall, cx: &mut Context<Self>) {
        let label = crate::tools_ui::action_label(&call.name, &call.arguments, true);
        self.set_activity(format!("A worker needs you: {label}"));
        if call.name == "write_file"
            && let Some(content) = self.current_file_content(&call)
        {
            self.before_contents.insert(call.id.clone(), content);
        }
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
    pub(crate) fn on_tool_result(
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
        // to when it misses. The row holds the done label while the
        // model chews on the result; the model's narration, when it
        // comes, lives in the transcript where prose belongs.
        self.set_activity(crate::tools_ui::action_label(name, arguments, false));
        if name.starts_with("agent_") {
            // The worker's run is over: its steps leave the progress
            // map, the line belongs to the leader again.
            self.active_worker_call = None;
            self.worker_progress.clear(call_id);
        }
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
        // The status column's facts: touched files, consulted memory,
        // the model's stated goal and next step. The fold bumps the
        // facts version; without a notify the column keeps its
        // last-rendered state until some other repaint happens (the
        // live-update bug the hand test caught).
        self.fold_status_tool(name, arguments, result);
        cx.notify();

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
    pub(crate) fn on_compacted(&mut self, kept: usize, total: usize, cx: &mut Context<Self>) {
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
    pub(crate) fn on_steered(&mut self, text: &str, cx: &mut Context<Self>) {
        // No activity change, a steer doesn't alter what's happening,
        // but the quiet gauge restarts all the same: something happened.
        self.last_event_at = Instant::now();
        self.fold_status_user(text);
        cx.notify();
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

    /// Asks the running turn to stop; the turn's loop does the rest.
    pub fn stop_turn(&mut self, cx: &mut Context<Self>) {
        if let Some(stop) = &self.stop_tx {
            let _ = stop.send(());
            self.set_activity("Stopping…");
            cx.notify();
        }
    }

    /// The turn was stopped: every card still waiting or running
    /// closes as stopped (persisted like any result), and the model's
    /// history gets an answer for each call it made, so the next
    /// request is a legal one.
    pub(crate) fn on_stopped(&mut self, cx: &mut Context<Self>) {
        const STOPPED: &str = "error: stopped by the user";
        self.flush_stream(cx);
        let open: Vec<(String, String, String)> = self
            .turns
            .iter()
            .filter_map(|turn| match turn {
                Turn::Tool {
                    call,
                    status: ToolStatus::Pending(_) | ToolStatus::Running,
                    ..
                } => Some((call.id.clone(), call.name.clone(), call.arguments.clone())),
                _ => None,
            })
            .collect();
        for (id, name, arguments) in open {
            self.on_tool_result(&id, &name, &arguments, STOPPED, cx);
        }
        gertie_core::close_unanswered_calls(&mut self.history.blocking_lock(), STOPPED);
        self.approval_replies.clear();
        self.active_worker_call = None;
        self.push_notice("Stopped.", cx);
    }

    pub(crate) fn on_error(&mut self, message: &str, cx: &mut Context<Self>) {
        self.flush_stream(cx);
        self.set_activity("Something went wrong");
        self.persist_stream_note(format!("[error] {message}"));
        self.list_state.reset(self.item_count());
        self.list_state.scroll_to_end();
        cx.notify();
    }

    /// The turn ended on the provider's rate limit: like an error, but
    /// the state persists, the ask is "wait, then continue", so the
    /// session goes Limited (badge on the row, Resume affordance) and
    /// comes back clear on resume or the next send.
    pub(crate) fn on_limited(&mut self, message: &str, cx: &mut Context<Self>) {
        self.flush_stream(cx);
        self.set_activity("Rate limited, pick it up when you're ready");
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
    pub(crate) fn resume_clicked(&mut self, cx: &mut Context<Self>) {
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
    pub(crate) fn snooze_clicked(&mut self, cx: &mut Context<Self>) {
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
}
