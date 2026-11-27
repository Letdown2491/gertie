//! The agent loop: request the model, execute its tool calls under the
//! permission policy, feed results back, repeat, bounded.

use crate::Role;
use crate::permissions::PermissionMode;
use crate::request::{Request, ToolCall, ToolSpec, WireMessage};
use crate::snapshot::SnapshotStore;
use crate::tool::Tool;
use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use std::sync::Arc;

/// Safety valve: a turn that loops this many tool rounds without finishing
/// gets the wrap-up nudge, a grace window to land it, then the turn
/// pauses resumable rather than dies. Runaway loops are how token bills
/// happen; the pause keeps the human in the loop instead of burning a
/// session's history on an error.
/// The round budget, a public constant because the settings panel
/// displays it beside the field that overrides it.
pub const DEFAULT_MAX_TOOL_ROUNDS: usize = 60;
/// The grace window after the nudge: a model that ignores the wrap-up
/// ask this many rounds in a row pauses the turn.
pub const DEFAULT_GRACE_ROUNDS: usize = 10;

/// What the UI (or any embedder) observes while a turn runs.
pub enum LoopEvent {
    /// Assistant text arrived.
    Delta {
        delta: String,
    },
    /// A mutating tool wants to run. The ask is registered with the
    /// broker under `id`, answer through `PermissionBroker::respond`;
    /// the loop waits until answered (a dropped answer counts as a deny
    ///, if the UI went away, doing nothing is the safe move).
    ToolCallRequested {
        id: u64,
        call: ToolCall,
    },
    /// A tool finished (approved, denied, or auto-run), this is what gets
    /// persisted and shown as the card's outcome. The arguments ride so
    /// the UI's status line can label the result ("Ran `cargo test`")
    /// without scraping its transcript rows for the call.
    ToolResult {
        call_id: String,
        name: String,
        arguments: String,
        result: String,
        /// Images the tool produced (#50), as data URLs: what the
        /// model saw beside the text, and what a store keeps.
        images: Vec<String>,
    },
    /// The model's round ended in tool calls, which run next. Comes
    /// before any of them starts. A store records the round's
    /// assistant row here even when the model wrote no text: without
    /// that row the calls' results have nothing to answer on reload,
    /// and one round's results run into the next's.
    ToolsCalled {
        calls: Vec<ToolCall>,
    },
    /// A call started executing without ever prompting (read-only
    /// tools always; mutating calls the mode pre-approved, which is
    /// why the pre-approved path yields this too). Exists so the UI
    /// can show activity for the calls that never prompt, silent
    /// work reads as a hang.
    ToolCallStarted {
        call: ToolCall,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
    },
    /// History was compacted in place: `kept` of `total` messages remain
    /// (a summary plus a safe tail). DB rows are not rewritten, the
    /// compaction is the in-memory view.
    Compacted {
        kept: usize,
        total: usize,
    },
    /// Superseded file reads collapsed to pointers (#39): `reads`
    /// results, `chars` characters no longer re-sent every round. The
    /// in-memory view only, like compaction; DB rows keep the text.
    Pruned {
        reads: usize,
        chars: usize,
    },
    /// The project's check ran (#40). A failure went back to the model;
    /// a pass (or a declined check) ended the turn.
    Checked {
        passed: bool,
    },
    /// A steered user message joined the history at a round boundary.
    /// The receiver persists it like any user turn, the loop only
    /// owns the in-memory history.
    Steered {
        text: String,
    },
    Error {
        message: String,
    },
    /// The provider's rate limit answered every retry: the turn ends
    /// limited. The UI persists the state and offers a manual Resume,
    /// a fresh turn that continues from history without a new user
    /// message.
    Limited {
        message: String,
    },
    /// The round budget crossed: the wrap-up nudge just entered
    /// history in band. Informational, the loop itself continues
    /// through the grace window; a receiver may surface it as a note.
    Nudged,
    /// The round budget's grace window closed: the turn pauses like a
    /// limited one. History ends with the last grace round's tool
    /// results, the nudge sits above them, so a resume turn re-reads
    /// the wrap-up ask with the results in hand.
    Budgeted {
        message: String,
    },
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Approve,
    /// The human refused. `feedback` is what they typed back, a bare
    /// deny is None. The loop folds feedback into the tool result, so
    /// a "no, and here's why" reaches the model and can become a
    /// correction instead of a dead end.
    Deny {
        feedback: Option<String>,
    },
}

/// The conversation history, shared between the loop and the caller so the
/// caller can keep sending turns without reconstructing anything.
pub type SharedHistory = Arc<tokio::sync::Mutex<Vec<WireMessage>>>;

pub type ToolRegistry = Arc<BTreeMap<String, Arc<dyn Tool>>>;

/// Builds the ToolSpec list advertised to the model.
pub fn tool_specs(tools: &ToolRegistry, guidance: crate::tool::ToolGuidance) -> Vec<ToolSpec> {
    tools
        .values()
        .map(|tool| match guidance {
            crate::tool::ToolGuidance::Standard => ToolSpec {
                name: tool.name().to_string(),
                description: tool.description(),
                parameters: tool.parameters(),
            },
            crate::tool::ToolGuidance::Terse => ToolSpec {
                name: tool.name().to_string(),
                description: tool.terse_description(),
                parameters: crate::tool::bare_schema(&tool.parameters()),
            },
        })
        .collect()
}

use std::collections::BTreeMap;

/// Everything one turn needs. The loop grows no more positional
/// parameters: system prompt, permission broker, and whatever M4+
/// adds are fields here.
pub struct TurnSpec {
    pub provider: Arc<dyn crate::provider::Provider>,
    pub tools: ToolRegistry,
    pub history: SharedHistory,
    /// The message opening the turn, None means *resume*: continue
    /// from history as it stands (it already ends with the user's
    /// message, or a tool result from the round that got rate-limited).
    pub user_message: Option<String>,
    pub snapshots: Option<Arc<SnapshotStore>>,
    /// Checkpoint attribution: rows recorded for this turn's mutating
    /// tools carry this session, so the ledger's turn windows can find
    /// them. None leaves rows unattributed, they show in no turn diff.
    pub checkpoint_session: Option<crate::SessionId>,
    /// Where tool-relative paths resolve: the same root the tool
    /// registry was built with. The checkpoint hook needs it to
    /// snapshot the file the tool will actually touch, a relative
    /// argument means workspace-relative, not CWD-relative.
    pub workspace_root: Option<std::path::PathBuf>,
    pub context_window: Option<u64>,
    /// Shared approval registry. When set, every ask, this loop's or a
    /// sub-loop's, registers here so one UI can answer them all. None
    /// means nobody can approve: mutations are denied, doing nothing is
    /// the safe move.
    pub permission_broker: Option<Arc<crate::PermissionBroker>>,
    /// How this turn's mutating-tool asks resolve, supervised, or a
    /// mode that pre-approves a class of asks outright. Fixed for the
    /// turn's life: a flip mid-turn applies to the next turn.
    pub permission_mode: PermissionMode,
    /// Whether bash runs inside the Landlock sandbox. Process-wide
    /// config, mirrored here so the Auto mode never guesses about
    /// enforcement it can't see.
    pub sandbox_bash: bool,
    /// Whose ask this loop's prompts are: the leader's surface as loop
    /// events, a worker's through the broker subscription.
    pub prompt_source: crate::PromptSource,
    /// The turn's routing token on the permission broker: a worker ask
    /// carrying it reaches only the surface that spawned this turn
    /// (the chat that delegated the worker). None, the token-absent
    /// fallback, leaves the ask unscoped: it fans out to every
    /// subscriber, the behavior before scoping existed.
    pub turn_token: Option<crate::TurnToken>,
    /// Sent as the first message of every request, never stored in
    /// history: it is per-turn framing, not conversation record (the
    /// DB stays canonical; the next turn may have a different index).
    pub system_prompt: Option<String>,
    /// Session instructions loaded from AGENTS.md files: sent as one
    /// user-role message right after the system prompt, before
    /// history, on every request. Framing like the prompt, never
    /// record: history can be compacted away, the instructions must
    /// not be lost with it.
    pub instructions: Option<String>,
    /// The memory index, snapshotted at session start and frozen for
    /// the session: sent as one user-role message after the
    /// instructions, before history, on every request. Framing, never
    /// record. Frozen because cache prefixes build tools -> system ->
    /// messages: a mutable index riding every request would bust the
    /// cache for the whole session (research/MEMORY-RESEARCH.md,
    /// finding 5). Mutations land next session's snapshot.
    pub memory_index: Option<String>,
    /// The session's repo map (#41): rendered from the session's hints
    /// on its first turn, then frozen, and sent after the memory index
    /// on every request. None = no map (off, workers, tests).
    pub repo_map: Option<crate::repo_map::SessionMap>,
    /// The memory store, for recall on touch (#36): a call that touches
    /// a file some live entry cites carries those entries in its
    /// result. None (workers, memory off) recalls nothing.
    pub memory: Option<std::sync::Arc<crate::MemoryStore>>,
    /// Mid-run steering: user messages typed while the turn is already
    /// running. Drained at round boundaries, each becomes a user push
    /// plus a `LoopEvent::Steered`, and a steer landing on what would
    /// be the final round buys another one, so it is always answered.
    /// None = the turn cannot be steered.
    pub steer: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
    /// Attachments riding with `user_message`, as data URLs. Steered
    /// messages are text-only; the first message of the turn carries
    /// the attachments.
    pub user_attachments: Vec<String>,
    /// The turn's round budgets: tool rounds before the wrap-up nudge,
    /// grace rounds after it before the pause. None = the built-in
    /// defaults (`DEFAULT_MAX_TOOL_ROUNDS`,
    /// `DEFAULT_GRACE_ROUNDS`). Long-running
    /// sessions (a big migration, a wide sweep) raise them; the safety
    /// valve's purpose, stopping runaway loops, is the constant's.
    pub max_tool_rounds: Option<usize>,
    pub grace_rounds: Option<usize>,
    /// What spawned workers are doing right now, written by the agent
    /// tools' worker loops and read by the leader's UI on its tick.
    /// The same handle flows in and back out: the UI holds one end,
    /// this loop hands the other to the tools that run their own
    /// loops (via [`Tool::run_with_call`]).
    pub worker_progress: crate::tool::WorkerProgress,
    /// The project's check (#40): run as a bash call, under the turn's
    /// permission mode, when the model ends a turn that edited files.
    /// None = no check.
    pub verify: Option<String>,
    /// How much the tool definitions coach this model (#42).
    pub tool_guidance: crate::tool::ToolGuidance,
}

impl TurnSpec {
    /// A spec with every optional part off: no user message, no
    /// snapshots, no framing, supervised, leader-sourced, the built-in
    /// round budgets. Callers set what their turn has with struct
    /// update syntax, so a new field is a change here, not at every
    /// construction site.
    pub fn new(
        provider: Arc<dyn crate::provider::Provider>,
        tools: ToolRegistry,
        history: SharedHistory,
    ) -> Self {
        Self {
            provider,
            tools,
            history,
            user_message: None,
            snapshots: None,
            checkpoint_session: None,
            workspace_root: None,
            context_window: None,
            permission_broker: None,
            permission_mode: PermissionMode::Supervised,
            sandbox_bash: false,
            prompt_source: crate::PromptSource::Leader,
            turn_token: None,
            system_prompt: None,
            instructions: None,
            memory_index: None,
            repo_map: None,
            memory: None,
            steer: None,
            user_attachments: Vec::new(),
            max_tool_rounds: None,
            grace_rounds: None,
            worker_progress: crate::tool::WorkerProgress::default(),
            verify: None,
            tool_guidance: crate::tool::ToolGuidance::Standard,
        }
    }
}

