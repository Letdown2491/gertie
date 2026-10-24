//! The agent loop: request the model, execute its tool calls under the
//! permission policy, feed results back, repeat — bounded.

use crate::Role;
use crate::request::{Request, ToolCall, ToolSpec, WireMessage};
use crate::snapshot::SnapshotStore;
use crate::tool::Tool;
use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use std::sync::Arc;

/// Safety valve: a turn that loops this many tool rounds without finishing
/// is cut off — runaway loops are how token bills happen.
const MAX_TOOL_ROUNDS: usize = 25;

/// What the UI (or any embedder) observes while a turn runs.
pub enum LoopEvent {
    /// Assistant text arrived.
    Delta {
        delta: String,
    },
    /// A mutating tool wants to run. The ask is registered with the
    /// broker under `id` — answer through `PermissionBroker::respond`;
    /// the loop waits until answered (a dropped answer counts as a deny
    /// — if the UI went away, doing nothing is the safe move).
    ToolCallRequested {
        id: u64,
        call: ToolCall,
    },
    /// A tool finished (approved, denied, or auto-run) — this is what gets
    /// persisted and shown as the card's outcome.
    ToolResult {
        call_id: String,
        name: String,
        result: String,
    },
    /// A read-only tool started executing (mutating calls surface through
    /// `ToolCallRequested` instead). Exists so the UI can show activity
    /// for the tools that never prompt — silent reads read as a hang.
    ToolCallStarted {
        call: ToolCall,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// History was compacted in place: `kept` of `total` messages remain
    /// (a summary plus a safe tail). DB rows are not rewritten — the
    /// compaction is the in-memory view.
    Compacted {
        kept: usize,
        total: usize,
    },
    Error {
        message: String,
    },
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Approve,
    Deny,
}

/// The conversation history, shared between the loop and the caller so the
/// caller can keep sending turns without reconstructing anything.
pub type SharedHistory = Arc<tokio::sync::Mutex<Vec<WireMessage>>>;

pub type ToolRegistry = Arc<BTreeMap<String, Arc<dyn Tool>>>;

/// Builds the ToolSpec list advertised to the model.
pub fn tool_specs(tools: &ToolRegistry) -> Vec<ToolSpec> {
    tools
        .values()
        .map(|tool| ToolSpec {
            name: tool.name().to_string(),
            description: tool.description(),
            parameters: tool.parameters(),
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
    pub user_message: String,
    pub snapshots: Option<Arc<SnapshotStore>>,
    pub context_window: Option<u64>,
    /// Shared approval registry. When set, every ask — this loop's or a
    /// sub-loop's — registers here so one UI can answer them all. None
    /// means nobody can approve: mutations are denied, doing nothing is
    /// the safe move.
    pub permission_broker: Option<Arc<crate::PermissionBroker>>,
    /// Sent as the first message of every request, never stored in
    /// history: it is per-turn framing, not conversation record (the
    /// DB stays canonical; the next turn may have a different index).
    pub system_prompt: Option<String>,
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
/// `path` gets a checkpoint of the file's prior content first —
/// record-before-write, uniformly, including tools added later.
///
/// When `context_window` is set, the loop compacts: once a round's
/// observed prompt size crosses 80% of the window, history is replaced
/// by a structured summary plus a safe tail before the next request.
pub fn run_turn(spec: TurnSpec) -> BoxStream<'static, LoopEvent> {
    let TurnSpec {
        provider,
        tools,
        history,
        user_message,
        snapshots,
        context_window,
        permission_broker,
        system_prompt,
    } = spec;
    let specs = tool_specs(&tools);
    Box::pin(async_stream::stream! {
        let mut request = Request {
            tools: specs,
            ..Request::default()
        };

        history.lock().await.push(WireMessage::user(user_message));

        // Observed prompt size from the last request's usage events, if
        // the provider reported any; the chars/4 estimate covers the rest.
        let mut last_input_tokens: Option<u64> = None;

        for round in 1..=MAX_TOOL_ROUNDS {
            // Compaction check happens at round start, before the
            // request is built — the point is to shrink what we're
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
                    let transcript = render_transcript(&history.lock().await);
                    let total = history.lock().await.len();
                    // Fail-open: if the summarization request fails, this
                    // round just runs long — compaction is an
                    // optimization, not a correctness step.
                    if let Ok(summary) = summarize(provider.clone(), &transcript).await {
                        let mut current = history.lock().await;
                        let tail_start = safe_tail_start(&current, KEEP_TAIL);
                        let tail: Vec<WireMessage> = current.drain(tail_start..).collect();
                        *current = vec![summary_message(&summary)];
                        current.extend(tail);
                        let kept = current.len();
                        drop(current);
                        // The old observation describes the pre-compact
                        // prompt; don't let it re-trigger.
                        last_input_tokens = None;
                        yield LoopEvent::Compacted { kept, total };
                    }
                }
            }
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

            let mut stream = provider.stream(request.clone());
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
                    crate::Event::Usage { input_tokens, output_tokens } => {
                        last_input_tokens = Some(input_tokens);
                        yield LoopEvent::Usage { input_tokens, output_tokens };
                    }
                    crate::Event::Done => {}
                    crate::Event::Error { message } => {
                        yield LoopEvent::Error { message };
                        return;
                    }
                }
            }

            if calls.is_empty() {
                history
                    .lock()
                    .await
                    .push(WireMessage::assistant(assistant_text));
                yield LoopEvent::Done;
                return;
            }

            history
                .lock()
                .await
                .push(WireMessage::assistant_with_tool_calls(
                    if assistant_text.is_empty() { None } else { Some(assistant_text) },
                    calls.clone(),
                ));

            for call in calls {
                let result = match tools.get(&call.name) {
                    None => Err(format!("unknown tool: {}", call.name)),
                    Some(tool) => {
                        // Hard policy first: a vet failure never reaches
                        // the prompt — there is nothing to approve.
                        if let Err(blocked) = tool.vet(&call.arguments) {
                            Err(format!("blocked by policy: {blocked}"))
                        }
                        // Mutating tools face the permission prompt; the
                        // ask registers with the broker (so the UI can
                        // answer it) and the loop awaits the answer.
                        else if tool.mutates() {
                            match &permission_broker {
                                Some(broker) => {
                                    let (request_id, answer) =
                                        broker.request(call.clone(), crate::PromptSource::Leader);
                                    yield LoopEvent::ToolCallRequested {
                                        id: request_id,
                                        call: call.clone(),
                                    };
                                    match answer.await {
                                        Ok(PermissionDecision::Approve) => {
                                            // Checkpoint the file's prior
                                            // state before anything
                                            // mutates it.
                                            if let (Some(store), Some(path)) =
                                                (&snapshots, argument_path(&call.arguments))
                                                && let Err(error) = store.record_file(None, &path)
                                            {
                                                yield LoopEvent::Error {
                                                    message: format!(
                                                        "checkpoint before {} failed: {error}",
                                                        call.name
                                                    ),
                                                };
                                                return;
                                            }
                                            tool.run(&call.arguments).await
                                        }
                                        // A dropped responder (UI went
                                        // away) denies too: doing
                                        // nothing is safe.
                                        Ok(PermissionDecision::Deny) | Err(_) => {
                                            Err("denied by user".to_string())
                                        }
                                    }
                                }
                                None => Err("no permission channel available — denied".to_string()),
                            }
                        } else {
                            yield LoopEvent::ToolCallStarted { call: call.clone() };
                            tool.run(&call.arguments).await
                        }
                    }
                };
                let result = match result {
                    Ok(text) => text,
                    Err(text) => format!("error: {text}"),
                };

                history
                    .lock()
                    .await
                    .push(WireMessage::tool_result(&call, &result));
                yield LoopEvent::ToolResult {
                    call_id: call.id,
                    name: call.name,
                    result,
                };
            }

            if round == MAX_TOOL_ROUNDS {
                yield LoopEvent::Error {
                    message: format!("stopped after {MAX_TOOL_ROUNDS} tool rounds"),
                };
                return;
            }
        }
    })
}

