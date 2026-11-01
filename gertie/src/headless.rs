//! `gertie run`: one turn, no window. Model text goes to stdout (clean
//! for pipes), every activity line to stderr, and the session persists
//! exactly like a GUI session, the librarian and search_history see it
//! later. Approval flow mirrors the UI's safety model: reads auto-run;
//! mutating calls print the tool and arguments and wait for Enter/y,
//! or deny outright when there is no terminal and no `--yes`.

use gertie_core::{
    LoopEvent, MessageId, PermissionBroker, PermissionDecision, Role, SessionId, SessionStore,
};
use std::collections::HashMap;
use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll, Waker};

pub fn run(
    prompt: Option<String>,
    yes: bool,
    config_path: Option<PathBuf>,
    ephemeral: bool,
) -> i32 {
    let boot = crate::bootstrap::Bootstrap::load(config_path.as_deref(), ephemeral);
    if let Some(error) = boot.config_error {
        eprintln!("error: {error}");
        return 1;
    }
    let Some(provider) = boot.provider() else {
        eprintln!(
            "error: no endpoint configured, add one to config.toml (see config.example.toml)"
        );
        return 1;
    };

    // A positional prompt wins; otherwise the prompt arrives piped on
    // stdin. A bare TTY with neither is a usage error, not a hang.
    let prompt = match prompt {
        Some(text) => text,
        None => {
            if std::io::stdin().is_terminal() {
                eprintln!(
                    "error: `gertie run` needs a prompt argument, or a prompt piped on stdin"
                );
                return 2;
            }
            let mut text = String::new();
            use std::io::Read as _;
            if let Err(error) = std::io::stdin().read_to_string(&mut text) {
                eprintln!("error: could not read prompt from stdin: {error}");
                return 1;
            }
            text
        }
    };
    // Same entrance rule as the app: invisible characters never reach
    // a prompt, so a headless run reads exactly what the UI would send.
    let prompt = gertie_core::strip_invisible(prompt.trim());
    if prompt.is_empty() {
        eprintln!("error: empty prompt");
        return 2;
    }

    eprintln!("· {}", boot.label());
    for warning in boot.config.key_warnings() {
        eprintln!("⚠ {warning}");
    }
    // A key that didn't resolve means a guaranteed 401, refuse to send
    // the doomed request; the warning above carries the fix.
    if boot
        .config
        .selected_endpoint()
        .is_some_and(|endpoint| endpoint.api_key.is_none())
    {
        return 1;
    }
    let context_window = provider.context_window();
    let provider: Arc<dyn gertie_core::Provider> = provider;

    // Same title rule as the GUI: the first user words name the session.
    let title: String = prompt.chars().take(48).collect();
    let session = match boot.store.create_session(&title) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("error: could not create session: {error}");
            return 1;
        }
    };
    if let Err(error) = boot
        .store
        .append_message(session, None, Role::User, &prompt)
    {
        eprintln!("error: could not persist user message: {error}");
        return 1;
    }
    // Subagent dispatch reads this to link its runs to this chat.
    boot.session_scope.set(Some(session));

    let history = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let stream = gertie_core::run_turn(gertie_core::TurnSpec {
        provider,
        tools: boot.tools.clone(),
        history,
        user_message: Some(prompt),
        snapshots: boot.snapshots.clone(),
        checkpoint_session: Some(session),
        workspace_root: Some(boot.workspace_root.clone()),
        context_window,
        permission_broker: Some(boot.broker.clone()),
        permission_mode: boot.default_mode,
        sandbox_bash: boot.sandbox_bash,
        prompt_source: gertie_core::PromptSource::Leader,
        system_prompt: boot.system_prompt.clone(),
        instructions: boot.instructions.clone(),
        memory_index: boot.memory_index.clone(),
        // One-shot prompt mode: no steering, nothing queues.
        steer: None,
        user_attachments: Vec::new(),
        max_tool_rounds: boot.max_tool_rounds(),
        grace_rounds: boot.grace_rounds(),
        worker_progress: gertie_core::WorkerProgress::default(),
    });
    let worker_asks = boot.broker.subscribe();

    let interactive = std::io::stdin().is_terminal();
    let mut decide = |_call: &gertie_core::ToolCall| -> PermissionDecision {
        if yes {
            return PermissionDecision::Approve;
        }
        if !interactive {
            eprintln!("  denied (non-interactive; use --yes to auto-approve)");
            return PermissionDecision::Deny { feedback: None };
        }
        eprint!("  approve? [Enter/y = yes, n = no, anything else is a reply to the model] ");
        use std::io::Write as _;
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(_) => match line.trim() {
                "" | "y" | "Y" | "yes" => PermissionDecision::Approve,
                "n" | "N" | "no" => {
                    eprintln!("  denied");
                    PermissionDecision::Deny { feedback: None }
                }
                // Anything else the human typed is worth more than a
                // bare no: it goes back to the model in the tool result.
                reply => {
                    eprintln!("  denied, your reply goes back to the model");
                    PermissionDecision::Deny {
                        feedback: Some(reply.to_string()),
                    }
                }
            },
            Err(_) => {
                eprintln!("  denied (stdin closed)");
                PermissionDecision::Deny { feedback: None }
            }
        }
    };

    let (outcome, _) = pump(
        stream,
        worker_asks,
        &boot.broker,
        &boot.store,
        session,
        &mut decide,
    );
    match outcome {
        Outcome::Done => 0,
        Outcome::Error { message } => {
            eprintln!("[error] {message}");
            1
        }
        Outcome::Limited { message } => {
            eprintln!("[limited] {message}");
            1
        }
        Outcome::Budgeted { message } => {
            eprintln!("[paused] {message}");
            1
        }
    }
}

