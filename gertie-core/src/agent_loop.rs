//! The agent loop: request the model, execute its tool calls under the
//! permission policy, feed results back, repeat — bounded.

use crate::Role;
use crate::permissions::PermissionMode;
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
    /// A steered user message joined the history at a round boundary.
    /// The receiver persists it like any user turn — the loop only
    /// owns the in-memory history.
    Steered {
        text: String,
    },
    Error {
        message: String,
    },
    /// The provider's rate limit answered every retry: the turn ends
    /// limited. The UI persists the state and offers a manual Resume —
    /// a fresh turn that continues from history without a new user
    /// message.
    Limited {
        message: String,
    },
    Done,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Approve,
    /// The human refused. `feedback` is what they typed back — a bare
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
    /// The message opening the turn — None means *resume*: continue
    /// from history as it stands (it already ends with the user's
    /// message, or a tool result from the round that got rate-limited).
    pub user_message: Option<String>,
    pub snapshots: Option<Arc<SnapshotStore>>,
    /// Checkpoint attribution: rows recorded for this turn's mutating
    /// tools carry this session, so the ledger's turn windows can find
    /// them. None leaves rows unattributed — they show in no turn diff.
    pub checkpoint_session: Option<crate::SessionId>,
    /// Where tool-relative paths resolve: the same root the tool
    /// registry was built with. The checkpoint hook needs it to
    /// snapshot the file the tool will actually touch — a relative
    /// argument means workspace-relative, not CWD-relative.
    pub workspace_root: Option<std::path::PathBuf>,
    pub context_window: Option<u64>,
    /// Shared approval registry. When set, every ask — this loop's or a
    /// sub-loop's — registers here so one UI can answer them all. None
    /// means nobody can approve: mutations are denied, doing nothing is
    /// the safe move.
    pub permission_broker: Option<Arc<crate::PermissionBroker>>,
    /// How this turn's mutating-tool asks resolve — supervised, or a
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
    /// Sent as the first message of every request, never stored in
    /// history: it is per-turn framing, not conversation record (the
    /// DB stays canonical; the next turn may have a different index).
    pub system_prompt: Option<String>,
    /// Mid-run steering: user messages typed while the turn is already
    /// running. Drained at round boundaries — each becomes a user push
    /// plus a `LoopEvent::Steered` — and a steer landing on what would
    /// be the final round buys another one, so it is always answered.
    /// None = the turn cannot be steered.
    pub steer: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
    /// Attachments riding with `user_message`, as data URLs. Steered
    /// messages are text-only; the first message of the turn carries
    /// the attachments.
    pub user_attachments: Vec<String>,
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
/// by a budgeted selection (verbatim prefixes — no summarizer model)
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
        system_prompt,
        steer,
        user_attachments,
    } = spec;
    let mut steer = steer;
    let specs = tool_specs(&tools);
    Box::pin(async_stream::stream! {
        let mut request = Request {
            tools: specs,
            ..Request::default()
        };

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
                    // Selection, not summarization: no model call, so
                    // there is no provider to fail — the only way this
                    // "fails" is nothing to select, which changes
                    // nothing.
                    if let Some((kept, total)) =
                        compact_history(history.clone(), selection_budget(context_window)).await
                    {
                        // The old observation describes the pre-compact
                        // prompt; don't let it re-trigger.
                        last_input_tokens = None;
                        yield LoopEvent::Compacted { kept, total };
                    }
                }
            }
            // Steering: messages the user typed mid-run join the
            // conversation here — the model reads them with the next
            // request, after the tool results it was working on.
            if let Some(steer) = steer.as_mut() {
                while let Ok(text) = steer.try_recv() {
                    history.lock().await.push(WireMessage::user(&text));
                    yield LoopEvent::Steered { text };
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
                    crate::Event::Limited { message } => {
                        yield LoopEvent::Limited { message };
                        return;
                    }
                }
            }

            if calls.is_empty() {
                history
                    .lock()
                    .await
                    .push(WireMessage::assistant(assistant_text));
                // A steer landing on what would be the final round buys
                // another one: the message joins the history and the
                // loop turns back to answer it. The round budget still
                // bounds this — steering is not an infinite ladder.
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
                        // Mutating tools: the turn's mode pre-approves
                        // a class of asks outright — an edit the
                        // checkpoint will snapshot is reversible, a
                        // sandboxed command is contained — and the
                        // rest face the permission prompt: the ask
                        // registers with the broker (so the UI can
                        // answer it) and the loop awaits the answer.
                        else if tool.mutates() {
                            let checkpoint_target = checkpoint_path(
                                &call.arguments,
                                workspace_root.as_deref(),
                            );
                            let pre_approved = permission_mode.auto_allows(
                                checkpoint_target.is_some(),
                                sandbox_bash,
                                tool.third_party(),
                            );
                            let decision = match (&permission_broker, pre_approved) {
                                // The mode settled it before any ask.
                                (_, true) => Ok(PermissionDecision::Approve),
                                (Some(broker), false) => {
                                    let (request_id, answer) =
                                        broker.request(call.clone(), prompt_source);
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
                                    Err("no permission channel available — denied".to_string())
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
                                        && let Err(error) =
                                            store.record_file(checkpoint_session, &path)
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
                                // The human said no — and possibly
                                // why. Their reply rides in the tool
                                // result, so the next round can act
                                // on it instead of repeating itself.
                                Ok(PermissionDecision::Deny { feedback }) => Err(
                                    match feedback {
                                        Some(text) if !text.trim().is_empty() => {
                                            format!("denied by user — their reply: {text}")
                                        }
                                        _ => "denied by user".to_string(),
                                    },
                                ),
                                Err(denied) => Err(denied),
                            }
                        } else {
                            yield LoopEvent::ToolCallStarted { call: call.clone() };
                            tool.run(&call.arguments).await
                        }
                    }
                };
                let result = match result {
                    Ok(text) => crate::strip_invisible(&text),
                    Err(text) => format!("error: {}", crate::strip_invisible(&text)),
                };

                // The strip above is the prompt boundary: tool output —
                // a web page, an MCP payload, anything the world sent
                // back — enters the model's context and the transcript
                // here, and enters it without bidirectional controls,
                // zero-width marks, or tag characters that could make
                // either reader see something other than what is there.
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
/// How many trailing messages survive a compaction. The UI's manual
/// `/compact` reads this as its minimum-size gate.
pub const KEEP_TAIL: usize = 8;

