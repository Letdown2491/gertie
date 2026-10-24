//! The agent loop: request the model, execute its tool calls under the
//! permission policy, feed results back, repeat — bounded.

use crate::request::{Request, ToolCall, ToolSpec, WireMessage};
use crate::snapshot::SnapshotStore;
use crate::tool::Tool;
use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::sync::oneshot;

/// Safety valve: a turn that loops this many tool rounds without finishing
/// is cut off — runaway loops are how token bills happen.
const MAX_TOOL_ROUNDS: usize = 25;

/// What the UI (or any embedder) observes while a turn runs.
pub enum LoopEvent {
    /// Assistant text arrived.
    Delta {
        delta: String,
    },
    /// A mutating tool wants to run. Grant or deny through `respond`; the
    /// loop waits until answered (a dropped responder counts as a deny —
    /// if the UI went away, doing nothing is the safe move).
    ToolCallRequested {
        call: ToolCall,
        respond: oneshot::Sender<PermissionDecision>,
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
pub fn run_turn(
    provider: Arc<dyn crate::provider::Provider>,
    tools: ToolRegistry,
    history: SharedHistory,
    user_message: String,
    snapshots: Option<Arc<SnapshotStore>>,
) -> BoxStream<'static, LoopEvent> {
    let specs = tool_specs(&tools);
    Box::pin(async_stream::stream! {
        let mut request = Request {
            tools: specs,
            ..Request::default()
        };

        history.lock().await.push(WireMessage::user(user_message));

        for round in 1..=MAX_TOOL_ROUNDS {
            request.messages = history.lock().await.clone();

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
                        // Mutating tools face the permission prompt first;
                        // the yield reaches the UI and the loop resumes on
                        // the answer.
                        let permitted = if tool.mutates() {
                            let (respond, answer) = oneshot::channel();
                            yield LoopEvent::ToolCallRequested {
                                call: call.clone(),
                                respond,
                            };
                            matches!(answer.await, Ok(PermissionDecision::Approve))
                        } else {
                            yield LoopEvent::ToolCallStarted { call: call.clone() };
                            true
                        };
                        match permitted {
                            false => Err("denied by user".to_string()),
                            true => {
                                // Checkpoint the file's prior state before
                                // anything mutates it.
                                if tool.mutates()
                                    && let (Some(store), Some(path)) =
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
    use futures_core::Stream;
    use std::task::{Context as TaskContext, Poll, Waker};
    use std::time::Duration;

    /// Pops one scripted event list per stream() call — turn one emits a
    /// tool call, turn two the final text.
    struct ScriptedProvider {
        scripts: std::sync::Mutex<Vec<Vec<Event>>>,
    }

    impl crate::provider::Provider for ScriptedProvider {
        fn stream(&self, _request: crate::Request) -> BoxStream<'static, Event> {
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

    /// Polls the stream to completion, answering permission prompts with
    /// `decide` (the loop runs on the provider's own runtime).
    fn drain(
        stream: BoxStream<'static, LoopEvent>,
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
                    // responder can't be cloned, and no test inspects it.
                    if let LoopEvent::ToolCallRequested { call, respond } = event {
                        let _ = respond.send(decide(&call));
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
            scripts: std::sync::Mutex::new(vec![vec![
                Event::Token { delta: "hi".into() },
                Event::Done,
            ]]),
        });
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let events = drain(
            run_turn(
                provider,
                registry(&[]),
                history.clone(),
                "hello".into(),
                None,
            ),
            |_| unreachable!("no tools to prompt for"),
        );

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
        let events = drain(
            run_turn(
                provider,
                registry(&[peek]),
                history.clone(),
                "look".into(),
                None,
            ),
            |_| panic!("read-only tools must not prompt"),
        );

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
            let events = drain(
                run_turn(
                    provider,
                    registry(&[writer]),
                    history.clone(),
                    "go".into(),
                    None,
                ),
                |call| {
                    assert_eq!(call.name, "write_file");
                    decided
                },
            );

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
        let events = drain(
            run_turn(
                provider,
                registry(&[editor]),
                Arc::new(tokio::sync::Mutex::new(Vec::new())),
                String::new(),
                Some(snapshots.clone()),
            ),
            |_| PermissionDecision::Approve,
        );

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
        let events = drain(
            run_turn(provider, registry(&[]), history.clone(), "go".into(), None),
            |_| panic!("nothing to prompt for"),
        );

        assert!(tool_result(&events, "mystery").starts_with("error: unknown tool"));
    }
}