#[derive(Debug)]
pub(crate) enum Outcome {
    /// The turn finished.
    Done,
    /// The loop surfaced an exhausted error.
    Error { message: String },
    /// The loop ended on the provider's rate limit. The transcript and
    /// the exit say so distinctly, an error asks "what broke", a limit
    /// asks "when do we go again".
    Limited { message: String },
    /// The round budget's grace window closed, the turn paused like a
    /// limited one: resumable in a session, exit 1 headless.
    Budgeted { message: String },
}

/// Drives one turn to completion on the current thread. Returns the
/// outcome plus everything the model said in total. The provider
/// drains on its own runtime; polling here only receives, so `Pending`
/// means "still producing", never deadlock, the same contract the
/// provider tests rely on.
pub(crate) fn pump(
    mut stream: futures_util::stream::BoxStream<'static, LoopEvent>,
    mut worker_asks: tokio::sync::mpsc::UnboundedReceiver<gertie_core::PendingAsk>,
    broker: &Arc<PermissionBroker>,
    store: &Arc<SessionStore>,
    session: SessionId,
    decide: &mut dyn FnMut(&gertie_core::ToolCall) -> PermissionDecision,
) -> (Outcome, String) {
    // Assistant text accumulates until a shape change, then lands as one
    // row, the headless equivalent of the UI's throttled persistence.
    // The recorder parents onto the session's tip, the way the real
    // front does: whatever the session already held is the
    // conversation this turn continues.
    let mut recorder = Recorder {
        store: store.clone(),
        session,
        last_db_id: store
            .load_recent_messages(session, 1)
            .ok()
            .and_then(|rows| rows.last().map(|row| row.id)),
        pending: String::new(),
        text: String::new(),
    };
    // call id → (name, arguments), so tool-result rows carry what the
    // model actually sent, the reload path depends on it.
    let mut calls: HashMap<String, (String, String)> = HashMap::new();
    let waker = Waker::noop();
    let mut cx = TaskContext::from_waker(waker);

    loop {
        // Worker asks first: a worker's loop parks inside its tool, so
        // its asks only surface here, never on the leader stream. An
        // empty or disconnected channel just ends the drain, no more
        // asks for now.
        while let Ok(ask) = worker_asks.try_recv() {
            recorder.flush();
            calls.insert(
                ask.call.id.clone(),
                (ask.call.name.clone(), ask.call.arguments.clone()),
            );
            let decision = ask_permission(&ask.call, true, decide);
            let _ = broker.respond(ask.id, decision);
        }

        match stream.as_mut().poll_next(&mut cx) {
            Poll::Ready(Some(event)) => match event {
                LoopEvent::Delta { delta } => {
                    use std::io::Write as _;
                    print!("{delta}");
                    let _ = std::io::stdout().flush();
                    recorder.pending.push_str(&delta);
                    recorder.text.push_str(&delta);
                }
                LoopEvent::ToolCallRequested { id, call } => {
                    recorder.flush();
                    calls.insert(call.id.clone(), (call.name.clone(), call.arguments.clone()));
                    let decision = ask_permission(&call, false, decide);
                    // A stale id (already answered) is harmless either way.
                    let _ = broker.respond(id, decision);
                }
                LoopEvent::ToolCallStarted { call } => {
                    calls.insert(call.id.clone(), (call.name.clone(), call.arguments.clone()));
                    eprintln!("· running {}…", call.name);
                }
                LoopEvent::ToolResult {
                    call_id,
                    name,
                    arguments: _,
                    result,
                } => {
                    let arguments = calls
                        .remove(&call_id)
                        .map(|(_, arguments)| arguments)
                        .unwrap_or_else(|| "{}".into());
                    eprintln!("· {} → {}", name, brief(&result));
                    if let Ok(db_id) = store.append_tool_result(
                        session,
                        recorder.last_db_id,
                        &call_id,
                        &name,
                        &arguments,
                        &result,
                    ) {
                        recorder.last_db_id = Some(db_id);
                    }
                }
                LoopEvent::Usage {
                    input_tokens,
                    output_tokens,
                    cached_input_tokens,
                } => {
                    // Persist against the session so its totals survive
                    // the run, same ledger the GUI reads on resume.
                    let _ =
                        store.add_usage(session, input_tokens, output_tokens, cached_input_tokens);
                    eprintln!("· tokens, {input_tokens} in / {output_tokens} out (this request)");
                }
                LoopEvent::Compacted { kept, total } => {
                    eprintln!("· context compacted, {kept} of {total} messages kept");
                }
                // One-shot prompt mode cannot steer, no stdin to speak
                // of, so the event cannot fire; the arm is completeness.
                LoopEvent::Steered { .. } => {}
                LoopEvent::Nudged => {}
                LoopEvent::Error { message } => {
                    recorder.flush();
                    return (Outcome::Error { message }, recorder.text);
                }
                LoopEvent::Limited { message } => {
                    recorder.flush();
                    return (Outcome::Limited { message }, recorder.text);
                }
                LoopEvent::Budgeted { message } => {
                    recorder.flush();
                    return (Outcome::Budgeted { message }, recorder.text);
                }
                LoopEvent::Done => {
                    recorder.flush();
                    println!();
                    return (Outcome::Done, recorder.text);
                }
            },
            Poll::Ready(None) => {
                recorder.flush();
                println!();
                return (Outcome::Done, recorder.text);
            }
            Poll::Pending => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
}

/// Prints the ask and applies the decider. Worker asks are labeled,
/// same information the UI's `· worker` card carries.
fn ask_permission(
    call: &gertie_core::ToolCall,
    worker: bool,
    decide: &mut dyn FnMut(&gertie_core::ToolCall) -> PermissionDecision,
) -> PermissionDecision {
    let who = if worker {
        "worker approval needed"
    } else {
        "approval needed"
    };
    eprintln!("· {who}, {}", call.name);
    eprintln!("  {}", describe(call));
    decide(call)
}

/// One line summarizing a tool result for stderr.
fn brief(result: &str) -> String {
    let first = result.lines().next().unwrap_or("").trim();
    truncate(first, 120)
}

/// What the human sees before deciding: the command for bash, the path
/// for file tools, the raw arguments otherwise.
fn describe(call: &gertie_core::ToolCall) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_str(&call.arguments).ok();
    let summary = match &parsed {
        Some(value) => {
            let command = value.get("command").and_then(|v| v.as_str());
            let path = value.get("path").and_then(|v| v.as_str());
            match (command, path) {
                (Some(command), _) => command.to_string(),
                (None, Some(path)) => path.to_string(),
                (None, None) => call.arguments.clone(),
            }
        }
        None => call.arguments.clone(),
    };
    truncate(&summary, 300)
}

fn truncate(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        text.to_string()
    } else {
        let head: String = text.chars().take(cap).collect();
        format!("{head}…")
    }
}