/// Runs one user turn to completion: request → (text | tool calls)* →
/// tool execution → repeat, until the model answers without tools, the
/// round budget is spent, or an error surfaces.
///
/// History is shared: the caller appends nothing mid-turn; the loop owns
/// every push (user message, assistant turns, tool results) so the next
/// turn starts from an exact canonical record.
///
/// When `snapshots` is set, every mutating tool whose arguments name a
/// `path` gets a checkpoint of the file's prior content first,
/// record-before-write, uniformly, including tools added later.
///
/// When `context_window` is set, the loop compacts: once a round's
/// observed prompt size crosses 80% of the window, history is replaced
/// by a budgeted selection (verbatim prefixes, no summarizer model)
/// plus a safe tail before the next request.
pub fn run_turn(spec: TurnSpec) -> BoxStream<'static, LoopEvent> {
    let TurnSpec {
        provider,
        tools,
        history,
        user_message,
        snapshots,
        checkpoint_session,
        workspace_root,
        context_window,
        permission_broker,
        permission_mode,
        sandbox_bash,
        prompt_source,
        turn_token,
        system_prompt,
        instructions,
        memory_index,
        repo_map,
        memory,
        steer,
        user_attachments,
        max_tool_rounds,
        grace_rounds,
        worker_progress,
        verify,
        tool_guidance,
    } = spec;
    let max_tool_rounds = max_tool_rounds.unwrap_or(DEFAULT_MAX_TOOL_ROUNDS);
    let grace_rounds = grace_rounds.unwrap_or(DEFAULT_GRACE_ROUNDS);
    let mut steer = steer;
    let specs = tool_specs(&tools, tool_guidance);
    Box::pin(async_stream::stream! {
        // A resume turn pushes nothing: history already ends with what
        // the model should continue from.
        if let Some(text) = user_message {
            history
                .lock()
                .await
                .push(WireMessage::user_with_attachments(text, user_attachments));
        }

        // Observed prompt size from the last request's usage events, if
        // the provider reported any; the chars/4 estimate covers the rest.
        let mut last_input_tokens: Option<u64> = None;
        // The repo map (#41): the session's first turn renders it from
        // what the session holds so far (the user's words, files already
        // touched, files live memory cites); later turns reuse the
        // frozen text.
        let repo_map_text = match &repo_map {
            Some(map) => {
                let cited: Vec<String> = memory
                    .as_ref()
                    .and_then(|store| {
                        let today = store.today().ok()?;
                        store.valid_entries(&today).ok()
                    })
                    .unwrap_or_default()
                    .into_iter()
                    .flat_map(|entry| entry.evidence.into_iter().map(|evidence| evidence.path))
                    .collect();
                let hints = crate::repo_map::hints_from(&history.lock().await, cited);
                map.text(hints).await
            }
            None => None,
        };
        // The check (#40): owed once an edit lands, run when the model
        // tries to end the turn, at most VERIFY_RUNS times a turn.
        let mut edited = false;
        let mut checks_run = 0usize;
        let mut checking: Option<String> = None;

        for round in 1..=(max_tool_rounds + grace_rounds) {
            // Compaction check happens at round start, before the
            // request is built, the point is to shrink what we're
            // about to send.
            {
                let prompt_tokens = match last_input_tokens {
                    Some(observed) => observed,
                    None => estimate_tokens(&history.lock().await),
                };
                let threshold = context_window.map(|window| window * 80 / 100);
                if let Some(threshold) = threshold
                    && prompt_tokens > threshold
                {
                    // Selection, not summarization: no model call, so
                    // there is no provider to fail, the only way this
                    // "fails" is nothing to select, which changes
                    // nothing.
                    if let Some((kept, total)) =
                        compact_history(history.clone(), compaction_budget(context_window)).await
                    {
                        // The old observation describes the pre-compact
                        // prompt; don't let it re-trigger.
                        last_input_tokens = None;
                        yield LoopEvent::Compacted { kept, total };
                    }
                }
            }
            // Superseded reads (#39): batched, so the cached prefix
            // breaks once per batch, not once per re-read.
            let pruned = crate::prune::prune_superseded(&mut history.lock().await);
            if let Some((reads, chars)) = pruned {
                yield LoopEvent::Pruned { reads, chars };
            }
            // Steering: messages the user typed mid-run join the
            // conversation here, the model reads them with the next
            // request, after the tool results it was working on.
            if let Some(steer) = steer.as_mut() {
                while let Ok(text) = steer.try_recv() {
                    history.lock().await.push(WireMessage::user(&text));
                    yield LoopEvent::Steered { text };
                }
            }
            // The request is rebuilt per round, history grows, and
            // the wire must see all of it. Owned into the provider:
            // the old draft moved the request in and cloned it back
            // out every round, a full deep copy of every message twice
            // over. A fresh shell costs only the tool specs' clone,
            // descriptions are framing, not payload.
            let mut request = Request {
                tools: specs.clone(),
                ..Request::default()
            };
            request.messages = history
                .lock()
                .await
                .iter()
                .map(WireMessage::clone)
                .collect::<Vec<_>>();
            // System prompt is framing, not record: prepended to the
            // request, absent from history and the DB.
            if let Some(prompt) = &system_prompt {
                request
                    .messages
                    .insert(0, WireMessage { role: Role::System, content: Some(prompt.clone()), ..WireMessage::default() });
            }
            // Session instructions are framing too: one user-role
            // message after the prompt, before history, on every
            // round. A user role because the model should treat the
            // text as binding direction, not as its own context
            // block; it is never persisted, so it cannot masquerade
            // as conversation.
            if let Some(instructions) = &instructions {
                let at = usize::from(system_prompt.is_some());
                request.messages.insert(
                    at,
                    WireMessage {
                        role: Role::User,
                        content: Some(format!(
                            "[session instructions]\n{instructions}\n[end session instructions]"
                        )),
                        ..WireMessage::default()
                    },
                );
            }
            // The memory index rides last in the framing block, after
            // instructions, before history. The order is the cache
            // story: the static prompt (byte-stable), then per-session
            // framing that never changes mid-session. Memory itself is
            // advisory recall; the framing text says so, because an
            // index that reads like policy gets obeyed like policy.
            if let Some(index) = &memory_index {
                let at = usize::from(system_prompt.is_some())
                    + usize::from(instructions.is_some());
                request.messages.insert(
                    at,
                    WireMessage {
                        role: Role::User,
                        content: Some(format!(
                            "[memory index -- frozen for this session. Entries from earlier \
                             sessions: if what you are about to do contradicts one, name it and \
                             ask the user before acting, unless their message already addressed \
                             it. Entries citing a file arrive when you touch it; search_memory \
                             finds the rest; save or correct a fact with remember (supersede a \
                             changed fact, never rewrite it)]\n{index}\n\
                             [end memory index]"
                        )),
                        ..WireMessage::default()
                    },
                );
            }

            // The repo map closes the framing block: per-session and
            // frozen like the index, so the cached prefix holds.
            if let Some(body) = &repo_map_text {
                let at = usize::from(system_prompt.is_some())
                    + usize::from(instructions.is_some())
                    + usize::from(memory_index.is_some());
                request.messages.insert(
                    at,
                    WireMessage {
                        role: Role::User,
                        content: Some(crate::repo_map::framing(body)),
                        ..WireMessage::default()
                    },
                );
            }

            let mut stream = provider.stream(request);
            let mut assistant_text = String::new();
            let mut calls: Vec<ToolCall> = Vec::new();

            while let Some(event) = stream.next().await {
                match event {
                    crate::Event::Started => {}
                    crate::Event::Token { delta } => {
                        assistant_text.push_str(&delta);
                        yield LoopEvent::Delta { delta };
                    }
                    crate::Event::ToolCall { id, name, arguments } => {
                        calls.push(ToolCall { id, name, arguments });
                    }
                    crate::Event::Usage {
                        input_tokens,
                        output_tokens,
                        cached_input_tokens,
                    } => {
                        last_input_tokens = Some(input_tokens);
                        yield LoopEvent::Usage {
                            input_tokens,
                            output_tokens,
                            cached_input_tokens,
                        };
                    }
                    crate::Event::Done => {}
                    crate::Event::Error { message } => {
                        yield LoopEvent::Error { message };
                        return;
                    }
                    crate::Event::Limited { message } => {
                        yield LoopEvent::Limited { message };
                        return;
                    }
                }
            }

            // The model wants to end a turn that edited files: the
            // project's check runs first, as a bash call riding the
            // model's final message, through the same permission path
            // as any call the model makes (the mode decides; the card,
            // the sandbox and the shaping all apply).
            if calls.is_empty()
                && edited
                && checks_run < VERIFY_RUNS
                && round < max_tool_rounds
                && let Some(command) = verify.as_deref()
            {
                let id = format!("gertie-check-{round}");
                calls.push(ToolCall {
                    id: id.clone(),
                    name: "bash".into(),
                    arguments: serde_json::json!({
                        "command": command,
                        "timeout": VERIFY_TIMEOUT_SECS,
                    })
                    .to_string(),
                });
                checking = Some(id);
                checks_run += 1;
                edited = false;
            }

            if calls.is_empty() {
                history
                    .lock()
                    .await
                    .push(WireMessage::assistant(assistant_text));
                // A steer landing on what would be the final round buys
                // another one: the message joins the history and the
                // loop turns back to answer it. The round budget still
                // bounds this, steering is not an infinite ladder.
                let mut steered = Vec::new();
                if let Some(steer) = steer.as_mut() {
                    while let Ok(text) = steer.try_recv() {
                        steered.push(text);
                    }
                }
                if !steered.is_empty() {
                    for text in steered {
                        history.lock().await.push(WireMessage::user(&text));
                        yield LoopEvent::Steered { text };
                    }
                    continue;
                }
                yield LoopEvent::Done;
                return;
            }

            let mut check_outcome: Option<CheckVerdict> = None;
            history
                .lock()
                .await
                .push(WireMessage::assistant_with_tool_calls(
                    if assistant_text.is_empty() { None } else { Some(assistant_text) },
                    calls.clone(),
                ));
            yield LoopEvent::ToolsCalled { calls: calls.clone() };

            for call in calls {
                // Images a tool produced (#50): only a tool that ran
                // fills this, a refusal or a denial carries none.
                let mut images: Vec<String> = Vec::new();
                let result = match tools.get(&call.name) {
                    None => Err(format!("unknown tool: {}", call.name)),
                    Some(_) if call.name == "ask_user" => {
                        // The structured question: the model asks the
                        // human a concrete question with clickable
                        // options, the answer rides back as the tool
                        // result. It always surfaces as a card, in
                        // every mode: in full access there is no other
                        // moment the human sees the ask at all. The
                        // question and options are the model's words,
                        // so the parse sanitizes them before anything
                        // renders. Same contract as a mutating ask:
                        // the event renders the card, the await blocks
                        // the round, a dropped responder counts as no
                        // answer.
                        match parse_ask(&call.arguments, &call.id) {
                            Err(error) => Err(error),
                            Ok(ask) => match &permission_broker {
                                None => Err("no permission channel available, cannot ask".into()),
                                Some(broker) => {
                                    let (request_id, answer) =
                                        broker.request(ask.call.clone(), prompt_source, turn_token);
                                    yield LoopEvent::ToolCallRequested {
                                        id: request_id,
                                        call: ask.call.clone(),
                                    };
                                    Ok(match answer.await {
                                        Err(_) => "the ask went unanswered".to_string(),
                                        // An ask reuses the permission reply: a
                                        // choice or typed answer comes back as Deny
                                        // with the text as feedback, and the card's
                                        // Dismiss button sends a bare Approve. So
                                        // here Approve means "closed unanswered",
                                        // never consent: keep this wording refusing,
                                        // whatever the variant's name suggests (the
                                        // Dismiss button in transcript.rs pins the
                                        // other end).
                                        Ok(PermissionDecision::Approve) => {
                                            "the user dismissed the question without \
                                             answering: no choice was made and nothing is \
                                             approved. Do not take the action you asked \
                                             about; stop and say what is waiting on them"
                                                .to_string()
                                        }
                                        Ok(PermissionDecision::Deny { feedback }) => match
                                            feedback
                                        {
                                            Some(text) if !text.trim().is_empty() => text,
                                            _ => "the user did not answer".to_string(),
                                        },
                                    })
                                }
                            },
                        }
                    }
                    Some(tool) => {
                        // Hard policy first: a vet failure never reaches
                        // the prompt, there is nothing to approve.
                        if let Err(blocked) = tool.vet(&call.arguments) {
                            Err(format!("blocked by policy: {blocked}"))
                        }
                        // Mutating tools: the turn's mode pre-approves
                        // a class of asks outright, an edit the
                        // checkpoint will snapshot is reversible, a
                        // sandboxed command is contained, and the
                        // rest face the permission prompt: the ask
                        // registers with the broker (so the UI can
                        // answer it) and the loop awaits the answer.
                        // The ask gate rides along: a tool can owe a
                        // card without mutating anything (web_fetch),
                        // its effect is not a file the checkpoint
                        // could restore.
                        else if tool.mutates() || tool.asks() {
                            let checkpoint_target = checkpoint_path(
                                &call.arguments,
                                workspace_root.as_deref(),
                            );
                            // Containment is a fact about this
                            // call, not the turn: the sandbox
                            // contains bash, and nothing else. A
                            // turn-wide flag here would wave a
                            // gated fetch or an MCP tool through
                            // every sandboxed auto turn. A tool
                            // may vouch for one call of its own
                            // (a screenshot of a workspace page).
                            let contained = (sandbox_bash && call.name == "bash")
                                || (!tool.third_party() && tool.contained(&call.arguments));
                            let pre_approved = permission_mode.auto_allows(
                                checkpoint_target.is_some(),
                                contained,
                                tool.third_party(),
                            );
                            let decision = match (&permission_broker, pre_approved) {
                                // The mode settled it before any ask.
                                // The run still surfaces as a started
                                // call: a pre-approved mutation is the
                                // model doing something the human can
                                // watch, a running card with its own
                                // clock, not an invisible act whose
                                // result appears from nowhere.
                                (_, true) => {
                                    yield LoopEvent::ToolCallStarted { call: call.clone() };
                                    Ok(PermissionDecision::Approve)
                                }
                                (Some(broker), false) => {
                                    let (request_id, answer) =
                                        broker.request(call.clone(), prompt_source, turn_token);
                                    yield LoopEvent::ToolCallRequested {
                                        id: request_id,
                                        call: call.clone(),
                                    };
                                    // A dropped responder (UI went
                                    // away) denies too: doing nothing
                                    // is safe.
                                    answer
                                        .await
                                        .map_err(|_| "denied by user".to_string())
                                }
                                (None, false) => {
                                    Err("no permission channel available, denied".to_string())
                                }
                            };
                            // The pre-approved path and the human's
                            // approval converge here on purpose: the
                            // checkpoint is what makes an auto-approval
                            // honest, so pre-approval must not be a way
                            // around the ledger.
                            match decision {
                                Ok(PermissionDecision::Approve) => {
                                    // Checkpoint the file's prior state
                                    // before anything mutates it.
                                    if let (Some(store), Some(path)) =
                                        (&snapshots, checkpoint_target)
                                        && let Err(error) = store.record_file(
                                            checkpoint_session,
                                            &path,
                                            // The turn's own root rides the
                                            // row: the restore's address,
                                            // however far the chat moves.
                                            workspace_root.as_deref(),
                                            // The call's own id rides too:
                                            // a reloaded card's diff asks
                                            // the ledger by it.
                                            Some(&call.id),
                                        )
                                    {
                                        yield LoopEvent::Error {
                                            message: format!(
                                                "checkpoint before {} failed: {error}",
                                                call.name
                                            ),
                                        };
                                        return;
                                    }
                                    let (ran, produced) = tool
                                        .run_with_images(&call, &worker_progress, turn_token)
                                        .await;
                                    images = produced;
                                    ran
                                }
                                // The human said no, and possibly
                                // why. Their reply rides in the tool
                                // result, so the next round can act
                                // on it instead of repeating itself.
                                Ok(PermissionDecision::Deny { feedback }) => Err(
                                    match feedback {
                                        Some(text) if !text.trim().is_empty() => {
                                            format!("denied by user, their reply: {text}")
                                        }
                                        _ => "denied by user".to_string(),
                                    },
                                ),
                                Err(denied) => Err(denied),
                            }
                        } else {
                            yield LoopEvent::ToolCallStarted { call: call.clone() };
                            let (ran, produced) = tool
                                .run_with_images(&call, &worker_progress, turn_token)
                                .await;
                            images = produced;
                            ran
                        }
                    }
                };
                let mut result = match result {
                    Ok(text) => crate::strip_invisible(&text),
                    Err(text) => format!("error: {}", crate::strip_invisible(&text)),
                };
                // Directory instructions: the first call that reaches
                // under a nested AGENTS.md carries it in its result
                // (directory_instructions.rs has the why). Any tool
                // whose arguments name a path counts, read or write,
                // run or refused: the rules apply to what comes next.
                if let Some(root) = workspace_root.as_deref()
                    && let Some(target) = checkpoint_path(&call.arguments, Some(root))
                {
                    let blocks = crate::directory_instructions::blocks_for(
                        root,
                        &target,
                        &history.lock().await,
                    );
                    if let Some(blocks) = blocks {
                        result.push_str(&blocks);
                    }
                    // Recall on touch (#36): the entries citing this
                    // file, once per session, on a call that worked.
                    if let Some(memory) = memory.as_deref()
                        && !result.starts_with("error:")
                        && let Some(block) = crate::memory_recall::block_for(
                            memory,
                            root,
                            &target,
                            &history.lock().await,
                        )
                    {
                        result.push_str(&block);
                    }
                }

                if EDIT_TOOLS.contains(&call.name.as_str()) && !result.starts_with("error:") {
                    edited = true;
                }
                if checking.as_deref() == Some(call.id.as_str()) {
                    let verdict = check_verdict(&result);
                    result = decorate_check(&result, verdict, checks_run >= VERIFY_RUNS);
                    check_outcome = Some(verdict);
                }

                // The strip above is the prompt boundary: tool output,
                // a web page, an MCP payload, anything the world sent
                // back, enters the model's context and the transcript
                // here, and enters it without bidirectional controls,
                // zero-width marks, or tag characters that could make
                // either reader see something other than what is there.
                history
                    .lock()
                    .await
                    .push(WireMessage::tool_result_with_images(&call, &result, images.clone()));
                yield LoopEvent::ToolResult {
                    call_id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                    result,
                    images,
                };
            }

            // A passing check ends the turn as the model meant to; a
            // declined one too (the human said not now). Only a failure
            // goes back to the model. A steer still buys a round.
            if let Some(verdict) = check_outcome.take() {
                checking = None;
                yield LoopEvent::Checked { passed: verdict == CheckVerdict::Passed };
                if verdict != CheckVerdict::Failed {
                    let mut steered = Vec::new();
                    if let Some(steer) = steer.as_mut() {
                        while let Ok(text) = steer.try_recv() {
                            steered.push(text);
                        }
                    }
                    if steered.is_empty() {
                        yield LoopEvent::Done;
                        return;
                    }
                    for text in steered {
                        history.lock().await.push(WireMessage::user(&text));
                        yield LoopEvent::Steered { text };
                    }
                    continue;
                }
            }

            if round >= max_tool_rounds {
                // A steer never dies at the budget boundary: drained
                // here first, each one buys the next round (same rule
                // as the plain-done path above, so the budget bounds
                // the model, not the human's interruption).
                let mut steered = Vec::new();
                if let Some(steer) = steer.as_mut() {
                    while let Ok(text) = steer.try_recv() {
                        steered.push(text);
                    }
                }
                if !steered.is_empty() {
                    for text in steered {
                        history.lock().await.push(WireMessage::user(&text));
                        yield LoopEvent::Steered { text };
                    }
                    continue;
                }
                if round == max_tool_rounds {
                    // First over-budget round: tell the model, in band,
                    // to land the turn. History ends with the tool
                    // results plus this nudge, so a resume turn
                    // continues from the wrap-up ask.
                    history.lock().await.push(WireMessage::user(
                        "You have reached the tool-round budget for this turn. \
                         Stop starting new work: finish or safely set down what is \
                         in flight, and give your final answer now.",
                    ));
                    yield LoopEvent::Nudged;
                    continue;
                }
                if round >= max_tool_rounds + grace_rounds {
                    // The model ignored the wrap-up ask through the
                    // whole grace window: stop spending. Pause like a
                    // rate limit, history intact, the human resumes
                    // when ready.
                    yield LoopEvent::Budgeted {
                        message: format!(
                            "paused after {max_tool_rounds} tool rounds plus \
                             {grace_rounds} grace rounds without wrapping up"
                        ),
                    };
                    return;
                }
            }
        }
    })
}

