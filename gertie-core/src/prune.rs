//! Superseded reads (#39): a file read whose content a later read of
//! the same file fully covers collapses to a one-line pointer.
//!
//! Every request re-sends the whole history, so a file read three
//! times costs three times, every round, for the rest of the session.
//! Only the latest copy carries information.
//!
//! The cache decides the shape. Providers cache the request prefix,
//! and rewriting an old message re-bills everything after it, so
//! pruning on every re-read would cost more than it saves. Pruning is
//! batched: nothing changes until the recoverable text passes
//! [`BATCH_CHARS`], then every superseded read collapses at once, one
//! cache miss per batch. The decision is a pure function of the
//! history, so a resumed session (history reloaded whole from the
//! database) prunes to the same bytes.

use crate::Role;
use crate::request::WireMessage;
use std::collections::HashMap;

/// Recoverable characters (about 6k tokens) that justify one rewrite.
pub const BATCH_CHARS: usize = 24_000;

/// The blocks the loop appends to a tool result. They stay when the
/// read collapses: their "already attached" checks scan the history
/// for them, and the model still needs what they said.
const ATTACHED: [&str; 2] = [
    "\n\n[directory instructions: ",
    "\n\n[memory: entries citing ",
];

const POINTER_OPENING: &str = "[read_file ";

/// A read's span of the file, in lines: `[start, end)`, end None for
/// "to the end of the file".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    start: u64,
    end: Option<u64>,
}

impl Span {
    fn covers(self, other: Span) -> bool {
        self.start <= other.start
            && match (self.end, other.end) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(mine), Some(theirs)) => mine >= theirs,
            }
    }
}

/// One read_file result in history.
struct Read {
    index: usize,
    path: String,
    span: Span,
    /// The body's length: what collapsing would recover.
    body: usize,
    /// A truncated result covers only the arguments it was read with.
    truncated: bool,
}

/// Collapses superseded reads once enough has accumulated. Returns
/// (reads collapsed, characters recovered), None when nothing changed.
pub fn prune_superseded(history: &mut [WireMessage]) -> Option<(usize, usize)> {
    let reads = reads_in(history);
    let superseded: Vec<&Read> = reads
        .iter()
        .enumerate()
        .filter(|(at, read)| {
            reads[at + 1..].iter().any(|later| {
                later.path == read.path
                    && if later.truncated {
                        later.span == read.span
                    } else {
                        later.span.covers(read.span)
                    }
            })
        })
        .map(|(_, read)| read)
        .collect();
    let recoverable: usize = superseded.iter().map(|read| read.body).sum();
    if recoverable < BATCH_CHARS {
        return None;
    }
    for read in &superseded {
        let message = &mut history[read.index];
        let content = message.content.take().unwrap_or_default();
        let attached = &content[body_end(&content)..];
        message.content = Some(format!(
            "{POINTER_OPENING}{}: this read succeeded; gertie removed this earlier copy to \
             save tokens, because a later read of the same lines, further down, has the \
             current content]{attached}",
            read.path
        ));
    }
    Some((superseded.len(), recoverable))
}

/// Where a result's own text ends and the attached blocks begin.
fn body_end(content: &str) -> usize {
    ATTACHED
        .iter()
        .filter_map(|opening| content.find(opening))
        .min()
        .unwrap_or(content.len())
}