/// Messages kept verbatim after a compaction, roughly two tool rounds.
const KEEP_TAIL: usize = 8;

/// Structured-summary marker: the replacement history opens with this
/// system message, and later compactions summarize it like anything else.
fn summary_message(summary: &str) -> WireMessage {
    WireMessage {
        role: Role::System,
        content: Some(format!(
            "### Conversation summary\n\n\
             {summary}\n\n\
             This summary replaced earlier history; continue the task from NEXT."
        )),
        ..WireMessage::default()
    }
}

/// chars/4: the boring token estimate, used only when the provider
/// hasn't reported usage yet.
fn estimate_tokens(history: &[WireMessage]) -> u64 {
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

/// Flattens history to text for the summarizer — the summarizer sees the
/// conversation the way the transcript would read, tool calls included.
fn render_transcript(history: &[WireMessage]) -> String {
    let mut lines = Vec::new();
    for message in history {
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        if let Some(content) = message.content.as_deref()
            && !content.trim().is_empty()
        {
            lines.push(format!("[{role}] {content}"));
        }
        for call in &message.tool_calls {
            lines.push(format!(
                "[assistant calls {}] {}",
                call.name, call.arguments
            ));
        }
    }
    lines.join("\n")
}

/// Compaction prompt: preserve the ledger discipline — what was the
/// goal, what's true now, what's decided, what's next.
const SUMMARY_PROMPT: &str = "Summarize the conversation below for a continuation agent that will carry the work forward with no other context. Respond in exactly this structure:\n\
     OBJECTIVE: the user's goal, one sentence.\n\
     STATE: what has been done — files touched, commands run, results that matter.\n\
     RULINGS: decisions made, user denials or corrections, constraints discovered.\n\
     NEXT: the immediate next action.\n\
     Be terse. Keep file paths and identifiers exact.\n\n\
     --- conversation ---\n";

async fn summarize(
    provider: Arc<dyn crate::provider::Provider>,
    transcript: &str,
) -> Result<String, String> {
    let request = Request {
        messages: vec![WireMessage::user(format!("{SUMMARY_PROMPT}\n{transcript}"))],
        tools: Vec::new(),
        max_output_tokens: None,
    };
    let mut stream = provider.stream(request);
    let mut summary = String::new();
    while let Some(event) = stream.next().await {
        match event {
            crate::Event::Token { delta } => summary.push_str(&delta),
            crate::Event::Error { message } => return Err(message),
            _ => {}
        }
    }
    if summary.trim().is_empty() {
        Err("summarizer produced nothing".into())
    } else {
        Ok(summary)
    }
}

/// Where a verbatim tail of `keep` messages may start without tearing a
/// tool round: tool results must follow the assistant call that produced
/// them. Orphan results (their call would be summarized away) are skipped
/// forward — a slice that opens on a tool result is not a legal request.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Event;
    use crate::ToolResult;
    use crate::permissions::PermissionBroker;
    use futures_core::Stream;
    use std::task::{Context as TaskContext, Poll, Waker};
    use std::time::Duration;

    /// Pops one scripted event list per stream() call — turn one emits a
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
        result: &'static str,
    }

    impl StubTool {
        fn new(name: &'static str, mutates: bool, result: &'static str) -> Self {
            Self {
                name,
                mutates,
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
                Poll::Pending => std::thread::sleep(Duration::from_millis(10)),
            }
        }
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
                provider,
                tools,
                history,
                user_message: message.into(),
                snapshots: None,
                context_window: None,
                permission_broker: Some(broker.clone()),
                system_prompt: None,
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
        // The read's start is announced before its result — silent reads
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

    #[test]
    fn mutating_tools_ask_and_honor_the_answer() {
        for (decided, expected) in [
            (PermissionDecision::Approve, "wrote"),
            (PermissionDecision::Deny, "error: denied by user"),
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
                decided
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
        let target = tmp.path().join("thing.txt");
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
        let events = drain(run_turn(spec), &broker, |_| PermissionDecision::Approve);

        assert_eq!(tool_result(&events, "edit_file"), "edited");
        // Exactly one object: the "before" content, snapshotted pre-edit.
        let objects: Vec<_> = std::fs::read_dir(tmp.path().join("snaps/objects"))
            .unwrap()
            .collect();
        assert_eq!(objects.len(), 1);
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
            panic!("a vet failure must not prompt — there is nothing to approve")
        });

        assert!(tool_result(&events, "bash").contains("blocked by policy"));
    }

    #[test]
    fn history_compacts_when_usage_crosses_the_window() {
        // Round 1 reports 90% of a 100k window; the loop should compact
        // (script 2 is the summarizer's answer) before round 3 finishes.
        let provider = Arc::new(ScriptedProvider {
            seen: std::sync::Mutex::new(Vec::new()),
            scripts: std::sync::Mutex::new(vec![
                vec![
                    Event::Usage {
                        input_tokens: 90_000,
                        output_tokens: 10,
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
                        delta: "OBJECTIVE: test compaction".into(),
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
        let (mut spec, broker) = turn(provider, registry(&[peek]), history.clone(), "go");
        spec.context_window = Some(100_000);
        let events = drain(run_turn(spec), &broker, |_| {
            panic!("read-only tools must not prompt")
        });

        assert!(
            events
                .iter()
                .any(|event| matches!(event, LoopEvent::Compacted { total: 3, .. }))
        );
        let history = history.blocking_lock();
        let summary = history
            .iter()
            .find(|message| message.role == Role::System)
            .expect("compacted history opens with the summary");
        assert!(summary.content.as_deref().unwrap().contains("OBJECTIVE"));
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

        // The model sees the denial — no silent no-op.
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
}