/// Persists the transcript the way the GUI does: assistant text lands as
/// a row per segment, tool results chained after it, so `load_wire_history`
/// and the next turn see the same record the model did.
struct Recorder {
    store: Arc<SessionStore>,
    session: SessionId,
    last_db_id: Option<MessageId>,
    pending: String,
    /// Everything the model said in total, for the caller's summary.
    text: String,
}

impl Recorder {
    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.pending);
        if let Ok(db_id) =
            self.store
                .append_message(self.session, self.last_db_id, Role::Assistant, &text)
        {
            self.last_db_id = Some(db_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gertie_core::{Event, Tool, ToolRegistry, ToolResult, TurnSpec, run_turn};
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;

    /// A provider with a queue of scripted responses, one per request.
    struct Scripted(std::sync::Mutex<VecDeque<Vec<Event>>>);

    impl gertie_core::Provider for Scripted {
        fn stream(
            &self,
            _request: gertie_core::Request,
        ) -> futures_util::stream::BoxStream<'static, Event> {
            let events = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| vec![Event::Done]);
            Box::pin(futures_util::stream::iter(events))
        }
    }

    /// A mutating tool that does nothing, enough to trigger the
    /// permission path.
    struct YesTool;

    impl Tool for YesTool {
        fn name(&self) -> &'static str {
            "fake_write"
        }
        fn description(&self) -> String {
            "test tool".into()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn mutates(&self) -> bool {
            true
        }
        fn run<'a>(
            &'a self,
            _arguments: &'a str,
        ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
            Box::pin(async { Ok("did nothing, honestly".into()) })
        }
    }

    fn fixture() -> (Arc<SessionStore>, gertie_core::SessionId) {
        let path = std::env::temp_dir().join(format!(
            "gertie-headless-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        let store = Arc::new(SessionStore::open(&path.join("sessions.db")).unwrap());
        let session = store.create_session("test").unwrap();
        (store, session)
    }

    #[test]
    fn an_approved_mutation_persists_the_whole_transcript() {
        let (store, session) = fixture();
        let provider = Arc::new(Scripted(std::sync::Mutex::new(VecDeque::from(vec![
            vec![
                Event::Started,
                Event::Token {
                    delta: "writing…".into(),
                },
                Event::ToolCall {
                    id: "t1".into(),
                    name: "fake_write".into(),
                    arguments: r#"{"path":"x.txt"}"#.into(),
                },
                Event::Done,
            ],
            vec![
                Event::Started,
                Event::Token {
                    delta: "all done".into(),
                },
                Event::Done,
            ],
        ]))));
        let mut tools: ToolRegistry = Arc::new(Default::default());
        Arc::get_mut(&mut tools)
            .unwrap()
            .insert("fake_write".into(), Arc::new(YesTool));
        let broker = Arc::new(PermissionBroker::new());
        store
            .append_message(session, None, Role::User, "do it")
            .unwrap();
        let stream = run_turn(TurnSpec {
            provider,
            tools,
            history: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            user_message: Some("do it".into()),
            snapshots: None,
            checkpoint_session: None,
            workspace_root: None,
            context_window: None,
            permission_broker: Some(broker.clone()),
            permission_mode: gertie_core::PermissionMode::Supervised,
            sandbox_bash: false,
            prompt_source: gertie_core::PromptSource::Leader,
            system_prompt: None,
            instructions: None,
            memory_index: None,
            steer: None,
            user_attachments: Vec::new(),
            max_tool_rounds: None,
            grace_rounds: None,
            worker_progress: gertie_core::WorkerProgress::default(),
        });
        let mut approvals = 0;
        let (outcome, text) = pump(
            stream,
            broker.subscribe(),
            &broker,
            &store,
            session,
            &mut |call| {
                approvals += 1;
                assert_eq!(call.name, "fake_write");
                PermissionDecision::Approve
            },
        );
        let Outcome::Done = outcome else {
            panic!("expected Done, got {outcome:?}");
        };
        assert_eq!(text, "writing…all done");
        assert_eq!(approvals, 1);

        // The persisted record: user, assistant segment, tool result,
        // final assistant, a reload would reconstruct exactly this.
        let messages = store.load_messages(session).unwrap();
        let rendered: Vec<String> = messages
            .iter()
            .map(|message| format!("{:?}: {}", message.role, message.content))
            .collect();
        assert_eq!(
            rendered,
            vec![
                "User: do it",
                "Assistant: writing…",
                "Tool: did nothing, honestly",
                "Assistant: all done",
            ]
        );
    }

    #[test]
    fn a_denied_mutation_reaches_the_model_as_a_deny() {
        let (store, session) = fixture();
        let provider = Arc::new(Scripted(std::sync::Mutex::new(VecDeque::from(vec![
            vec![
                Event::ToolCall {
                    id: "t1".into(),
                    name: "fake_write".into(),
                    arguments: "{}".into(),
                },
                Event::Done,
            ],
            vec![
                Event::Token {
                    delta: "understood".into(),
                },
                Event::Done,
            ],
        ]))));
        let mut tools: ToolRegistry = Arc::new(Default::default());
        Arc::get_mut(&mut tools)
            .unwrap()
            .insert("fake_write".into(), Arc::new(YesTool));
        let broker = Arc::new(PermissionBroker::new());
        let stream = run_turn(TurnSpec {
            provider,
            tools,
            history: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            user_message: Some("do it".into()),
            snapshots: None,
            checkpoint_session: None,
            workspace_root: None,
            context_window: None,
            permission_broker: Some(broker.clone()),
            permission_mode: gertie_core::PermissionMode::Supervised,
            sandbox_bash: false,
            prompt_source: gertie_core::PromptSource::Leader,
            system_prompt: None,
            instructions: None,
            memory_index: None,
            steer: None,
            user_attachments: Vec::new(),
            max_tool_rounds: None,
            grace_rounds: None,
            worker_progress: gertie_core::WorkerProgress::default(),
        });
        let mut approvals = 0;
        let (outcome, _) = pump(
            stream,
            broker.subscribe(),
            &broker,
            &store,
            session,
            &mut |_| {
                approvals += 1;
                PermissionDecision::Deny { feedback: None }
            },
        );
        assert!(matches!(outcome, Outcome::Done));
        assert_eq!(approvals, 1);
        let messages = store.load_messages(session).unwrap();
        assert!(messages.iter().any(
            |message| message.role == Role::Assistant && message.content.contains("understood")
        ));
    }
}