/// An item of the compaction digest keeps a 240-char prefix — enough
/// to know what happened, not enough to fill a window. (t3code's
/// handoff rule: a budgeted selection, not an agent-written summary.)
const ITEM_PREFIX: usize = 240;

/// The original request keeps a bigger allowance than the digest's
/// other lines — it anchors everything that follows.
const REQUEST_PREFIX: usize = 2_000;

/// Tokens held back for instructions, tools, and the work that follows
/// a compaction: at least 16k, never more than a quarter of the window
/// (the same reserve shape t3code uses).
fn reserve_tokens(window: u64) -> u64 {
    (window / 4).max(16_000)
}

/// Selection never shrinks below this, even on tiny windows — a digest
/// that keeps nothing is worse than one that overshoots.
const MIN_SELECTION_TOKENS: u64 = 4_000;

/// When no context window is configured (compaction never auto-fires),
/// `/compact` budgets against this assumption — t3code's stand-in for
/// unknown capacity.
const ASSUMED_WINDOW_TOKENS: u64 = 128_000;

/// The character budget for a compaction's selection: the window minus
/// the reserve, in chars at the bytes/4 estimate — floored so a tiny
/// window still keeps *something*.
pub fn selection_budget(context_window: Option<u64>) -> usize {
    let window = context_window.unwrap_or(ASSUMED_WINDOW_TOKENS);
    let budget_tokens = window
        .saturating_sub(reserve_tokens(window))
        .max(MIN_SELECTION_TOKENS);
    (budget_tokens * 4) as usize
}

/// Compacts the history in place: everything before the kept tail
/// becomes a budgeted SELECTION, not a summary — the original request,
/// then verbatim prefixes of the omitted turns, newest first, inside
/// one system message. No model is called; every line cites the record
/// verbatim. Returns `(kept, total)` — None when the kept tail already
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

/// The synchronous core of `compact_history` — the lock is the only
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