/// Checks a turn may run: the first, then two after failures.
pub const VERIFY_RUNS: usize = 3;

/// A check's time limit: builds are slower than the bash default.
const VERIFY_TIMEOUT_SECS: u64 = 300;

/// Tools whose success means the check is owed.
const EDIT_TOOLS: [&str; 2] = ["edit_file", "write_file"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckVerdict {
    Passed,
    Failed,
    /// Denied by the human, or blocked by policy: not the model's to fix.
    Declined,
}

/// Reads a bash result: `exit N:` and timeouts fail, denials decline.
fn check_verdict(result: &str) -> CheckVerdict {
    if result.starts_with("error: denied") || result.starts_with("error: blocked") {
        CheckVerdict::Declined
    } else if result.starts_with("exit ") || result.starts_with("error:") {
        CheckVerdict::Failed
    } else {
        CheckVerdict::Passed
    }
}

/// What the model reads: who ran the check and why, and on a failure,
/// that the turn is not done.
fn decorate_check(result: &str, verdict: CheckVerdict, last: bool) -> String {
    let head = "[gertie ran the project check before ending the turn]";
    match verdict {
        CheckVerdict::Passed => format!("{head}\n{result}\n[check passed]"),
        CheckVerdict::Declined => format!("{head}\n{result}"),
        CheckVerdict::Failed if last => format!(
            "{head}\n{result}\n\nThe check failed and will not run again this turn: tell the user \
             what still fails and why."
        ),
        CheckVerdict::Failed => format!(
            "{head}\n{result}\n\nThe check failed, so the turn is not done: fix what it reports, \
             or say why the failure is expected (for example, the check cannot run here)."
        ),
    }
}

/// Messages kept verbatim after a compaction, roughly two tool rounds.
/// How many trailing messages survive a compaction. The UI's manual
/// `/compact` reads this as its minimum-size gate.
pub const KEEP_TAIL: usize = 8;

/// An item of the compaction digest keeps a 240-char prefix, enough
/// to know what happened, not enough to fill a window. (t3code's
/// handoff rule: a budgeted selection, not an agent-written summary.)
const ITEM_PREFIX: usize = 240;

/// The original request keeps a bigger allowance than the digest's
/// other lines, it anchors everything that follows.
const REQUEST_PREFIX: usize = 2_000;

/// Tokens held back for instructions, tools, and the work that follows
/// a compaction: at least 16k, never more than a quarter of the window
/// (the same reserve shape t3code uses).
fn reserve_tokens(window: u64) -> u64 {
    (window / 4).max(16_000)
}

/// Selection never shrinks below this, even on tiny windows, a digest
/// that keeps nothing is worse than one that overshoots.
const MIN_SELECTION_TOKENS: u64 = 4_000;

/// When no context window is configured (compaction never auto-fires),
/// `/compact` budgets against this assumption, t3code's stand-in for
/// unknown capacity.
const ASSUMED_WINDOW_TOKENS: u64 = 128_000;

/// The compaction ceiling when the config names none: what a 200k
/// window gets at 80%. A model advertising a 1M window used to grow a
/// session to 400k tokens before its first compaction, every request
/// re-sending all of it: slow to first token and costly per turn.
pub const DEFAULT_COMPACT_AT_TOKENS: u64 = 160_000;

/// The window compaction works against: the model's own, capped so
/// the 80% trigger lands at `compact_at` tokens (None = the default
/// ceiling, Some(0) = no ceiling). With no model window the ceiling
/// alone sets it, so compaction runs instead of never.
pub fn compaction_window(window: Option<u64>, compact_at: Option<u64>) -> Option<u64> {
    let ceiling = match compact_at {
        Some(0) => None,
        Some(tokens) => Some(tokens),
        None => Some(DEFAULT_COMPACT_AT_TOKENS),
    }
    .map(|tokens| tokens * 100 / 80);
    match (window, ceiling) {
        (Some(window), Some(ceiling)) => Some(window.min(ceiling)),
        (window, ceiling) => window.or(ceiling),
    }
}

/// The selection budget a compaction uses: sized against half the
/// window, so the compacted history lands well under the trigger. A
/// selection sized against the whole window (75% of it, against an 80%
/// trigger) could refill to the trigger and compact again rounds later.
pub fn compaction_budget(window: Option<u64>) -> usize {
    selection_budget(window.map(|window| window / 2))
}

/// The character budget for a compaction's selection: the window minus
/// the reserve, in chars at the bytes/4 estimate, floored so a tiny
/// window still keeps *something*.
pub fn selection_budget(context_window: Option<u64>) -> usize {
    let window = context_window.unwrap_or(ASSUMED_WINDOW_TOKENS);
    let budget_tokens = window
        .saturating_sub(reserve_tokens(window))
        .max(MIN_SELECTION_TOKENS);
    (budget_tokens * 4) as usize
}

/// Compacts the history in place: everything before the kept tail
/// becomes a budgeted SELECTION, not a summary, the original request,
/// then verbatim prefixes of the omitted turns, newest first, inside
/// one system message. No model is called; every line cites the record
/// verbatim. Returns `(kept, total)`, None when the kept tail already
/// covers the whole history (a compaction that shrinks nothing is not
/// one), leaving the history untouched. Fail-open by construction:
/// there is no provider call to fail.
pub async fn compact_history(
    history: SharedHistory,
    budget_chars: usize,
) -> Option<(usize, usize)> {
    let mut current = history.lock().await;
    compact_in_place(&mut current, budget_chars)
}

/// The synchronous core of `compact_history`, the lock is the only
/// thing that awaits, so the logic is directly unit-testable.
fn compact_in_place(current: &mut Vec<WireMessage>, budget_chars: usize) -> Option<(usize, usize)> {
    let total = current.len();
    let tail_start = safe_tail_start(current, KEEP_TAIL);
    if tail_start == 0 {
        return None;
    }
    let digest = select_digest(&current[..tail_start], budget_chars);
    let tail: Vec<WireMessage> = current.drain(tail_start..).collect();
    *current = vec![selection_message(&digest)];
    current.extend(tail);
    Some((current.len(), total))
}

/// chars/4: the boring token estimate, used only when the provider
/// hasn't reported usage yet.
pub fn estimate_tokens(history: &[WireMessage]) -> u64 {
    let chars: usize = history
        .iter()
        .map(|message| {
            message.content.as_deref().unwrap_or("").len()
                + message
                    .tool_calls
                    .iter()
                    .map(|call| call.arguments.len())
                    .sum::<usize>()
        })
        .sum();
    (chars as u64) / 4
}

/// Flattens the pre-tail history into the digest's lines: the original
/// request on its own line, then one line per omitted item, verbatim
/// prefix, role cited, newest first until the character budget is
/// spent, restored to chronological order. This is a table of contents
/// of the conversation's own past, not a retelling of it.
fn select_digest(head: &[WireMessage], budget_chars: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    // The original request anchors the selection; it survives every
    // compaction (bounded by its own larger prefix).
    if let Some(first) = head
        .iter()
        .find(|message| message.role == Role::User)
        .and_then(|message| message.content.as_deref())
    {
        let text = first.trim();
        if !text.is_empty() {
            lines.push(format!(
                "[user, the original request] {}",
                prefix(text, REQUEST_PREFIX)
            ));
        }
    }
    // The rest of the digest: walk newest → oldest (the turns nearest
    // the kept tail matter most), stop when the budget is spent, then
    // put the survivors back in the order they happened. The anchor
    // message is skipped, it already has its line.
    let first_user = head.iter().position(|message| message.role == Role::User);
    let mut spent: usize = lines.iter().map(String::len).sum();
    let mut items: Vec<String> = Vec::new();
    'outer: for (position, message) in head.iter().enumerate().rev() {
        if Some(position) == first_user {
            continue;
        }
        if let Some(content) = message
            .content
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            let line = format!(
                "[{}] {}",
                role_name(&message.role),
                prefix(content, ITEM_PREFIX)
            );
            if spent + line.len() > budget_chars {
                break;
            }
            spent += line.len();
            items.push(line);
        }
        for call in &message.tool_calls {
            let line = format!(
                "[assistant called {}] {}",
                call.name,
                prefix(call.arguments.trim(), ITEM_PREFIX)
            );
            if spent + line.len() > budget_chars {
                break 'outer;
            }
            spent += line.len();
            items.push(line);
        }
    }
    items.reverse();
    lines.extend(items);
    lines.join("\n")
}

/// The structured question, parsed and sanitized: the flat schema
/// (question, two to four options, optional free-text allowance)
/// becomes a synthetic `ask_user` call whose arguments carry the
/// cleaned shape for the card to render. Every string is the model's
/// output, so nothing reaches the UI unsanitized.
fn parse_ask(arguments: &str, call_id: &str) -> Result<AskUserQuestion, String> {
    #[derive(serde::Deserialize)]
    struct AskArgs {
        question: String,
        #[serde(default)]
        options: Vec<String>,
        #[serde(default)]
        allow_custom: bool,
    }
    let args: AskArgs = serde_json::from_str(arguments)
        .map_err(|error| format!("ask_user: bad arguments ({error})"))?;
    let question = crate::sanitize_untrusted(&args.question, 512);
    if question.trim().is_empty() {
        return Err("ask_user: the question was empty".into());
    }
    if args.options.len() > 4 {
        return Err(format!(
            "ask_user: {} options is too many; ask with four or fewer",
            args.options.len()
        ));
    }
    let options: Vec<String> = args
        .options
        .iter()
        .map(|option| crate::sanitize_untrusted(option, 256))
        .collect();
    if options.len() < 2 {
        return Err(
            "ask_user: needs at least two options; for open questions, just ask in prose".into(),
        );
    }
    Ok(AskUserQuestion {
        call: ToolCall {
            // The card's call keeps the model's own id: the UI pairs
            // cards with their results by it, a synthetic id here
            // would leave the ask card pending forever.
            id: call_id.to_string(),
            name: "ask_user".into(),
            arguments: serde_json::json!({
                "question": question,
                "options": options,
                "allow_custom": args.allow_custom,
            })
            .to_string(),
        },
    })
}

struct AskUserQuestion {
    call: ToolCall,
}