/// Every successful, not yet collapsed read_file result, in order.
fn reads_in(history: &[WireMessage]) -> Vec<Read> {
    #[derive(serde::Deserialize)]
    struct Args {
        path: String,
        #[serde(default)]
        offset: Option<u64>,
        #[serde(default)]
        limit: Option<u64>,
    }
    let mut arguments: HashMap<&str, &str> = HashMap::new();
    let mut reads = Vec::new();
    for (index, message) in history.iter().enumerate() {
        for call in &message.tool_calls {
            arguments.insert(call.id.as_str(), call.arguments.as_str());
        }
        if message.role != Role::Tool || message.name.as_deref() != Some("read_file") {
            continue;
        }
        let Some(content) = message.content.as_deref() else {
            continue;
        };
        if content.starts_with("error:") || content.starts_with(POINTER_OPENING) {
            continue;
        }
        let Some(args) = message
            .tool_call_id
            .as_deref()
            .and_then(|id| arguments.get(id))
            .and_then(|raw| serde_json::from_str::<Args>(raw).ok())
        else {
            continue;
        };
        let start = args.offset.unwrap_or(1).max(1);
        let body = &content[..body_end(content)];
        reads.push(Read {
            index,
            path: args.path.trim_start_matches("./").to_string(),
            span: Span {
                start,
                end: args.limit.map(|limit| start + limit),
            },
            body: body.len(),
            truncated: body.ends_with("[truncated]"),
        });
    }
    reads
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::ToolCall;

    fn read(id: &str, arguments: &str, content: &str) -> [WireMessage; 2] {
        let call = ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: arguments.into(),
        };
        [
            WireMessage::assistant_with_tool_calls(None, vec![call.clone()]),
            WireMessage::tool_result(&call, content),
        ]
    }

    fn big(tag: &str) -> String {
        format!("{tag}\n{}", "x".repeat(BATCH_CHARS))
    }

    fn history(parts: Vec<[WireMessage; 2]>) -> Vec<WireMessage> {
        parts.into_iter().flatten().collect()
    }

    #[test]
    fn a_reread_file_collapses_once_the_batch_is_worth_it() {
        let mut messages = history(vec![
            read("1", r#"{"path": "src/a.rs"}"#, &big("first")),
            read("2", r#"{"path": "./src/a.rs"}"#, &big("second")),
        ]);
        let (collapsed, recovered) = prune_superseded(&mut messages).unwrap();
        assert_eq!(collapsed, 1);
        assert!(recovered > BATCH_CHARS);
        let first = messages[1].content.as_deref().unwrap();
        assert!(
            first.starts_with("[read_file src/a.rs: this read succeeded; gertie removed"),
            "{first}"
        );
        assert!(
            messages[3]
                .content
                .as_deref()
                .unwrap()
                .starts_with("second")
        );
        // Deterministic and idempotent: a second pass changes nothing.
        let before = messages.clone();
        assert!(prune_superseded(&mut messages).is_none());
        assert_eq!(messages, before);
    }

    #[test]
    fn small_savings_wait_so_the_cached_prefix_survives() {
        let mut messages = history(vec![
            read("1", r#"{"path": "a.rs"}"#, "short"),
            read("2", r#"{"path": "a.rs"}"#, "short"),
        ]);
        let before = messages.clone();
        assert!(prune_superseded(&mut messages).is_none());
        assert_eq!(messages, before);
    }

    #[test]
    fn only_a_covering_later_read_supersedes() {
        let mut messages = history(vec![
            // Whole file, then only part of it: the whole read stays.
            read("1", r#"{"path": "a.rs"}"#, &big("whole")),
            read("2", r#"{"path": "a.rs", "offset": 10, "limit": 5}"#, "part"),
            // A part, then the whole file: the part goes.
            read(
                "3",
                r#"{"path": "b.rs", "offset": 1, "limit": 50}"#,
                &big("part of b"),
            ),
            read("4", r#"{"path": "b.rs"}"#, "all of b"),
            // A different file never supersedes.
            read("5", r#"{"path": "c.rs"}"#, &big("c")),
            read("6", r#"{"path": "d.rs"}"#, "d"),
        ]);
        prune_superseded(&mut messages).unwrap();
        let text = |index: usize| messages[index].content.as_deref().unwrap();
        assert!(text(1).starts_with("whole"));
        assert!(text(5).starts_with("[read_file b.rs"));
        assert!(text(9).starts_with('c'));
    }

    #[test]
    fn a_truncated_later_read_covers_only_its_own_arguments() {
        let mut messages = history(vec![
            read(
                "1",
                r#"{"path": "a.rs", "offset": 900, "limit": 50}"#,
                &big("tail"),
            ),
            read("2", r#"{"path": "a.rs"}"#, "head only\n… [truncated]"),
        ]);
        assert!(
            prune_superseded(&mut messages).is_none(),
            "the tail is not in the head"
        );
    }

    #[test]
    fn attached_blocks_survive_the_collapse() {
        let first = format!(
            "{}\n\n[memory: entries citing a.rs]\n- [p1] x\n[end memory]",
            big("body")
        );
        let mut messages = history(vec![
            read("1", r#"{"path": "a.rs"}"#, &first),
            read("2", r#"{"path": "a.rs"}"#, "again"),
        ]);
        let (_, recovered) = prune_superseded(&mut messages).unwrap();
        let text = messages[1].content.as_deref().unwrap();
        assert!(text.starts_with("[read_file a.rs"), "{text}");
        assert!(
            text.ends_with("\n\n[memory: entries citing a.rs]\n- [p1] x\n[end memory]"),
            "{text}"
        );
        assert!(
            recovered < first.len(),
            "the block is not counted as recovered"
        );
    }

    #[test]
    fn errors_are_never_superseded_or_superseding() {
        let mut messages = history(vec![
            read("1", r#"{"path": "a.rs"}"#, &big("body")),
            read("2", r#"{"path": "a.rs"}"#, "error: read_file: gone"),
        ]);
        assert!(prune_superseded(&mut messages).is_none());
    }
}
