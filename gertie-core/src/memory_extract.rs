// gertie-core/src/memory_extract.rs
//! Session-end extraction: one provider call turns the session's
//! digest into proposed memory ops.
//!
//! The design (DESIGN.md "Memory") makes this the *only* automatic
//! writer, and the proposal is the artifact the user reviews: the
//! extraction proposes delta ops (the same `Op` type the store applies),
//! never applies them. Eligibility filters first (Codex's pattern):
//! short and still-active sessions are skipped, sessions that ingested
//! untrusted web/repo content are flagged on the proposal, already-
//! extracted digests are skipped via their hash in the sidecar.

use crate::Event;
use crate::memory::Op;
use crate::provider::Provider;
use crate::request::Request;
use crate::sanitize_untrusted;
use futures_util::StreamExt;
use std::sync::Arc;

/// A session's digest: the text the extraction call sees. Built from
/// the session's persisted messages (the record, not the framing).
pub fn digest_from(messages: &[crate::Message], max_chars: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    for message in messages {
        let role = match message.role {
            crate::Role::User => "user",
            crate::Role::Assistant => "assistant",
            // Tool rows are noise for extraction, except a denial with
            // a reason (#44): the user correcting the agent, in place.
            crate::Role::Tool => {
                if let Some(correction) = correction_in(message) {
                    lines.push(format!(
                        "user (rejected {} `{}`): {}",
                        correction.tool, correction.target, correction.reply
                    ));
                }
                continue;
            }
            _ => continue,
        };
        let text = message.content.trim();
        if text.is_empty() {
            continue;
        }
        lines.push(format!("{}: {}", role, text));
    }
    let joined = lines.join("\n\n");
    if joined.chars().count() <= max_chars {
        return sanitize_untrusted(&joined, max_chars);
    }
    // Keep the head (the request) and the tail (the outcome); the
    // middle is what a digest already summarizes.
    let head = joined.chars().take(max_chars / 2).collect::<String>();
    let tail_start = joined.chars().count() - max_chars / 2;
    let tail = joined.chars().skip(tail_start).collect::<String>();
    format!("{head}\n\n[middle omitted]\n\n{tail}")
}

/// A user correction (#44): a tool call the user denied, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correction {
    pub tool: String,
    /// What was attempted: the command, the path, or the first argument.
    pub target: String,
    /// The file involved, when the call named one: the evidence a
    /// correction entry cites, so recall on touch brings it back.
    pub path: Option<String>,
    pub reply: String,
}

/// The prefix the loop gives a denial that carried the user's reply.
const DENIED_WITH_REPLY: &str = "error: denied by user, their reply: ";

/// Every correction in a session, in order. A bare denial ("no") is
/// not one: it says what, not why, and memory needs the why.
pub fn corrections_from(messages: &[crate::Message]) -> Vec<Correction> {
    messages.iter().filter_map(correction_in).collect()
}

fn correction_in(message: &crate::Message) -> Option<Correction> {
    if message.role != crate::Role::Tool {
        return None;
    }
    let reply = message.content.strip_prefix(DENIED_WITH_REPLY)?.trim();
    if reply.is_empty() {
        return None;
    }
    let arguments: serde_json::Value = message
        .tool_arguments
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default();
    let field = |name: &str| arguments.get(name).and_then(|value| value.as_str());
    let path = field("path")
        .or_else(|| field("file_path"))
        .map(|path| path.trim_start_matches("./").to_string());
    let target = field("command")
        .map(str::to_string)
        .or_else(|| path.clone())
        .or_else(|| {
            arguments
                .as_object()
                .and_then(|object| object.values().find_map(|value| value.as_str()))
                .map(str::to_string)
        })
        .unwrap_or_default();
    Some(Correction {
        tool: message.tool_name.clone().unwrap_or_else(|| "a tool".into()),
        target: sanitize_untrusted(&target, 120),
        path,
        reply: sanitize_untrusted(reply, 400),
    })
}