fn role_name(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Char-boundary-safe prefix: a byte slice would tear a multi-byte
/// character; this takes whole characters or all of them.
fn prefix(text: &str, max: usize) -> &str {
    text.char_indices()
        .nth(max)
        .map_or(text, |(end, _)| &text[..end])
}

/// The digest rides in the same system-message shape the old summary
/// used, legal on every wire, and later compactions treat it like
/// any other message.
fn selection_message(digest: &str) -> WireMessage {
    WireMessage {
        role: Role::System,
        content: Some(format!(
            "### Earlier history, selected from the record, not summarized\n\n\
             {digest}\n\n\
             Context was compacted to fit the window. The lines above are\n\
             verbatim prefixes, not a retelling; the full text of every\n\
             omitted message is still in the session record, the\n\
             read_session tool retrieves it."
        )),
        ..WireMessage::default()
    }
}

/// Where a verbatim tail of `keep` messages may start without tearing a
/// tool round: tool results must follow the assistant call that produced
/// them. Orphan results (their call would be summarized away) are skipped
/// forward, a slice that opens on a tool result is not a legal request.
fn safe_tail_start(history: &[WireMessage], keep: usize) -> usize {
    let mut start = history.len().saturating_sub(keep);
    while start < history.len() && history[start].role == Role::Tool {
        start += 1;
    }
    start
}

/// Extracts the file a mutating call targets, if its arguments name one.
/// This is the checkpoint hook's convention: `path` (or `file_path`) as a
/// string means "snapshot this before running".
fn argument_path(arguments: &str) -> Option<std::path::PathBuf> {
    let value: serde_json::Value = serde_json::from_str(arguments).ok()?;
    value
        .get("path")
        .or_else(|| value.get("file_path"))
        .and_then(|path| path.as_str())
        .map(std::path::PathBuf::from)
}

/// The path the checkpoint hook snapshots: the tool's argument, with a
/// relative name resolved against the workspace root, the same
/// resolution every path-naming tool applies. The result is always
/// *contained*: lexical `..` walks plus a canonicalize check on the
/// nearest existing ancestor, the same rules the file tools' resolve()
/// applies. A path that escapes (or an absolute one outside the
/// workspace) is None, the ask is not pre-approvable, because the
/// checkpoint's reversibility argument does not reach past the
/// workspace. Absolute paths inside the workspace are contained and
/// snapshot-able; absolute paths outside are None.
fn checkpoint_path(arguments: &str, root: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    let path = argument_path(arguments)?;
    let root = root?;
    if path.is_absolute() {
        return contained(root, &path);
    }
    let joined = root.join(path);
    contained(root, &joined)
}

/// Containment for a checkpoint target: the nearest existing ancestor
/// must canonicalize back inside the workspace root, a symlink inside
/// the workspace pointing out of it turns a pre-approved write into
/// filesystem access the permission gate never saw. An unresolvable
/// path (broken chain, permission error) is None: fail closed.
fn contained(root: &std::path::Path, path: &std::path::Path) -> Option<std::path::PathBuf> {
    // Lexical check first: whatever remains after the root's prefix may
    // only use `..` to cancel normals it first added.
    let suffix = path.strip_prefix(root).ok()?;
    let mut offset: isize = 0;
    for component in suffix.components() {
        match component {
            std::path::Component::ParentDir => offset -= 1,
            std::path::Component::Normal(_) => offset += 1,
            std::path::Component::CurDir => {}
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return None,
        }
        if offset < 0 {
            return None;
        }
    }
    let canonical_root = root.canonicalize().ok()?;
    // Walk up to the nearest ancestor that exists and canonicalize it;
    // the nonexistent tail is joined back on.
    let mut ancestor = path.to_path_buf();
    let mut tail = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(resolved) => {
                if !resolved.starts_with(&canonical_root) {
                    return None;
                }
                let mut contained = resolved;
                for part in tail.iter().rev() {
                    contained = contained.join(part);
                }
                return Some(contained);
            }
            Err(_) => {
                let last = ancestor.file_name().map(std::path::PathBuf::from)?;
                ancestor = ancestor.parent()?.to_path_buf();
                tail.push(last);
            }
        }
    }
}