/// Flattens the pre-tail history into the digest's lines: the original
/// request on its own line, then one line per omitted item — verbatim
/// prefix, role cited — newest first until the character budget is
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
                "[user — the original request] {}",
                prefix(text, REQUEST_PREFIX)
            ));
        }
    }
    // The rest of the digest: walk newest → oldest (the turns nearest
    // the kept tail matter most), stop when the budget is spent, then
    // put the survivors back in the order they happened. The anchor
    // message is skipped — it already has its line.
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
/// used — legal on every wire — and later compactions treat it like
/// any other message.
fn selection_message(digest: &str) -> WireMessage {
    WireMessage {
        role: Role::System,
        content: Some(format!(
            "### Earlier history — selected from the record, not summarized\n\n\
             {digest}\n\n\
             Context was compacted to fit the window. The lines above are\n\
             verbatim prefixes, not a retelling; the full text of every\n\
             omitted message is still in the session record — the\n\
             read_session tool retrieves it."
        )),
        ..WireMessage::default()
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

/// The path the checkpoint hook snapshots: the tool's argument, with a
/// relative name resolved against the workspace root — the same
/// resolution every path-naming tool applies. The result is always
/// *contained*: lexical `..` walks plus a canonicalize check on the
/// nearest existing ancestor, the same rules the file tools' resolve()
/// applies. A path that escapes (or an absolute one outside the
/// workspace) is None — the ask is not pre-approvable, because the
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
/// must canonicalize back inside the workspace root — a symlink inside
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
        // Absolute paths outside the workspace — the model's home
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
                user_message: Some(message.into()),
                snapshots: None,
                checkpoint_session: None,
                workspace_root: None,
                context_window: None,
                permission_broker: Some(broker.clone()),
                permission_mode: crate::permissions::PermissionMode::Supervised,
                sandbox_bash: false,
                prompt_source: crate::PromptSource::Leader,
                system_prompt: None,
                steer: None,
                user_attachments: Vec::new(),
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

    /// A limited turn is terminal and reaches the UI as its own event —
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
        // opened the turn is in history — what a resume continues from.
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

    /// A resume turn pushes nothing: it continues from history as it
    /// stands — the rate-limited round's rows are still there.
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
        // boundary, before request one — the model reads them with it.
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
    /// Done — the user typed while the model was finishing, the
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
        // The model finishes — but the steer landed first, so the loop
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
    fn nothing_follows_done_turn_end_is_not_settlement() {
        // The heaviest path (a tool round) must still end the instant
        // Done is yielded: the loop does no settlement work — no
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
            "nothing may follow Done — turn end is not settlement"
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
                    feedback: Some("no — use the staging dir".into()),
                },
                "error: denied by user — their reply: no — use the staging dir",
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
        // The tool's argument is workspace-relative — the checkpoint
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
        let events = drain(run_turn(spec), &broker, |_| PermissionDecision::Approve);

        assert_eq!(tool_result(&events, "edit_file"), "edited");
        // Exactly one object, and it holds the file's real prior bytes —
        // not an empty read from the process CWD.
        let objects: Vec<_> = std::fs::read_dir(tmp.path().join("snaps/objects"))
            .unwrap()
            .collect();
        assert_eq!(objects.len(), 1);
        let object = objects[0].as_ref().unwrap().path();
        assert_eq!(std::fs::read(object).unwrap(), b"before");
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
            // The edit is checkpointed and reversible — it ran unasked.
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
        // same ask prompts — the mode never pretends a kernel that
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
            panic!("full access must not ask — the human is the only thing removed")
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
            panic!("a vet failure must not prompt — there is nothing to approve")
        });

        assert!(tool_result(&events, "bash").contains("blocked by policy"));
    }

    #[test]
    fn history_compacts_when_usage_crosses_the_window() {
        // A prior conversation sits in history; round 1 observes 90% of
        // a 100k window — the loop must replace the old turns with a
        // budgeted selection before the next request. No summarizer
        // round exists: two scripts drive the whole turn.
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
                        delta: "all done".into(),
                    },
                    Event::Done,
                ],
            ]),
        });
        let peek: Arc<dyn Tool> = Arc::new(StubTool::new("peek", false, "peeked"));
        let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        // The prior conversation: an original request and ten settled
        // turns — enough head for the selection to have something to
        // select.
        {
            let mut current = history.blocking_lock();
            current.push(WireMessage::user("the original request"));
            for i in 0..10 {
                current.push(WireMessage::assistant(format!(
                    "old turn {i} — did a thing"
                )));
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
            selection.contains("[user — the original request] the original request"),
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
            "[user — the original request] goal: refactor the thing"
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
                "ancient turn {i} — {}",
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
        let text = "héllo wörld — ünïcode ✓";
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
}