/// The files a session read or wrote, from its tool rows: every
/// call whose arguments name a `path` (or `file_path`), in first-touch
/// order, deduplicated, at most 40. Extraction may cite only these as
/// evidence, so a cited file is one the session actually looked at.
/// Paths are as the model wrote them; the store resolves and checks
/// them when it hashes.
pub fn touched_files(messages: &[crate::Message]) -> Vec<String> {
    let mut touched: Vec<String> = Vec::new();
    for message in messages {
        if message.role != crate::Role::Tool {
            continue;
        }
        let Some(arguments) = message.tool_arguments.as_deref() else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
            continue;
        };
        let Some(path) = value
            .get("path")
            .or_else(|| value.get("file_path"))
            .and_then(|path| path.as_str())
        else {
            continue;
        };
        let path = path.trim().trim_start_matches("./").to_string();
        if path.is_empty() || touched.contains(&path) {
            continue;
        }
        touched.push(path);
        if touched.len() == 40 {
            break;
        }
    }
    touched
}

/// Drops every evidence path an add cites that is not in `touched`:
/// a model can name a file, only the session's own reads make it
/// evidence.
pub fn restrict_evidence(ops: &mut [Op], touched: &[String]) {
    for op in ops {
        if let Op::Add { evidence, .. } = op {
            evidence.retain(|path| touched.contains(path));
        }
    }
}

/// The extraction prompt: small, flat, ACE-shaped. The schema is the
/// ops JSON the store already understands; counters and ids stay out
/// of the model's hands (the merge assigns them).
pub fn extraction_prompt(digest: &str, index: &str, touched: &[String]) -> String {
    let files = if touched.is_empty() {
        "(none)".to_string()
    } else {
        touched.join("\n")
    };
    format!(
        "You are extracting durable memory from a coding-agent session. \
Propose changes to a long-term memory store as a JSON array of ops. \
Ops:\n\
- {{\"op\":\"add\",\"file\":\"<topic-file>.md\",\"text\":\"<one-line entry>\",\"source\":\"<session ref>\",\"evidence\":[\"<file from the list below>\"]}}\n\
- {{\"op\":\"update\",\"id\":\"<entry id>\",\"text\":\"<one-line replacement>\"}}\n\
- {{\"op\":\"supersede\",\"id\":\"<entry id>\",\"by\":\"<id of the existing entry that replaces it, if any>\"}}\n\
- {{\"op\":\"archive\",\"id\":\"<entry id>\"}}\n\
- {{\"op\":\"link\",\"from\":\"<id>\",\"to\":\"<id>\",\"relation\":\"relates_to|solves|causes|builds_on|replaces\"}}\n\n\
Corrections come first: a line \"user (rejected <tool> ...): <why>\", or a \
user message telling the agent it got something wrong, states how this \
project wants things done. Record each one that will hold beyond this task \
as an add with \"source\":\"correction\", worded as the rule (\"use cargo \
clean, never rm -rf target\"), citing the file involved when there is one.\n\
Rules: one line per entry, under 160 chars. Facts and preferences only, \
never task narration. Supersede an existing entry when the new fact \
replaces it: pair the supersede with the add that states the new fact \
(with exactly one add in the array, the pair is linked automatically), \
or name an existing replacement in by. For a fact about the code, cite \
in evidence the files from this list that support it (at most 8, only \
these; omit evidence otherwise):\n{files}\n\n\
Existing memory (do not repeat it):\n{index}\n\n\
Return ONLY the JSON array, no prose.\n\nSession:\n{digest}"
    )
}

/// One proposed op, parsed and ready for review. The proposal keeps
/// the raw op so applying it after approval is the store's own path.
#[derive(Debug, Clone)]
pub struct Proposal {
    pub ops: Vec<Op>,
    /// Untrusted-content flag: sessions whose web_fetch fraction crossed
    /// a threshold mark their proposal, and the card says so.
    pub flagged: bool,
    pub digest_hash: String,
}