/// Answers every tool call the history left unanswered, after a turn
/// was stopped mid-round: dropping the turn's future (the cancel)
/// can leave the last assistant message's calls without results, and
/// a request whose calls have no results is rejected by every wire.
/// Returns how many calls it answered.
pub fn close_unanswered_calls(history: &mut Vec<WireMessage>, result: &str) -> usize {
    let Some(last_call) = history
        .iter()
        .rposition(|message| message.role == Role::Assistant && !message.tool_calls.is_empty())
    else {
        return 0;
    };
    let answered: std::collections::HashSet<String> = history[last_call + 1..]
        .iter()
        .filter_map(|message| message.tool_call_id.clone())
        .collect();
    let missing: Vec<ToolCall> = history[last_call]
        .tool_calls
        .iter()
        .filter(|call| !answered.contains(&call.id))
        .cloned()
        .collect();
    for call in &missing {
        history.push(WireMessage::tool_result(call, result));
    }
    missing.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checkpoint_paths_stay_inside_the_workspace() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("deep/dir")).unwrap();
        std::fs::write(root.join("exists.txt"), "x").unwrap();

        let ok = |args: &str| checkpoint_path(args, Some(root.as_path())).expect("contained");
        // A plain relative name...
        assert_eq!(ok(r#"{"path": "thing.txt"}"#), root.join("thing.txt"));
        // ...one in a directory that exists...
        assert_eq!(
            ok(r#"{"path": "deep/thing.txt"}"#),
            root.join("deep/thing.txt")
        );
        // ...and a new file under an existing directory (the tail is
        // joined back onto the canonicalized ancestor).
        assert_eq!(
            ok(r#"{"path": "deep/dir/new.txt"}"#),
            root.join("deep/dir/new.txt")
        );
        // `..` that stays inside is fine.
        assert_eq!(
            ok(r#"{"file_path": "deep/../thing.txt"}"#),
            root.join("thing.txt")
        );

        let refused = |args: &str| checkpoint_path(args, Some(root.as_path())).is_none();
        // Absolute paths outside the workspace, the model's home
        // directory is not the workspace's business.
        assert!(refused(r#"{"path": "/etc/passwd"}"#));
        assert!(refused(r#"{"file_path": "/home/someone/.bashrc"}"#));
        // `..` that walks out.
        assert!(refused(r#"{"path": "../../escape.txt"}"#));
        // A symlink inside the workspace pointing out of it.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", root.join("sneaky")).unwrap();
            assert!(refused(r#"{"path": "sneaky/passwd"}"#));
        }
        // No workspace root at all: nothing is pre-approvable.
        assert!(checkpoint_path(r#"{"path": "thing.txt"}"#, None).is_none());
        // Arguments with no path at all.
        assert!(checkpoint_path(r#"{"command": "ls"}"#, Some(root.as_path())).is_none());
    }

    use crate::Event;
    use crate::ToolResult;
    use crate::permissions::PermissionBroker;
    use futures_core::Stream;
    use std::task::{Context as TaskContext, Poll, Waker};
    use std::time::Duration;

    /// Pops one scripted event list per stream() call, turn one emits a
    /// tool call, turn two the final text.
    struct ScriptedProvider {
        scripts: std::sync::Mutex<Vec<Vec<Event>>>,
        /// Requests seen, for assertions about what the loop sends.
        seen: std::sync::Mutex<Vec<crate::Request>>,
    }

    impl crate::provider::Provider for ScriptedProvider {
        fn stream(&self, request: crate::Request) -> BoxStream<'static, Event> {
            self.seen.lock().unwrap().push(request);
            let script = self.scripts.lock().unwrap().remove(0);
            Box::pin(futures_util::stream::iter(script))
        }
    }

    struct StubTool {
        name: &'static str,
        mutates: bool,
        asks: bool,
        result: &'static str,
    }

    impl StubTool {
        fn new(name: &'static str, mutates: bool, result: &'static str) -> Self {
            Self {
                name,
                mutates,
                asks: mutates,
                result,
            }
        }

        /// A read-only tool that still owes a card, the web_fetch
        /// shape: nothing checkpoints, the prompt is the whole gate.
        fn gated(name: &'static str, result: &'static str) -> Self {
            Self {
                name,
                mutates: false,
                asks: true,
                result,
            }
        }
    }

    impl Tool for StubTool {
        fn name(&self) -> &'static str {
            self.name
        }

        fn description(&self) -> String {
            "stub".into()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        fn mutates(&self) -> bool {
            self.mutates
        }

        fn asks(&self) -> bool {
            self.asks
        }

        fn run<'a>(
            &'a self,
            _arguments: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>> {
            Box::pin(async { Ok(self.result.to_string()) })
        }
    }

    /// Polls the stream to completion, answering permission prompts
    /// through the broker (the loop runs on the provider's own runtime).
    fn drain(
        stream: BoxStream<'static, LoopEvent>,
        broker: &Arc<PermissionBroker>,
        decide: impl Fn(&ToolCall) -> PermissionDecision,
    ) -> Vec<LoopEvent> {
        let started = std::time::Instant::now();
        let mut stream = stream;
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);
        let mut events = Vec::new();
        loop {
            match std::pin::Pin::new(&mut stream).poll_next(&mut cx) {
                Poll::Ready(Some(event)) => {
                    // Answer the prompt and don't keep the event: the
                    // ask is settled, no test inspects it.
                    if let LoopEvent::ToolCallRequested { id, call } = event {
                        broker.respond(id, decide(&call));
                    } else {
                        events.push(event);
                    }
                }
                Poll::Ready(None) => return events,
                Poll::Pending => {
                    // The hang guard: a helper this layer cannot time
                    // out must fail fast and by name, never spin
                    // politely forever.
                    if started.elapsed() > Duration::from_secs(10) {
                        panic!("the stream stayed Pending for 10s, an await never resolved");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    /// The model asks, the human clicks an option, the option's text
    /// becomes the tool result the model sees. The ask surfaces as a
    /// ToolCallRequested (the card) even though ask_user mutates
    /// nothing: the card is the ask.
    #[test]
    fn the_answered_ask_rides_back_as_the_tool_result() {
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![
                vec![
                    Event::ToolCall {
                        id: "a1".into(),
                        name: "ask_user".into(),
                        arguments: serde_json::json!({
                            "question": "Which database?",
                            "options": ["postgres", "sqlite"],
                            "allow_custom": true
                        })
                        .to_string(),
                    },
                    Event::Done,
                ],
                // The second round ends the turn; without it the
                // provider would re-ask every round until the budget.
                vec![Event::Token { delta: "ok".into() }, Event::Done],
            ]),
        });
        let ask = StubTool::new("ask_user", false, "unreachable");
        // The broker must be the spec's own: answering a different
        // broker's ask leaves the loop's await pending forever, the
        // exact wedge the drain guard below exists to name.
        let (spec, broker) = turn(
            provider,
            registry(&[Arc::new(ask)]),
            Arc::new(tokio::sync::Mutex::new(Vec::new())),
            "go",
        );
        let events = drain(run_turn(spec), &broker, |_call| {
            // The UI's option click arrives as deny-with-feedback:
            // the same path a typed reply takes.
            PermissionDecision::Deny {
                feedback: Some("sqlite".into()),
            }
        });

        let result = tool_result(&events, "ask_user");
        assert_eq!(result, "sqlite", "the clicked option reached the model");
    }

    /// A malformed ask is the model's mistake: it reaches the model as
    /// an error, never a hang.
    #[test]
    fn a_malformed_ask_is_an_error_result() {
        struct BadAskProvider;
        impl crate::provider::Provider for BadAskProvider {
            fn stream(&self, _request: crate::Request) -> BoxStream<'static, crate::Event> {
                Box::pin(futures_util::stream::iter(vec![
                    crate::Event::ToolCall {
                        id: "a1".into(),
                        name: "ask_user".into(),
                        arguments: serde_json::json!({
                            "question": "Which?",
                            "options": ["only one"]
                        })
                        .to_string(),
                    },
                    crate::Event::Done,
                ]))
            }
        }

        let ask = StubTool::new("ask_user", false, "unreachable");
        let (spec, broker) = turn(
            Arc::new(BadAskProvider),
            registry(&[Arc::new(ask)]),
            Arc::new(tokio::sync::Mutex::new(Vec::new())),
            "go",
        );
        let events = drain(run_turn(spec), &broker, |_| PermissionDecision::Approve);

        let result = tool_result(&events, "ask_user");
        assert!(
            result.contains("error:"),
            "one option is a model mistake, told so: {result}"
        );
    }

    /// The card's call keeps the model's own id: the UI pairs cards
    /// with their results by it (a synthetic id left every answered
    /// ask card pending in the transcript forever).
    #[test]
    fn the_ask_card_keeps_the_models_call_id() {
        let ask = parse_ask(r#"{"question": "Which?", "options": ["a", "b"]}"#, "a1").expect("ask");
        assert_eq!(ask.call.id, "a1");
        assert_eq!(ask.call.name, "ask_user");
    }

    /// A script of one tool-call round, then a plain-text wrap-up.
    fn one_call_then_text(name: &'static str) -> Vec<Vec<Event>> {
        vec![
            vec![
                Event::ToolCall {
                    id: "c0".into(),
                    name: name.into(),
                    arguments: "{}".into(),
                },
                Event::Done,
            ],
            vec![Event::Token { delta: "ok".into() }, Event::Done],
        ]
    }

    /// A tool can owe a card without mutating anything (web_fetch is
    /// the shape: the model chose the destination, and the effect is
    /// whatever the body carries, outbound bytes included). In
    /// supervised the gate asks; nothing runs before the answer.
    #[test]
    fn an_ask_gate_prompts_in_supervised() {
        let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(one_call_then_text("watch")),
        });
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (spec, broker) = turn(
            provider,
            registry(&[Arc::new(StubTool::gated("watch", "spied"))]),
            history.clone(),
            "go",
        );
        let flag = asked.clone();
        let events = drain(run_turn(spec), &broker, move |_| {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
            PermissionDecision::Approve
        });

        assert!(
            asked.load(std::sync::atomic::Ordering::Relaxed),
            "the gated tool must face the prompt before it runs"
        );
        assert_eq!(tool_result(&events, "watch"), "spied");
    }

    /// Full access settles the same gate without a card: the mode is
    /// the human saying nothing asks.
    #[test]
    fn full_access_settles_an_ask_gate_without_a_prompt() {
        let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(one_call_then_text("watch")),
        });
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (mut spec, broker) = turn(
            provider,
            registry(&[Arc::new(StubTool::gated("watch", "spied"))]),
            history.clone(),
            "go",
        );
        spec.permission_mode = PermissionMode::FullAccess;
        let flag = asked.clone();
        let events = drain(run_turn(spec), &broker, move |_| {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
            PermissionDecision::Approve
        });

        assert!(
            !asked.load(std::sync::atomic::Ordering::Relaxed),
            "full access means no card"
        );
        assert_eq!(tool_result(&events, "watch"), "spied");
    }

    /// The containment flag is per-call, not turn-wide: a gated
    /// read-only tool (web_fetch is the shape) must face its card in
    /// auto mode even when the turn runs sandboxed, because the
    /// sandbox contains bash, not the fetch.
    #[test]
    fn auto_mode_contains_bash_not_the_fetch() {
        let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(one_call_then_text("watch")),
        });
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (mut spec, broker) = turn(
            provider,
            registry(&[Arc::new(StubTool::gated("watch", "spied"))]),
            history.clone(),
            "go",
        );
        spec.permission_mode = PermissionMode::Auto;
        spec.sandbox_bash = true;
        let flag = asked.clone();
        let events = drain(run_turn(spec), &broker, move |_| {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
            PermissionDecision::Approve
        });

        assert!(
            asked.load(std::sync::atomic::Ordering::Relaxed),
            "a gated tool must card in auto mode: the sandbox contains bash, not the fetch"
        );
        assert_eq!(tool_result(&events, "watch"), "spied");
    }

    /// A gated tool that vouches for one call (`Tool::contained`, a
    /// screenshot of a workspace page) runs unasked in auto, and still
    /// asks in supervised: the pass is the auto mode's, not the tool's.
    #[test]
    fn a_tools_contained_call_runs_unasked_only_in_auto() {
        struct Local;
        impl Tool for Local {
            fn name(&self) -> &'static str {
                "shoot"
            }
            fn description(&self) -> String {
                "shoot".into()
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            fn asks(&self) -> bool {
                true
            }
            fn contained(&self, _arguments: &str) -> bool {
                true
            }
            fn run<'a>(
                &'a self,
                _arguments: &'a str,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>>
            {
                Box::pin(async { Ok("shot".to_string()) })
            }
        }
        for (mode, should_ask) in [
            (PermissionMode::Auto, false),
            (PermissionMode::Supervised, true),
        ] {
            let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let provider = Arc::new(ScriptedProvider {
                seen: std::sync::Mutex::new(Vec::new()),
                scripts: std::sync::Mutex::new(one_call_then_text("shoot")),
            });
            let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            let (mut spec, broker) = turn(
                provider,
                registry(&[Arc::new(Local)]),
                history.clone(),
                "go",
            );
            spec.permission_mode = mode;
            let flag = asked.clone();
            let events = drain(run_turn(spec), &broker, move |_| {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                PermissionDecision::Approve
            });
            assert_eq!(
                asked.load(std::sync::atomic::Ordering::Relaxed),
                should_ask,
                "{mode:?}"
            );
            assert_eq!(tool_result(&events, "shoot"), "shot");
        }
    }

    /// And the contained bash itself stays pre-approved in auto: the
    /// mode's whole point survives the tightening.
    #[test]
    fn auto_mode_still_pre_approves_contained_bash() {
        let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(one_call_then_text("bash")),
        });
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (mut spec, broker) = turn(
            provider,
            registry(&[Arc::new(StubTool::new("bash", true, "ran"))]),
            history.clone(),
            "go",
        );
        spec.permission_mode = PermissionMode::Auto;
        spec.sandbox_bash = true;
        let flag = asked.clone();
        let events = drain(run_turn(spec), &broker, move |_| {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
            PermissionDecision::Approve
        });

        assert!(
            !asked.load(std::sync::atomic::Ordering::Relaxed),
            "contained bash is the one thing auto exists to pre-approve"
        );
        assert_eq!(tool_result(&events, "bash"), "ran");
    }

    /// The gate does not catch reads: a tool that neither mutates nor
    /// asks runs uncarded, the read-only contract stands.
    #[test]
    fn a_plain_read_still_runs_without_a_card() {
        let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(one_call_then_text("peek")),
        });
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (spec, broker) = turn(
            provider,
            registry(&[Arc::new(StubTool::new("peek", false, "saw"))]),
            history.clone(),
            "go",
        );
        let flag = asked.clone();
        let events = drain(run_turn(spec), &broker, move |_| {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
            PermissionDecision::Approve
        });

        assert!(
            !asked.load(std::sync::atomic::Ordering::Relaxed),
            "a plain read must not gain a card"
        );
        assert_eq!(tool_result(&events, "peek"), "saw");
    }

    fn registry(tools: &[Arc<dyn Tool>]) -> ToolRegistry {
        Arc::new(
            tools
                .iter()
                .map(|tool| (tool.name().to_string(), tool.clone()))
                .collect(),
        )
    }

    /// Boring TurnSpec for tests; override fields for the special cases.
    fn turn(
        provider: Arc<dyn crate::provider::Provider>,
        tools: ToolRegistry,
        history: SharedHistory,
        message: &str,
    ) -> (TurnSpec, Arc<PermissionBroker>) {
        let broker = Arc::new(PermissionBroker::new());
        (
            TurnSpec {
                user_message: Some(message.into()),
                permission_broker: Some(broker.clone()),
                ..TurnSpec::new(provider, tools, history)
            },
            broker,
        )
    }

    fn tool_result(events: &[LoopEvent], name: &str) -> String {
        events
            .iter()
            .find_map(|event| match event {
                LoopEvent::ToolResult {
                    name: found,
                    result,
                    ..
                } if found == name => Some(result.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no ToolResult for {name}"))
    }

    /// A limited turn is terminal and reaches the UI as its own event,
    /// the resume affordance hangs on telling it apart from an error.
    #[test]
    fn a_rate_limited_turn_ends_in_loop_event_limited() {
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![vec![
                Event::Started,
                Event::Limited {
                    message: "429: rate limit exceeded".into(),
                },
            ]]),
        });
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (spec, broker) = turn(provider, registry(&[]), history.clone(), "go");
        let events = drain(run_turn(spec), &broker, |_| PermissionDecision::Approve);

        assert!(
            events.iter().any(
                |event| matches!(event, LoopEvent::Limited { message } if message.contains("429"))
            ),
            "the limit relays as LoopEvent::Limited"
        );
        // Nothing after it: the turn is over. And the user message that
        // opened the turn is in history, what a resume continues from.
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, LoopEvent::Done | LoopEvent::Error { .. }))
        );
        assert_eq!(
            history.blocking_lock().last().unwrap().content.as_deref(),
            Some("go")
        );
    }

    /// One scripted peek per round: `count` rounds of a tool call,
    /// then a bare wrap-up (or another tool call) for the last round.
    fn round_scripts(count: usize, last: Event) -> Vec<Vec<Event>> {
        let mut scripts = Vec::new();
        for i in 0..count {
            let _ = i;
            scripts.push(vec![
                Event::ToolCall {
                    id: format!("c{i}"),
                    name: "peek".into(),
                    arguments: "{}".into(),
                },
                Event::Done,
            ]);
        }
        scripts.push(vec![last, Event::Done]);
        scripts
    }

    fn peek_registry() -> (Arc<ScriptedProvider>, ToolRegistry) {
        let peek: Arc<dyn Tool> = Arc::new(StubTool::new("peek", false, "peeked"));
        (
            Arc::new(ScriptedProvider {
                seen: std::sync::Mutex::new(Vec::new()),
                scripts: std::sync::Mutex::new(Vec::new()),
            }),
            registry(&[peek]),
        )
    }

    /// The round budget is a soft edge: at DEFAULT_MAX_TOOL_ROUNDS the loop
    /// nudges the model to wrap up (in band, a history row) and keeps
    /// going, so ordinary work that merely runs long is never cut off.
    #[test]
    fn the_round_budget_nudges_and_keeps_going() {
        let (provider, tools) = peek_registry();
        provider.scripts.lock().unwrap().extend(round_scripts(
            DEFAULT_MAX_TOOL_ROUNDS,
            Event::Token {
                delta: "wrapped up".into(),
            },
        ));
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (spec, broker) = turn(provider, tools, history.clone(), "go");
        let events = drain(run_turn(spec), &broker, |_| {
            panic!("read-only tools must not prompt")
        });

        assert!(
            events
                .iter()
                .any(|event| matches!(event, LoopEvent::Nudged)),
            "the wrap-up nudge fired at the budget"
        );
        assert!(
            events.iter().any(|event| matches!(event, LoopEvent::Done)),
            "the turn finished in the grace window, not paused"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, LoopEvent::Budgeted { .. })),
            "no pause while the model wrapped up"
        );
        // The nudge is in band: a history user row the model reads.
        let history = history.blocking_lock();
        assert!(
            history.iter().any(|message| message.role == Role::User
                && message
                    .content
                    .as_deref()
                    .is_some_and(|text| text.contains("tool-round budget"))),
            "the nudge rides history as a user row"
        );
    }

    /// The budgets are the spec's: a lowered budget nudges at its
    /// round, not the constant's, and the pause reports the resolved
    /// numbers.
    #[test]
    fn the_budgets_are_the_specs() {
        let (provider, tools) = peek_registry();
        // Two tool rounds, then the nudge round, then a round that
        // ignores the nudge, then the pause.
        provider.scripts.lock().unwrap().extend(round_scripts(
            4,
            Event::Token {
                delta: "still going".into(),
            },
        ));
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (mut spec, broker) = turn(provider, tools, history.clone(), "go");
        spec.max_tool_rounds = Some(2);
        spec.grace_rounds = Some(1);
        let events = drain(run_turn(spec), &broker, |_| {
            panic!("read-only tools must not prompt")
        });
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LoopEvent::Nudged)),
            "the nudge fired at the spec's budget, round 2"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LoopEvent::Budgeted { message, .. }
                    if message.contains("2 tool rounds") && message.contains("1 grace rounds"))),
            "the pause reports the resolved numbers"
        );
        assert!(
            !events.iter().any(|event| matches!(event, LoopEvent::Done)),
            "the model ignored the nudge through the grace window"
        );
    }

    /// A model that ignores the wrap-up nudge through the whole grace
    /// window pauses the turn like a rate limit: Budgeted, history
    /// intact, resumable. Not an error, the work is not lost.
    #[test]
    fn ignoring_the_wrap_up_nudge_pauses_the_turn_resumable() {
        let (provider, tools) = peek_registry();
        provider.scripts.lock().unwrap().extend(round_scripts(
            DEFAULT_MAX_TOOL_ROUNDS + DEFAULT_GRACE_ROUNDS,
            Event::ToolCall {
                id: "still-going".into(),
                name: "peek".into(),
                arguments: "{}".into(),
            },
        ));
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (spec, broker) = turn(provider, tools, history.clone(), "go");
        let events = drain(run_turn(spec), &broker, |_| {
            panic!("read-only tools must not prompt")
        });

        assert!(
            events
                .iter()
                .any(|event| matches!(event, LoopEvent::Nudged)),
            "the nudge fired once at the budget"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, LoopEvent::Budgeted { .. }))
                .count(),
            1,
            "exactly one pause, then the stream ends"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, LoopEvent::Done | LoopEvent::Error { .. })),
            "a grace-window blowout pauses, it neither finishes nor errors"
        );
        // The nudge is in history right above the last tool rows: what
        // a resume turn continues from, the wrap-up ask with the
        // results in hand.
        let history = history.blocking_lock();
        assert!(
            history.iter().any(|message| message.role == Role::User
                && message
                    .content
                    .as_deref()
                    .is_some_and(|text| text.contains("tool-round budget"))),
            "the nudge rides history as a user row"
        );
    }

    /// A steer landing on the budget boundary never dies there: it is
    /// drained and buys the next round, the budget bounds the model,
    /// not the human's interruption.
    #[test]
    fn a_steer_at_the_budget_boundary_buys_its_round() {
        let (provider, tools) = peek_registry();
        provider.scripts.lock().unwrap().extend(round_scripts(
            DEFAULT_MAX_TOOL_ROUNDS,
            Event::Token {
                delta: "answering the steer".into(),
            },
        ));
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (mut spec, broker) = turn(provider, tools, history.clone(), "go");
        let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel();
        steer_tx
            .send("wait, check the other file".to_string())
            .unwrap();
        spec.steer = Some(steer_rx);
        let events = drain(run_turn(spec), &broker, |_| {
            panic!("read-only tools must not prompt")
        });

        assert!(
            events.iter().any(|event| matches!(event,
                    LoopEvent::Steered { text } if text == "wait, check the other file")),
            "the steer landed at the boundary instead of dying"
        );
        assert!(events.iter().any(|event| matches!(event, LoopEvent::Done)));
    }

    /// A resume turn pushes nothing: it continues from history as it
    /// stands, the rate-limited round's rows are still there.
    #[test]
    fn a_resume_turn_continues_from_history_without_a_new_user_message() {
        let call = ToolCall {
            id: "c1".into(),
            name: "stub".into(),
            arguments: "{}".into(),
        };
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(vec![
            WireMessage::user("go"),
            WireMessage::assistant_with_tool_calls(Some("on it".into()), vec![call.clone()]),
            WireMessage::tool_result(&call, "done"),
        ]));
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![vec![Event::Started, Event::Done]]),
        });
        let (mut spec, broker) = turn(provider, registry(&[]), history.clone(), "ignored");
        spec.user_message = None; // resume
        let events = drain(run_turn(spec), &broker, |_| PermissionDecision::Approve);

        assert!(matches!(events.last(), Some(LoopEvent::Done)));
        let history = history.blocking_lock();
        assert_eq!(
            history
                .iter()
                .filter(|message| message.role == Role::User)
                .count(),
            1,
            "no second user row: the resume continued the existing history"
        );
    }

    #[test]
    fn steering_drains_at_the_round_boundary() {
        // Steers queued before the turn: both land at the first
        // boundary, before request one, the model reads them with it.
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![vec![
                Event::Token {
                    delta: "partial".into(),
                },
                Event::Done,
            ]]),
        });
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (mut spec, broker) = turn(provider, registry(&[]), history.clone(), "go");
        let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel();
        steer_tx.send("first steer".to_string()).unwrap();
        steer_tx.send("second steer".to_string()).unwrap();
        spec.steer = Some(steer_rx);
        let events = drain(run_turn(spec), &broker, |_| {
            panic!("no tools to prompt for")
        });

        let steered: Vec<&String> = events
            .iter()
            .filter_map(|event| match event {
                LoopEvent::Steered { text } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(steered, ["first steer", "second steer"]);
        let history = history.blocking_lock();
        // user, steer 1, steer 2, assistant
        assert_eq!(history.len(), 4);
        assert_eq!(history[1], WireMessage::user("first steer"));
        assert_eq!(history[2], WireMessage::user("second steer"));
        assert_eq!(history[3], WireMessage::assistant("partial"));
    }

    /// A provider that injects a steer the moment its script hits
    /// Done, the user typed while the model was finishing, the
    /// classic rescue shape. Once.
    struct SteerOnDone {
        inner: Arc<ScriptedProvider>,
        steer_tx: tokio::sync::mpsc::UnboundedSender<String>,
        steered: Arc<std::sync::atomic::AtomicBool>,
    }

    impl crate::provider::Provider for SteerOnDone {
        fn stream(&self, request: crate::Request) -> BoxStream<'static, Event> {
            let inner = self.inner.clone();
            let steer_tx = self.steer_tx.clone();
            let steered = self.steered.clone();
            Box::pin(async_stream::stream! {
                let mut inner_stream = inner.stream(request);
                while let Some(event) = inner_stream.next().await {
                    let done = matches!(event, Event::Done);
                    yield event;
                    if done
                        && !steered.swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        let _ = steer_tx.send("late steer".to_string());
                    }
                }
            })
        }
    }

    #[test]
    fn a_steer_rescues_the_final_round() {
        // The model finishes, but the steer landed first, so the loop
        // turns back and answers it instead of calling it done.
        let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel();
        let provider = Arc::new(SteerOnDone {
            inner: Arc::new(ScriptedProvider {
                seen: std::sync::Mutex::new(Vec::new()),
                scripts: std::sync::Mutex::new(vec![
                    vec![
                        Event::Token {
                            delta: "partial".into(),
                        },
                        Event::Done,
                    ],
                    vec![
                        Event::Token {
                            delta: "answered the steer".into(),
                        },
                        Event::Done,
                    ],
                ]),
            }),
            steer_tx,
            steered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (mut spec, broker) = turn(provider, registry(&[]), history.clone(), "go");
        spec.steer = Some(steer_rx);
        let events = drain(run_turn(spec), &broker, |_| {
            panic!("no tools to prompt for")
        });

        assert!(
            events
                .iter()
                .any(|event| matches!(event, LoopEvent::Steered { text } if text == "late steer"))
        );
        assert!(events.iter().any(|event| matches!(event, LoopEvent::Done)));
        let history = history.blocking_lock();
        // user, assistant round 1, steered user, assistant round 2
        assert_eq!(history.len(), 4);
        assert_eq!(history[1], WireMessage::assistant("partial"));
        assert_eq!(history[2], WireMessage::user("late steer"));
        assert_eq!(history[3], WireMessage::assistant("answered the steer"));
    }

    #[test]
    fn plain_answers_finish_without_tool_rounds() {
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![vec![
                Event::Token { delta: "hi".into() },
                Event::Done,
            ]]),
        });
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (spec, broker) = turn(provider, registry(&[]), history.clone(), "hello");
        let events = drain(run_turn(spec), &broker, |_| {
            unreachable!("no tools to prompt for")
        });

        assert!(events.iter().any(|event| matches!(event, LoopEvent::Done)));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, LoopEvent::ToolResult { .. }))
        );
        let history = history.blocking_lock();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0], WireMessage::user("hello"));
        assert_eq!(history[1], WireMessage::assistant("hi"));
    }

    #[test]
    fn a_nested_agents_md_rides_the_first_result_under_it() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("docs")).unwrap();
        std::fs::write(workspace.path().join("docs/AGENTS.md"), "no em dashes\n").unwrap();
        let call = |id: &str| Event::ToolCall {
            id: id.into(),
            name: "peek".into(),
            arguments: r#"{"path": "docs/guide.md"}"#.into(),
        };
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![
                vec![call("c1"), Event::Done],
                vec![call("c2"), Event::Done],
                vec![Event::Token { delta: "ok".into() }, Event::Done],
            ]),
        });
        let peek: Arc<dyn Tool> = Arc::new(StubTool::new("peek", false, "peeked"));
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (mut spec, broker) = turn(provider, registry(&[peek]), history.clone(), "look");
        spec.workspace_root = Some(workspace.path().to_path_buf());
        let events = drain(run_turn(spec), &broker, |_| panic!("reads never prompt"));

        let results: Vec<&String> = events
            .iter()
            .filter_map(|event| match event {
                LoopEvent::ToolResult { result, .. } => Some(result),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2);
        // The first touch carries the file, in the transcript's record
        // and in the model's history alike; the second owes nothing.
        assert!(results[0].starts_with("peeked\n\n[directory instructions: docs/AGENTS.md]\n"));
        assert!(results[0].contains("no em dashes"));
        assert_eq!(results[1], "peeked");
        let history = history.blocking_lock();
        assert_eq!(history[2].content.as_deref(), Some(results[0].as_str()));
    }

    #[test]
    fn read_only_tools_run_without_prompting() {
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![
                vec![
                    Event::ToolCall {
                        id: "c1".into(),
                        name: "peek".into(),
                        arguments: "{}".into(),
                    },
                    Event::Done,
                ],
                vec![
                    Event::Token {
                        delta: "done".into(),
                    },
                    Event::Done,
                ],
            ]),
        });
        let peek: Arc<dyn Tool> = Arc::new(StubTool::new("peek", false, "peeked"));
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (spec, broker) = turn(provider, registry(&[peek]), history.clone(), "look");
        let events = drain(run_turn(spec), &broker, |_| {
            panic!("read-only tools must not prompt")
        });

        assert_eq!(tool_result(&events, "peek"), "peeked");
        // The read's start is announced before its result, silent reads
        // would read as a hang in the UI.
        let started = events
            .iter()
            .position(|event| matches!(event, LoopEvent::ToolCallStarted { .. }))
            .expect("read-only tool announces itself");
        let finished = events
            .iter()
            .position(|event| matches!(event, LoopEvent::ToolResult { .. }))
            .expect("tool result");
        assert!(started < finished);
        assert!(events.iter().any(|event| matches!(event, LoopEvent::Done)));
        let history = history.blocking_lock();
        assert_eq!(history.len(), 4); // user, assistant+call, tool result, assistant
        assert_eq!(history[3], WireMessage::assistant("done"));
    }

    /// A tool that returns a picture beside its text (#50).
    struct PictureTool;

    impl Tool for PictureTool {
        fn name(&self) -> &'static str {
            "picture"
        }

        fn description(&self) -> String {
            "picture".into()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        fn run<'a>(
            &'a self,
            _arguments: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>> {
            Box::pin(async { Ok("saved to /c/shot.png".to_string()) })
        }

        fn run_with_images<'a>(
            &'a self,
            call: &'a ToolCall,
            progress: &'a crate::WorkerProgress,
            turn_token: Option<crate::TurnToken>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = (ToolResult, Vec<String>)> + Send + 'a>,
        > {
            Box::pin(async move {
                (
                    self.run_with_call(call, progress, turn_token).await,
                    vec!["data:image/png;base64,QUJD".to_string()],
                )
            })
        }
    }

    #[test]
    fn a_tools_images_reach_the_next_request_and_the_event() {
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![
                vec![
                    Event::ToolCall {
                        id: "c1".into(),
                        name: "picture".into(),
                        arguments: "{}".into(),
                    },
                    Event::Done,
                ],
                vec![
                    Event::Token {
                        delta: "I see it".into(),
                    },
                    Event::Done,
                ],
            ]),
        });
        let picture: Arc<dyn Tool> = Arc::new(PictureTool);
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (spec, broker) = turn(
            provider.clone(),
            registry(&[picture]),
            history.clone(),
            "look",
        );
        let events = drain(run_turn(spec), &broker, |_| PermissionDecision::Approve);

        let images = events
            .iter()
            .find_map(|event| match event {
                LoopEvent::ToolResult { images, .. } => Some(images.clone()),
                _ => None,
            })
            .expect("tool result");
        assert_eq!(images, vec!["data:image/png;base64,QUJD".to_string()]);
        // The request after the call carries the picture on the result.
        let seen = provider.seen.lock().unwrap();
        let result = seen[1]
            .messages
            .iter()
            .find(|message| message.role == Role::Tool)
            .expect("the result rides the second request");
        assert_eq!(result.content.as_deref(), Some("saved to /c/shot.png"));
        assert_eq!(result.attachments, images);
    }

    #[test]
    fn nothing_follows_done_turn_end_is_not_settlement() {
        // The heaviest path (a tool round) must still end the instant
        // Done is yielded: the loop does no settlement work, no
        // trailing persistence, no summary, nothing after the turn's
        // last event. If anything ever follows Done, this fails and
        // the doctrine needs a deliberate rewrite, not a silent slip.
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![
                vec![
                    Event::ToolCall {
                        id: "c1".into(),
                        name: "peek".into(),
                        arguments: "{}".into(),
                    },
                    Event::Done,
                ],
                vec![
                    Event::Token {
                        delta: "all done".into(),
                    },
                    Event::Done,
                ],
            ]),
        });
        let peek: Arc<dyn Tool> = Arc::new(StubTool::new("peek", false, "peeked"));
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (spec, broker) = turn(provider, registry(&[peek]), history.clone(), "look");
        let events = drain(run_turn(spec), &broker, |_| {
            panic!("read-only tools must not prompt")
        });

        let done = events
            .iter()
            .position(|event| matches!(event, LoopEvent::Done))
            .expect("the turn yields Done");
        assert_eq!(
            done,
            events.len() - 1,
            "nothing may follow Done, turn end is not settlement"
        );
    }

    #[test]
    fn mutating_tools_ask_and_honor_the_answer() {
        for (decided, expected) in [
            (PermissionDecision::Approve, "wrote"),
            (
                PermissionDecision::Deny { feedback: None },
                "error: denied by user",
            ),
            (
                PermissionDecision::Deny {
                    feedback: Some("no, use the staging dir".into()),
                },
                "error: denied by user, their reply: no, use the staging dir",
            ),
        ] {
            let provider = Arc::new(ScriptedProvider {
                seen: std::sync::Mutex::new(Vec::new()),
                scripts: std::sync::Mutex::new(vec![
                    vec![
                        Event::ToolCall {
                            id: "c1".into(),
                            name: "write_file".into(),
                            arguments: "{}".into(),
                        },
                        Event::Done,
                    ],
                    vec![Event::Token { delta: "ok".into() }, Event::Done],
                ]),
            });
            let writer: Arc<dyn Tool> = Arc::new(StubTool::new("write_file", true, "wrote"));
            let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            let (spec, broker) = turn(provider, registry(&[writer]), history.clone(), "go");
            let events = drain(run_turn(spec), &broker, |call| {
                assert_eq!(call.name, "write_file");
                decided.clone()
            });

            assert_eq!(tool_result(&events, "write_file"), expected);
            let history = history.blocking_lock();
            assert_eq!(
                history[2].content.as_deref(),
                Some(expected),
                "the model must see the denial, not just the UI"
            );
        }
    }

    #[test]
    fn approved_mutations_checkpoint_the_target_file() {
        let tmp = tempfile::tempdir().unwrap();
        let snapshots = Arc::new(SnapshotStore::open(&tmp.path().join("snaps")).unwrap());
        // The tool's argument is workspace-relative, the checkpoint
        // hook must resolve it the way tools do, or it would snapshot
        // the wrong file (an empty read against the process CWD).
        let workspace = tempfile::tempdir().unwrap();
        let target = workspace.path().join("thing.txt");
        std::fs::write(&target, "before").unwrap();

        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![
                vec![
                    Event::ToolCall {
                        id: "c1".into(),
                        name: "edit_file".into(),
                        arguments: r#"{"path": "thing.txt"}"#.into(),
                    },
                    Event::Done,
                ],
                vec![Event::Done],
            ]),
        });
        let editor: Arc<dyn Tool> = Arc::new(StubTool::new("edit_file", true, "edited"));
        let (mut spec, broker) = turn(
            provider,
            registry(&[editor]),
            Arc::new(tokio::sync::Mutex::new(Vec::new())),
            "",
        );
        spec.snapshots = Some(snapshots.clone());
        spec.workspace_root = Some(workspace.path().to_path_buf());
        spec.checkpoint_session = Some(crate::SessionId(7));
        let events = drain(run_turn(spec), &broker, |_| PermissionDecision::Approve);

        assert_eq!(tool_result(&events, "edit_file"), "edited");
        // Exactly one object, and it holds the file's real prior bytes,
        // not an empty read from the process CWD.
        let objects: Vec<_> = std::fs::read_dir(tmp.path().join("snaps/objects"))
            .unwrap()
            .collect();
        assert_eq!(objects.len(), 1);
        let object = objects[0].as_ref().unwrap().path();
        assert_eq!(std::fs::read(object).unwrap(), b"before");
        // The checkpoint answers to the call's own id, the exact key a
        // reloaded card asks by: its diff can be the real change.
        assert_eq!(
            snapshots
                .before_content_of_call(crate::SessionId(7), "c1")
                .unwrap(),
            Some(b"before".to_vec())
        );
    }

    #[test]
    fn accept_edits_auto_approves_checkpointed_edits_and_still_asks_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let snapshots = Arc::new(SnapshotStore::open(&tmp.path().join("snaps")).unwrap());
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("thing.txt"), "before").unwrap();

        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![
                vec![
                    Event::ToolCall {
                        id: "c1".into(),
                        name: "edit_file".into(),
                        arguments: r#"{"path": "thing.txt"}"#.into(),
                    },
                    Event::ToolCall {
                        id: "c2".into(),
                        name: "bash".into(),
                        arguments: r#"{"command": "ls"}"#.into(),
                    },
                    Event::Done,
                ],
                vec![Event::Done],
            ]),
        });
        let editor: Arc<dyn Tool> = Arc::new(StubTool::new("edit_file", true, "edited"));
        let shell: Arc<dyn Tool> = Arc::new(StubTool::new("bash", true, "shelled"));
        let (mut spec, broker) = turn(
            provider,
            registry(&[editor, shell]),
            Arc::new(tokio::sync::Mutex::new(Vec::new())),
            "",
        );
        spec.snapshots = Some(snapshots.clone());
        spec.workspace_root = Some(workspace.path().to_path_buf());
        spec.permission_mode = PermissionMode::AcceptEdits;

        let events = drain(run_turn(spec), &broker, |call| {
            // The edit is checkpointed and reversible, it ran unasked.
            // Bash touches the world outside a single file: it asks.
            assert_eq!(call.name, "bash", "only the non-checkpointed call asks");
            PermissionDecision::Approve
        });

        assert_eq!(tool_result(&events, "edit_file"), "edited");
        assert_eq!(tool_result(&events, "bash"), "shelled");
        // The auto-approved edit is on the ledger exactly like an
        // approved one.
        assert_eq!(
            std::fs::read_dir(tmp.path().join("snaps/objects"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn the_auto_mode_keys_bash_off_the_sandbox() {
        // Sandbox on: contained bash runs unasked. Sandbox off: the
        // same ask prompts, the mode never pretends a kernel that
        // isn't enforcing is enforcing.
        for (sandboxed, expected_asks) in [(true, 0), (false, 1)] {
            let provider = Arc::new(ScriptedProvider {
                seen: std::sync::Mutex::new(Vec::new()),
                scripts: std::sync::Mutex::new(vec![
                    vec![
                        Event::ToolCall {
                            id: "c1".into(),
                            name: "bash".into(),
                            arguments: r#"{"command": "ls"}"#.into(),
                        },
                        Event::Done,
                    ],
                    vec![Event::Done],
                ]),
            });
            let shell: Arc<dyn Tool> = Arc::new(StubTool::new("bash", true, "shelled"));
            let (mut spec, broker) = turn(
                provider,
                registry(&[shell]),
                Arc::new(tokio::sync::Mutex::new(Vec::new())),
                "",
            );
            spec.permission_mode = PermissionMode::Auto;
            spec.sandbox_bash = sandboxed;
            let asks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count = asks.clone();
            let events = drain(run_turn(spec), &broker, move |call| {
                assert_eq!(call.name, "bash");
                count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                PermissionDecision::Approve
            });
            assert_eq!(tool_result(&events, "bash"), "shelled");
            assert_eq!(
                asks.load(std::sync::atomic::Ordering::Relaxed),
                expected_asks,
                "sandbox = {sandboxed}"
            );
        }
    }

    #[test]
    fn full_access_runs_mutations_without_an_ask() {
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![
                vec![
                    Event::ToolCall {
                        id: "c1".into(),
                        name: "bash".into(),
                        arguments: r#"{"command": "ls"}"#.into(),
                    },
                    Event::Done,
                ],
                vec![Event::Done],
            ]),
        });
        let shell: Arc<dyn Tool> = Arc::new(StubTool::new("bash", true, "shelled"));
        let (mut spec, broker) = turn(
            provider,
            registry(&[shell]),
            Arc::new(tokio::sync::Mutex::new(Vec::new())),
            "",
        );
        spec.permission_mode = PermissionMode::FullAccess;
        let events = drain(run_turn(spec), &broker, |_| {
            panic!("full access must not ask, the human is the only thing removed")
        });
        assert_eq!(tool_result(&events, "bash"), "shelled");
    }

    #[test]
    fn unknown_tools_report_back_rather_than_die() {
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![
                vec![
                    Event::ToolCall {
                        id: "c1".into(),
                        name: "mystery".into(),
                        arguments: "{}".into(),
                    },
                    Event::Done,
                ],
                vec![Event::Done],
            ]),
        });
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (spec, broker) = turn(provider, registry(&[]), history.clone(), "go");
        let events = drain(run_turn(spec), &broker, |_| panic!("nothing to prompt for"));

        assert!(tool_result(&events, "mystery").starts_with("error: unknown tool"));
    }

    #[test]
    fn hard_blocked_calls_never_reach_the_prompt() {
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![
                vec![
                    Event::ToolCall {
                        id: "c1".into(),
                        name: "bash".into(),
                        arguments: r#"{"command": "rm -rf /"}"#.into(),
                    },
                    Event::Done,
                ],
                vec![Event::Done],
            ]),
        });
        let (spec, broker) = turn(
            provider,
            registry(&[Arc::new(BlockedTool)]),
            Arc::new(tokio::sync::Mutex::new(Vec::new())),
            "go",
        );
        let events = drain(run_turn(spec), &broker, |_| {
            panic!("a vet failure must not prompt, there is nothing to approve")
        });

        assert!(tool_result(&events, "bash").contains("blocked by policy"));
    }

    /// A bash stand-in that answers from a script, one result per run,
    /// and records the commands it was given.
    struct ScriptedBash {
        results: std::sync::Mutex<Vec<&'static str>>,
        commands: std::sync::Mutex<Vec<String>>,
    }

    impl ScriptedBash {
        fn new(results: &[&'static str]) -> Arc<Self> {
            Arc::new(Self {
                results: std::sync::Mutex::new(results.iter().rev().copied().collect()),
                commands: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    impl Tool for ScriptedBash {
        fn name(&self) -> &'static str {
            "bash"
        }
        fn description(&self) -> String {
            "scripted bash".into()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn mutates(&self) -> bool {
            true
        }
        fn run<'a>(
            &'a self,
            arguments: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>> {
            let command = serde_json::from_str::<serde_json::Value>(arguments)
                .ok()
                .and_then(|value| value["command"].as_str().map(str::to_string))
                .unwrap_or_default();
            self.commands.lock().unwrap().push(command);
            let result = self.results.lock().unwrap().pop().unwrap_or("ok");
            Box::pin(async move { Ok(result.to_string()) })
        }
    }

    fn edit_round(id: &str) -> Vec<Event> {
        vec![
            Event::ToolCall {
                id: id.into(),
                name: "edit_file".into(),
                arguments: r#"{"path": "a.rs"}"#.into(),
            },
            Event::Done,
        ]
    }

    fn say(text: &str) -> Vec<Event> {
        vec![Event::Token { delta: text.into() }, Event::Done]
    }

    /// Runs a scripted turn with a check configured; returns the events,
    /// the requests the model saw, and the commands bash ran.
    fn checked_turn(
        scripts: Vec<Vec<Event>>,
        bash_results: &[&'static str],
        decide: impl Fn(&ToolCall) -> PermissionDecision,
    ) -> (Vec<LoopEvent>, Vec<crate::Request>, Vec<String>) {
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(scripts),
        });
        let edit: Arc<dyn Tool> = Arc::new(StubTool::new("edit_file", true, "edited a.rs"));
        let bash = ScriptedBash::new(bash_results);
        let tools = registry(&[edit, bash.clone() as Arc<dyn Tool>]);
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (mut spec, broker) = turn(provider.clone(), tools, history, "fix a.rs");
        spec.verify = Some("cargo check".into());
        let events = drain(run_turn(spec), &broker, decide);
        let seen = provider.seen.lock().unwrap().clone();
        let commands = bash.commands.lock().unwrap().clone();
        (events, seen, commands)
    }

    fn checked(events: &[LoopEvent]) -> Vec<bool> {
        events
            .iter()
            .filter_map(|event| match event {
                LoopEvent::Checked { passed } => Some(*passed),
                _ => None,
            })
            .collect()
    }

    fn last_tool_text(request: &crate::Request) -> String {
        request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Tool)
            .and_then(|message| message.content.clone())
            .unwrap_or_default()
    }

    /// #40: an editing turn the model ends gets checked; a pass ends it
    /// with no extra model round.
    #[test]
    fn a_passing_check_ends_the_editing_turn_without_another_round() {
        let (events, seen, commands) = checked_turn(
            vec![edit_round("e1"), say("done")],
            &["Finished dev"],
            |_| PermissionDecision::Approve,
        );
        assert_eq!(commands, vec!["cargo check".to_string()]);
        assert_eq!(checked(&events), vec![true]);
        assert_eq!(seen.len(), 2, "the pass ends the turn: no third request");
        assert!(matches!(events.last(), Some(LoopEvent::Done)));
    }

    /// A failure goes back to the model as "not done"; the fix is
    /// checked again.
    #[test]
    fn a_failing_check_bounces_the_turn_back_until_it_passes() {
        let (events, seen, commands) = checked_turn(
            vec![
                edit_round("e1"),
                say("done"),
                edit_round("e2"),
                say("fixed"),
            ],
            &[
                "exit 101:\nsrc/a.rs:3:1: error[E0308]: mismatched types",
                "Finished dev",
            ],
            |_| PermissionDecision::Approve,
        );
        assert_eq!(commands.len(), 2);
        assert_eq!(checked(&events), vec![false, true]);
        let bounced = last_tool_text(&seen[2]);
        assert!(
            bounced.starts_with("[gertie ran the project check")
                && bounced.contains("mismatched types")
                && bounced.contains("the turn is not done"),
            "{bounced}"
        );
        assert_eq!(seen.len(), 4);
    }

    /// No edit, no check: a question answered from reading costs nothing.
    #[test]
    fn a_turn_without_edits_is_not_checked() {
        let (events, seen, commands) = checked_turn(vec![say("it does X")], &[], |_| {
            panic!("nothing should ask")
        });
        assert!(commands.is_empty());
        assert!(checked(&events).is_empty());
        assert_eq!(seen.len(), 1);
    }

    /// The check is bounded: after VERIFY_RUNS failures the model is told
    /// to report, and the turn ends on its answer.
    #[test]
    fn checks_stop_after_the_cap_and_ask_for_a_report() {
        let mut scripts = Vec::new();
        for index in 0..VERIFY_RUNS {
            scripts.push(edit_round(&format!("e{index}")));
            scripts.push(say("done"));
        }
        scripts.push(say("still failing: the linker is missing"));
        let failures = vec!["exit 1:\nerror: linker not found"; VERIFY_RUNS];
        let (events, seen, commands) =
            checked_turn(scripts, &failures, |_| PermissionDecision::Approve);
        assert_eq!(commands.len(), VERIFY_RUNS);
        assert_eq!(checked(&events), vec![false; VERIFY_RUNS]);
        let last = last_tool_text(seen.last().unwrap());
        assert!(last.contains("will not run again this turn"), "{last}");
        assert!(matches!(events.last(), Some(LoopEvent::Done)));
    }

    /// A declined check is the human's call, not a failure to fix.
    #[test]
    fn a_declined_check_ends_the_turn() {
        let (events, seen, commands) =
            checked_turn(vec![edit_round("e1"), say("done")], &[], |call| {
                if call.name == "bash" {
                    PermissionDecision::Deny { feedback: None }
                } else {
                    PermissionDecision::Approve
                }
            });
        assert!(commands.is_empty(), "denied: bash never ran");
        assert_eq!(checked(&events), vec![false]);
        assert_eq!(seen.len(), 2, "no bounce on a denial");
        assert!(matches!(events.last(), Some(LoopEvent::Done)));
    }

    /// #39 end to end: the same large file read twice; the request
    /// after the second read carries the first as a pointer, the
    /// second whole, and the loop reports what it recovered.
    #[test]
    fn a_reread_file_reaches_the_model_once() {
        let call = |id: &str| Event::ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: r#"{"path": "src/big.rs"}"#.into(),
        };
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![
                vec![call("r1"), Event::Done],
                vec![call("r2"), Event::Done],
                vec![
                    Event::Token {
                        delta: "read it".into(),
                    },
                    Event::Done,
                ],
            ]),
        });
        let body: &'static str = Box::leak(
            format!(
                "     1  fn big() {{}}\n{}",
                "x".repeat(crate::prune::BATCH_CHARS)
            )
            .into_boxed_str(),
        );
        let read: Arc<dyn Tool> = Arc::new(StubTool::new("read_file", false, body));
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (spec, broker) = turn(provider.clone(), registry(&[read]), history, "read big.rs");
        let events = drain(run_turn(spec), &broker, |_| panic!("reads do not prompt"));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LoopEvent::Pruned { reads: 1, .. })),
            "the loop says what it collapsed"
        );
        let seen = provider.seen.lock().unwrap();
        let last = seen.last().expect("three requests");
        let reads: Vec<&str> = last
            .messages
            .iter()
            .filter(|message| message.name.as_deref() == Some("read_file"))
            .filter_map(|message| message.content.as_deref())
            .collect();
        assert_eq!(reads.len(), 2);
        assert!(
            reads[0].starts_with("[read_file src/big.rs: this read succeeded"),
            "{}",
            &reads[0][..60]
        );
        assert_eq!(reads[1], body, "the latest read stays whole");
    }

    /// The compaction window: a huge model window is capped at the
    /// ceiling, a small one stands, 0 lifts the ceiling, and with no
    /// window the ceiling alone turns compaction on.
    #[test]
    fn a_large_window_compacts_at_the_ceiling() {
        let default_trigger = DEFAULT_COMPACT_AT_TOKENS * 100 / 80;
        assert_eq!(
            compaction_window(Some(1_046_576), None),
            Some(default_trigger)
        );
        assert_eq!(compaction_window(Some(128_000), None), Some(128_000));
        assert_eq!(compaction_window(Some(1_000_000), Some(0)), Some(1_000_000));
        assert_eq!(compaction_window(None, None), Some(default_trigger));
        assert_eq!(compaction_window(None, Some(0)), None);
        assert_eq!(
            compaction_window(Some(1_000_000), Some(80_000)),
            Some(100_000)
        );
        // The trigger is 80% of the window: 160k for the default.
        assert_eq!(default_trigger * 80 / 100, DEFAULT_COMPACT_AT_TOKENS);
        // The selection lands well under the trigger, in tokens.
        let window = compaction_window(Some(1_046_576), None).unwrap();
        let selected_tokens = compaction_budget(Some(window)) as u64 / 4;
        assert!(
            selected_tokens * 2 <= window * 80 / 100,
            "selection {selected_tokens} vs trigger {}",
            window * 80 / 100
        );
    }

    #[test]
    fn history_compacts_when_usage_crosses_the_window() {
        // A prior conversation sits in history; round 1 observes 90% of
        // a 100k window, the loop must replace the old turns with a
        // budgeted selection before the next request. No summarizer
        // round exists: two scripts drive the whole turn.
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![
                vec![
                    Event::Usage {
                        input_tokens: 90_000,
                        output_tokens: 10,
                        cached_input_tokens: 0,
                    },
                    Event::ToolCall {
                        id: "c1".into(),
                        name: "peek".into(),
                        arguments: "{}".into(),
                    },
                    Event::Done,
                ],
                vec![
                    Event::Token {
                        delta: "all done".into(),
                    },
                    Event::Done,
                ],
            ]),
        });
        let peek: Arc<dyn Tool> = Arc::new(StubTool::new("peek", false, "peeked"));
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        // The prior conversation: an original request and ten settled
        // turns, enough head for the selection to have something to
        // select.
        {
            let mut current = history.blocking_lock();
            current.push(WireMessage::user("the original request"));
            for i in 0..10 {
                current.push(WireMessage::assistant(format!("old turn {i}, did a thing")));
                current.push(WireMessage::user(format!("older reply {i}")));
            }
        }
        let (mut spec, broker) = turn(provider, registry(&[peek]), history.clone(), "go");
        spec.context_window = Some(100_000);
        let events = drain(run_turn(spec), &broker, |_| {
            panic!("read-only tools must not prompt")
        });

        // 1 original + 20 turns + "go" + assistant+call + tool result.
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LoopEvent::Compacted { total: 24, .. }))
        );
        let history = history.blocking_lock();
        let selection = history
            .iter()
            .find(|message| message.role == Role::System)
            .expect("compacted history opens with the selection");
        let selection = selection.content.as_deref().unwrap();
        assert!(
            selection.contains("selected from the record"),
            "the marker says what happened"
        );
        assert!(
            selection.contains("[user, the original request] the original request"),
            "the original request survives verbatim"
        );
        assert!(
            selection.contains("[tool] peeked") || selection.contains("old turn"),
            "omitted turns appear as prefix lines: {selection}"
        );
        assert!(history.iter().any(|message| message.role == Role::Assistant
            && message.content.as_deref() == Some("all done")));
    }

    #[test]
    fn below_the_window_history_is_untouched() {
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![vec![Event::Done]]),
        });
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (mut spec, broker) = turn(provider, registry(&[]), history.clone(), "go");
        spec.context_window = Some(100_000);
        let events = drain(run_turn(spec), &broker, |_| panic!("nothing to prompt for"));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, LoopEvent::Compacted { .. }))
        );
        assert!(
            !history
                .blocking_lock()
                .iter()
                .any(|message| message.role == Role::System)
        );
    }

    #[test]
    fn headless_without_a_broker_mutations_are_denied() {
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![
                vec![
                    Event::ToolCall {
                        id: "c1".into(),
                        name: "write_file".into(),
                        arguments: "{}".into(),
                    },
                    Event::Done,
                ],
                vec![
                    Event::Token {
                        delta: "noted".into(),
                    },
                    Event::Done,
                ],
            ]),
        });
        let writer: Arc<dyn Tool> = Arc::new(StubTool::new("write_file", true, "wrote"));
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (mut spec, broker) = turn(provider, registry(&[writer]), history.clone(), "go");
        spec.permission_broker = None;
        let events = drain(run_turn(spec), &broker, |_| {
            panic!("no ask should be outstanding")
        });

        // The model sees the denial, no silent no-op.
        assert!(tool_result(&events, "write_file").contains("no permission channel"));
    }

    #[test]
    fn system_prompt_frames_every_request_but_stays_out_of_history() {
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![vec![Event::Done]]),
        });
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (mut spec, broker) = turn(provider.clone(), registry(&[]), history.clone(), "go");
        spec.system_prompt = Some("You are gertie.".into());
        let _ = drain(run_turn(spec), &broker, |_| panic!("nothing to prompt for"));

        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen[0].messages.first().unwrap().role, Role::System);
        // The DB-bound history holds the conversation, not the framing:
        // no System message ever lands there.
        let history = history.blocking_lock();
        assert!(history.len() <= 2);
        assert!(history.iter().all(|message| message.role != Role::System));
        assert_eq!(history[0].role, Role::User);
    }

    #[test]
    fn session_instructions_frame_every_request_but_stay_out_of_history() {
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![
                vec![
                    Event::ToolCall {
                        id: "c1".into(),
                        name: "peek".into(),
                        arguments: "{}".into(),
                    },
                    Event::Done,
                ],
                vec![
                    Event::Token {
                        delta: "noted".into(),
                    },
                    Event::Done,
                ],
            ]),
        });
        let peeker: Arc<dyn Tool> = Arc::new(StubTool::new("peek", false, "peeked"));
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (mut spec, broker) = turn(provider.clone(), registry(&[peeker]), history.clone(), "go");
        spec.system_prompt = Some("You are gertie.".into());
        spec.instructions = Some("no em dashes".into());
        let _ = drain(run_turn(spec), &broker, |_| {
            panic!("a read-only tool must not ask")
        });

        // Every request carries the prompt, then the instructions,
        // then the conversation. Framing order is load-bearing: the
        // instructions sit after the static prefix so caching survives.
        let seen = provider.seen.lock().unwrap();
        assert!(seen.len() >= 2, "a tool round means two requests");
        for request in seen.iter() {
            assert_eq!(request.messages[0].role, Role::System);
            assert_eq!(request.messages[1].role, Role::User);
            let text = request.messages[1].content.as_deref().unwrap();
            assert!(text.starts_with("[session instructions]\n"));
            assert!(text.contains("no em dashes"));
            assert!(text.ends_with("\n[end session instructions]"));
        }
        // The record holds the conversation, not the framing.
        let history = history.blocking_lock();
        assert!(history.iter().all(|message| {
            !message
                .content
                .as_deref()
                .unwrap_or("")
                .contains("[session instructions]")
        }));
    }

    #[test]
    fn without_instructions_nothing_is_injected() {
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![vec![Event::Done]]),
        });
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (mut spec, broker) = turn(provider.clone(), registry(&[]), history.clone(), "go");
        spec.system_prompt = Some("You are gertie.".into());
        let _ = drain(run_turn(spec), &broker, |_| panic!("nothing to prompt for"));

        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen[0].messages[0].role, Role::System);
        assert_eq!(seen[0].messages[1].role, Role::User);
        assert_ne!(
            seen[0].messages[1].content.as_deref().unwrap(),
            "[session instructions]"
        );
        assert!(seen[0].messages.iter().all(|message| {
            !message
                .content
                .as_deref()
                .unwrap_or("")
                .starts_with("[session instructions]")
        }));
    }

    #[test]
    fn the_memory_index_frames_alone_when_instructions_are_absent() {
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![vec![Event::Done]]),
        });
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let (mut spec, broker) = turn(provider.clone(), registry(&[]), history.clone(), "go");
        spec.system_prompt = Some("You are gertie.".into());
        spec.memory_index = Some("- [p1] helpful=0 harmful=0 tabs over spaces".into());
        let _ = drain(run_turn(spec), &broker, |_| panic!("nothing to prompt for"));

        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen[0].messages[0].role, Role::System);
        // The index slots directly after the prompt when no
        // instructions sit between them.
        assert_eq!(seen[0].messages[1].role, Role::User);
        let index = seen[0].messages[1].content.as_deref().unwrap();
        assert!(index.starts_with("[memory index"));
        assert!(index.contains("tabs over spaces"));
    }

    /// #41: the repo map closes the framing block, after the memory
    /// index, and is the same bytes on every round of the turn.
    #[test]
    fn the_repo_map_frames_after_the_index_and_holds_across_rounds() {
        struct Fixed;
        impl crate::repo_map::RepoMapSource for Fixed {
            fn render(&self, hints: &crate::repo_map::MapHints, _: usize) -> Option<String> {
                Some(format!(
                    "src/pool.rs: struct Pool (asked: {})",
                    hints.said.trim()
                ))
            }
        }
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(one_call_then_text("peek")),
        });
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let peek: Arc<dyn Tool> = Arc::new(StubTool::new("peek", false, "peeked"));
        let (mut spec, broker) = turn(
            provider.clone(),
            registry(&[peek]),
            history.clone(),
            "fix the pool",
        );
        spec.system_prompt = Some("You are gertie.".into());
        spec.memory_index = Some("- [p1] helpful=0 harmful=0 tabs".into());
        let map = crate::repo_map::SessionMap::new(Arc::new(Fixed), 500);
        spec.repo_map = Some(map.clone());
        let _ = drain(run_turn(spec), &broker, |_| panic!("nothing to prompt for"));

        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "a tool round, then the wrap-up");
        for request in seen.iter() {
            let framing = request.messages[2].content.as_deref().unwrap();
            assert!(framing.starts_with("[repo map"), "{framing}");
            assert!(framing.contains("src/pool.rs: struct Pool (asked: fix the pool)"));
        }
        assert_eq!(seen[0].messages[2].content, seen[1].messages[2].content);
        assert!(map.is_frozen(), "the next turn reuses it");
        // Framing, never record: history holds none of it.
        assert!(history.blocking_lock().iter().all(|message| {
            !message
                .content
                .as_deref()
                .unwrap_or("")
                .contains("[repo map")
        }));
    }

    #[test]
    fn safe_tail_never_opens_on_an_orphan_tool_result() {
        let history = vec![
            WireMessage::user("go"),
            WireMessage::assistant_with_tool_calls(
                None,
                vec![ToolCall {
                    id: "c1".into(),
                    name: "peek".into(),
                    arguments: "{}".into(),
                }],
            ),
            WireMessage::tool_result(
                &ToolCall {
                    id: "c1".into(),
                    name: "peek".into(),
                    arguments: "{}".into(),
                },
                "peeked",
            ),
        ];
        // A tail of 2 would start on the orphan result; it must slide
        // forward to include the call that produced it.
        assert_eq!(safe_tail_start(&history, 2), 1);
        assert_eq!(safe_tail_start(&history, 3), 0);
    }

    #[test]
    fn digest_lines_are_verbatim_prefixes_in_conversation_order() {
        let head = vec![
            WireMessage::user("goal: refactor the thing"),
            WireMessage::assistant("working on it"),
            WireMessage::user("use the staging dir for outputs"),
        ];
        let digest = select_digest(&head, 100_000);
        let lines: Vec<&str> = digest.lines().collect();
        assert_eq!(
            lines[0],
            "[user, the original request] goal: refactor the thing"
        );
        assert_eq!(lines[1], "[assistant] working on it");
        assert_eq!(lines[2], "[user] use the staging dir for outputs");
    }

    #[test]
    fn the_original_request_survives_regardless_of_budget() {
        let head = vec![
            WireMessage::user("the original request is long and matters"),
            WireMessage::assistant("yes"),
            WireMessage::user("go on"),
        ];
        // A budget too small for anything else still keeps the request.
        let digest = select_digest(&head, 10);
        assert!(digest.contains("the original request is long"));
        assert!(!digest.contains("go on"), "budget ran out before it");
    }

    #[test]
    fn over_budget_selection_prefers_the_recent_middle() {
        let mut head = vec![WireMessage::user("the original request")];
        for i in 0..400 {
            head.push(WireMessage::assistant(format!(
                "ancient turn {i}, {}",
                "x".repeat(ITEM_PREFIX)
            )));
        }
        head.push(WireMessage::assistant("the newest middle turn"));
        // Generous but bounded: not everything fits.
        let digest = select_digest(&head, 8_000);
        assert!(digest.contains("the newest middle turn"));
        assert!(
            !digest.contains("ancient turn 0 "),
            "the oldest middle is what gets dropped: {}",
            &digest[..200]
        );
    }

    #[test]
    fn compaction_does_nothing_when_the_tail_covers_everything() {
        let mut current = vec![WireMessage::user("go"), WireMessage::assistant("done")];
        assert_eq!(compact_in_place(&mut current, 10_000), None);
        assert_eq!(current.len(), 2, "history untouched");
    }

    #[test]
    fn compaction_selects_and_reports_kept_and_total() {
        let mut current = vec![WireMessage::user("the original request")];
        for i in 0..20 {
            current.push(WireMessage::assistant(format!("old turn {i}")));
        }
        current.push(WireMessage::assistant("fresh work"));
        let (kept, total) =
            compact_in_place(&mut current, 100_000).expect("there is a head to select");
        assert_eq!(total, 22);
        assert_eq!(kept, 1 + KEEP_TAIL, "the digest replaces the head only");
        assert_eq!(current[0].role, Role::System);
        let digest = current[0].content.as_deref().unwrap();
        assert!(digest.contains("selected from the record"));
        assert!(digest.contains("the original request"));
        // The tail is verbatim and still last.
        assert_eq!(
            current.last().unwrap().content.as_deref(),
            Some("fresh work")
        );
    }

    #[test]
    fn selection_budget_holds_back_a_reserve() {
        // A quarter of the window is reserved; bytes/4 converts back.
        assert_eq!(selection_budget(Some(100_000)), (100_000 - 25_000) * 4);
        // Small windows keep the 16k reserve but never select below 4k.
        assert_eq!(
            selection_budget(Some(20_000)),
            (20_000 - 16_000) as usize * 4
        );
        assert_eq!(
            selection_budget(Some(17_000)),
            (MIN_SELECTION_TOKENS * 4) as usize
        );
        // No window: the assumed-capacity fallback.
        assert_eq!(
            selection_budget(None),
            ((ASSUMED_WINDOW_TOKENS - ASSUMED_WINDOW_TOKENS / 4) * 4) as usize
        );
    }

    #[test]
    fn prefixes_never_tear_a_multibyte_character() {
        let text = "héllo wörld, ünïcode ✓";
        // A byte slice would panic or corrupt at these cut points; a
        // char slice returns a valid prefix every time.
        for n in 0..=text.chars().count() {
            let cut = prefix(text, n);
            assert!(text.starts_with(cut), "prefix {n} stays a prefix");
            assert!(cut.chars().count() <= n);
        }
        assert_eq!(prefix(text, text.chars().count()), text);
    }

    /// Mutating, so it would prompt if the vet were consulted in the
    /// wrong order.
    struct BlockedTool;

    impl Tool for BlockedTool {
        fn name(&self) -> &'static str {
            "bash"
        }

        fn description(&self) -> String {
            "stub".into()
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        fn mutates(&self) -> bool {
            true
        }

        fn vet(&self, _arguments: &str) -> Result<(), String> {
            Err("always blocked".into())
        }

        fn run<'a>(
            &'a self,
            _arguments: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>> {
            Box::pin(async { Ok("ran".into()) })
        }
    }

    #[test]
    fn a_stopped_round_gets_its_unanswered_calls_closed() {
        let call = |id: &str| ToolCall {
            id: id.to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
        };
        let mut history = vec![
            WireMessage::user("go"),
            WireMessage::assistant_with_tool_calls(None, vec![call("a"), call("b")]),
            WireMessage::tool_result(&call("a"), "done"),
        ];
        assert_eq!(close_unanswered_calls(&mut history, "stopped"), 1);
        assert_eq!(history.len(), 4);
        assert_eq!(history[3].tool_call_id.as_deref(), Some("b"));
        assert_eq!(history[3].content.as_deref(), Some("stopped"));
        // Nothing left to answer: a second pass is a no-op.
        assert_eq!(close_unanswered_calls(&mut history, "stopped"), 0);
        // A history with no calls is left alone.
        let mut plain = vec![WireMessage::user("hi"), WireMessage::assistant("hello")];
        assert_eq!(close_unanswered_calls(&mut plain, "stopped"), 0);
        assert_eq!(plain.len(), 2);
    }
}