/// Parses the model's JSON array into ops. A parse failure is a None
/// with a reason: extraction failures are notices, never errors.
pub fn parse_ops(raw: &str) -> Result<Vec<Op>, String> {
    // Tolerate a fenced block; models like to wrap JSON.
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(|rest| rest.trim_start())
        .unwrap_or(trimmed);
    let inner = inner
        .strip_suffix("```")
        .map(|rest| rest.trim_end())
        .unwrap_or(inner);
    let value: serde_json::Value =
        serde_json::from_str(inner).map_err(|error| format!("unparseable ops: {error}"))?;
    let array = value
        .as_array()
        .ok_or_else(|| "ops must be a JSON array".to_string())?;
    if array.len() > 16 {
        return Err(format!("too many ops ({})", array.len()));
    }
    let mut ops = Vec::new();
    for item in array {
        let obj = item
            .as_object()
            .ok_or_else(|| "each op must be an object".to_string())?;
        let kind = obj
            .get("op")
            .and_then(|kind| kind.as_str())
            .ok_or_else(|| "missing op kind".to_string())?;
        let text_field = |name: &str| -> Result<String, String> {
            obj.get(name)
                .and_then(|value| value.as_str())
                .map(|value| sanitize_untrusted(value, 200).to_string())
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| format!("missing {name}"))
        };
        let op = match kind {
            "add" => Op::Add {
                file: sanitize_untrusted(&text_field("file")?, 64),
                text: text_field("text")?,
                source: obj
                    .get("source")
                    .and_then(|value| value.as_str())
                    .map(|value| sanitize_untrusted(value, 64).to_string()),
                evidence: obj
                    .get("evidence")
                    .and_then(|value| value.as_array())
                    .map(|paths| {
                        paths
                            .iter()
                            .filter_map(|path| path.as_str())
                            .take(crate::memory::EVIDENCE_LIMIT)
                            .map(|path| sanitize_untrusted(path, 256))
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            "update" => Op::Update {
                id: text_field("id")?,
                text: text_field("text")?,
            },
            "supersede" => Op::Supersede {
                id: text_field("id")?,
                by: obj
                    .get("by")
                    .and_then(|value| value.as_str())
                    .map(|value| sanitize_untrusted(value, 16))
                    .filter(|value| !value.trim().is_empty()),
            },
            "archive" => Op::Archive {
                id: text_field("id")?,
            },
            "link" => Op::Link {
                from: text_field("from")?,
                to: text_field("to")?,
                relation: obj
                    .get("relation")
                    .and_then(|value| value.as_str())
                    .unwrap_or("relates_to")
                    .to_string(),
            },
            other => return Err(format!("unknown op kind: {other}")),
        };
        ops.push(op);
    }
    Ok(ops)
}

/// Runs one extraction call: digest + current index in, ops out.
/// One request, no tools, stream drained to completion. Returns the
/// raw text (parsing is the caller's step, so the review card can
/// show what the model actually said when parsing failed).
pub async fn run_extraction(
    provider: Arc<dyn Provider>,
    digest: &str,
    index: &str,
    touched: &[String],
) -> Result<String, String> {
    let prompt = extraction_prompt(digest, index, touched);
    let request = Request {
        messages: vec![crate::request::WireMessage::user(&prompt)],
        tools: Vec::new(),
        // No output cap, the same budget a chat turn gets: a reasoning
        // model spends tokens on thinking before any content arrives,
        // and a cap that thinking eats strands the request (the
        // empty-extraction bug: 2048 was all reasoning, zero content).
        max_output_tokens: None,
    };
    let mut stream = provider.stream(request);
    let mut collected = String::new();
    let mut produced = false;
    while let Some(event) = stream.next().await {
        match event {
            Event::Token { delta } => {
                produced = true;
                collected.push_str(&delta);
            }
            // Thinking is live production: the endpoint is up even
            // before any content arrives.
            Event::Reasoning { .. } => produced = true,
            Event::Error { message } => return Err(message),
            Event::Limited { message } => return Err(format!("rate limited: {message}")),
            Event::Done => break,
            _ => {}
        }
    }
    // Never hand an empty string to the parser: the error names the
    // shape of the silence instead of serde's "EOF at column 0".
    if collected.is_empty() {
        return Err(if produced {
            "the model reasoned but returned no content".to_string()
        } else {
            "the stream ended without any response".to_string()
        });
    }
    Ok(collected)
}

/// Tests live here (the extraction module's behavior matrix).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ops_reads_the_flat_schema() {
        let raw = r#"[
            {"op":"add","file":"gotchas.md","text":"cite the flake first","source":"388"},
            {"op":"update","id":"p12","text":"rewritten"},
            {"op":"supersede","id":"p7"},
            {"op":"archive","id":"p9"},
            {"op":"link","from":"p1","to":"p2","relation":"solves"}
        ]"#;
        let ops = parse_ops(raw).unwrap();
        assert_eq!(ops.len(), 5);
        assert!(matches!(
            ops[0],
            Op::Add { ref file, .. } if file == "gotchas.md"
        ));
        assert!(matches!(ops[2], Op::Supersede { ref id, by: None } if id == "p7"));
        assert!(matches!(ops[4], Op::Link { ref relation, .. } if relation == "solves"));
        let named = parse_ops(r#"[{"op":"supersede","id":"p7","by":"p8"}]"#).unwrap();
        assert!(matches!(
            named[0],
            Op::Supersede { ref id, by: Some(ref by) } if id == "p7" && by == "p8"
        ));
    }

    #[test]
    fn parse_ops_tolerates_fences_and_rejects_junk() {
        assert!(parse_ops("```json\n[]\n```").is_ok());
        assert!(parse_ops("not json").is_err());
        assert!(parse_ops(r#"[{"op":"zap","id":"p1"}]"#).is_err());
        // 17 ops exceeds the cap.
        let many: Vec<String> = (0..17)
            .map(|i| format!(r#"{{"op":"add","file":"f.md","text":"t{i}"}}"#))
            .collect();
        assert!(parse_ops(&format!("[{}]", many.join(","))).is_err());
    }

    #[test]
    fn digest_keeps_head_and_tail_with_tool_rows_skipped() {
        let row = |role: crate::Role, text: &str| crate::Message {
            id: crate::MessageId(1),
            parent_id: None,
            role,
            content: text.into(),
            tool_call_id: None,
            tool_name: None,
            tool_arguments: None,
            images: Vec::new(),
        };
        let messages = vec![
            row(crate::Role::User, "the request"),
            row(crate::Role::Tool, "tool noise the digest must not carry"),
            row(crate::Role::Assistant, "the outcome"),
        ];
        let digest = digest_from(&messages, 10_000);
        assert!(digest.contains("user: the request"));
        assert!(digest.contains("assistant: the outcome"));
        assert!(!digest.contains("tool noise"));
    }

    #[test]
    fn extraction_prompt_carries_the_schema_and_index() {
        let prompt = extraction_prompt("digest text", "existing memory", &["src/a.rs".into()]);
        assert!(prompt.contains("\"op\":\"add\""));
        assert!(prompt.contains("existing memory"));
        assert!(prompt.contains("\nsrc/a.rs\n"));
        assert!(extraction_prompt("d", "i", &[]).contains("(none)"));
    }

    fn row(role: crate::Role, content: &str) -> crate::Message {
        crate::Message {
            id: crate::MessageId(1),
            parent_id: None,
            role,
            content: content.into(),
            tool_call_id: None,
            tool_name: None,
            tool_arguments: None,
            images: Vec::new(),
        }
    }

    fn denied(tool: &str, arguments: &str, reply: Option<&str>) -> crate::Message {
        crate::Message {
            id: crate::MessageId(1),
            parent_id: None,
            role: crate::Role::Tool,
            content: match reply {
                Some(reply) => format!("error: denied by user, their reply: {reply}"),
                None => "error: denied by user".into(),
            },
            tool_call_id: Some("c".into()),
            tool_name: Some(tool.into()),
            tool_arguments: Some(arguments.into()),
            images: Vec::new(),
        }
    }

    /// #44: a denial with a reason is the user stating how the project
    /// wants things done. It lived only in a tool row, which the digest
    /// skipped, so extraction never saw it.
    #[test]
    fn denials_with_a_reason_are_corrections_in_the_digest() {
        let messages = vec![
            row(crate::Role::User, "clean up the build"),
            denied(
                "bash",
                r#"{"command": "rm -rf target"}"#,
                Some("never rm target here, use cargo clean"),
            ),
            denied(
                "edit_file",
                r#"{"path": "src/pool.rs"}"#,
                Some("POOL_SIZE stays 8"),
            ),
            denied("bash", r#"{"command": "ls"}"#, None),
            row(crate::Role::Assistant, "ran cargo clean instead"),
        ];
        let corrections = corrections_from(&messages);
        assert_eq!(corrections.len(), 2, "a bare denial carries no lesson");
        assert_eq!(corrections[0].tool, "bash");
        assert_eq!(corrections[0].target, "rm -rf target");
        assert_eq!(corrections[1].target, "src/pool.rs");
        assert_eq!(corrections[1].path.as_deref(), Some("src/pool.rs"));
        let digest = digest_from(&messages, 10_000);
        let at = |needle: &str| {
            digest
                .find(needle)
                .unwrap_or_else(|| panic!("{needle}: {digest}"))
        };
        assert!(
            at("user: clean up the build")
                < at("user (rejected bash `rm -rf target`): never rm target here")
        );
        assert!(
            at("never rm target") < at("assistant: ran cargo clean"),
            "in order"
        );
        assert!(!digest.contains("denied by user"), "the raw row stays out");
    }

    /// #44 end to end, minus the provider call: a short session with
    /// one denial and its reason, through the digest, the parsed
    /// proposal, evidence restriction and the store, to the recall
    /// block the next session gets when it touches the file.
    #[test]
    fn a_correction_comes_back_when_the_file_is_touched_again() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/pool.rs"),
            "pub const POOL_SIZE: usize = 8;\n",
        )
        .unwrap();
        let store = crate::MemoryStore::open(
            root.join(".gertie/memory"),
            root.join("global"),
            Arc::new(crate::SessionStore::open(std::path::Path::new(":memory:")).unwrap()),
        );
        let messages = vec![
            row(crate::Role::User, "raise the pool to 32"),
            denied(
                "edit_file",
                r#"{"path": "./src/pool.rs"}"#,
                Some("no: the CI runners have 8 cores, a bigger pool deadlocks teardown"),
            ),
            row(crate::Role::Assistant, "left it at 8"),
        ];
        let digest = digest_from(&messages, 12_000);
        assert!(digest.contains("deadlocks teardown"), "{digest}");
        let touched = touched_files(&messages);
        assert_eq!(touched, vec!["src/pool.rs"]);

        // What an extraction model returns for that digest.
        let mut ops = parse_ops(
            r#"[{"op":"add","file":"gotchas.md","source":"correction",
                "text":"POOL_SIZE stays 8: the CI runners have 8 cores and a bigger pool deadlocks teardown",
                "evidence":["src/pool.rs","src/elsewhere.rs"]}]"#,
        )
        .unwrap();
        restrict_evidence(&mut ops, &touched);
        store.apply(&ops, 4096).unwrap();
        store.sync().unwrap();

        let entry = store.scan().into_iter().next().unwrap();
        assert_eq!(entry.helpful, 1, "the user said it: one vote up");
        assert_eq!(entry.evidence.len(), 1);
        assert_eq!(entry.evidence[0].path, "src/pool.rs");

        // Next session: the file is touched, the correction rides along.
        let block = crate::memory_recall::block_for(&store, &root, &root.join("src/pool.rs"), &[])
            .expect("the correction cites the file");
        assert!(block.contains("deadlocks teardown"), "{block}");
    }

    #[test]
    fn the_prompt_asks_for_corrections_first() {
        let prompt = extraction_prompt("d", "i", &[]);
        assert!(prompt.contains("\"source\":\"correction\""), "{prompt}");
        assert!(prompt.contains("rejected"), "{prompt}");
    }

    fn tool_row(arguments: &str) -> crate::Message {
        crate::Message {
            id: crate::MessageId(1),
            parent_id: None,
            role: crate::Role::Tool,
            content: "ok".into(),
            tool_call_id: Some("c".into()),
            tool_name: Some("read_file".into()),
            tool_arguments: Some(arguments.into()),
            images: Vec::new(),
        }
    }

    #[test]
    fn touched_files_come_from_path_arguments_once_each() {
        let messages = vec![
            tool_row(r#"{"path": "./src/a.rs"}"#),
            tool_row(r#"{"file_path": "src/b.rs"}"#),
            tool_row(r#"{"path": "src/a.rs"}"#),
            tool_row(r#"{"command": "ls"}"#),
            tool_row("not json"),
        ];
        assert_eq!(touched_files(&messages), vec!["src/a.rs", "src/b.rs"]);
    }

    /// A canned provider: replays `events` verbatim. The smallest
    /// possible stand-in for the wire's behavior on this endpoint.
    struct Canned(Vec<Event>);

    impl Provider for Canned {
        fn stream(&self, _request: Request) -> futures_core::stream::BoxStream<'static, Event> {
            let events = self.0.clone();
            Box::pin(futures_util::stream::iter(events))
        }
    }

    #[test]
    fn a_reasoning_only_stream_errors_honestly_not_as_json_eof() {
        // The glm-5.3-flash bug, distilled: the model thought until the
        // old cap cut it off, content never arrived, and the caller
        // read serde's "EOF at column 0" instead of the truth.
        let provider = Arc::new(Canned(vec![
            Event::Reasoning {
                delta: "thinking".into(),
            },
            Event::Done,
        ]));
        let error =
            crate::testing::block_on(run_extraction(provider, "digest", "index", &[])).unwrap_err();
        assert!(
            error.contains("reasoned but returned no content"),
            "{error}"
        );
    }

    #[test]
    fn an_empty_stream_errors_honestly_not_as_json_eof() {
        let provider = Arc::new(Canned(vec![Event::Done]));
        let error =
            crate::testing::block_on(run_extraction(provider, "digest", "index", &[])).unwrap_err();
        assert!(
            error.contains("stream ended without any response"),
            "{error}"
        );
    }

    #[test]
    fn reasoning_then_content_parses_to_ops() {
        // The fixed wire: thinking precedes the answer, the parser
        // reads only the answer.
        let provider = Arc::new(Canned(vec![
            Event::Reasoning {
                delta: "let me check the index".into(),
            },
            Event::Token { delta: "[]".into() },
            Event::Done,
        ]));
        let ops =
            crate::testing::block_on(run_extraction(provider, "digest", "index", &[])).unwrap();
        assert!(parse_ops(&ops).unwrap().is_empty());
    }

    #[test]
    fn evidence_is_limited_to_what_the_session_touched() {
        let mut ops = parse_ops(
            r#"[{"op":"add","file":"gotchas.md","text":"t","evidence":["src/a.rs","/etc/passwd"]}]"#,
        )
        .unwrap();
        restrict_evidence(&mut ops, &["src/a.rs".into()]);
        assert!(
            matches!(&ops[0], Op::Add { evidence, .. } if evidence == &vec!["src/a.rs".to_string()])
        );
    }
}
