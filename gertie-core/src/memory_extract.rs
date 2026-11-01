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
            _ => continue, // tool rows are noise for extraction
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

/// The extraction prompt: small, flat, ACE-shaped. The schema is the
/// ops JSON the store already understands; counters and ids stay out
/// of the model's hands (the merge assigns them).
pub fn extraction_prompt(digest: &str, index: &str) -> String {
    format!(
        "You are extracting durable memory from a coding-agent session. \
Propose changes to a long-term memory store as a JSON array of ops. \
Ops:\n\
- {{\"op\":\"add\",\"file\":\"<topic-file>.md\",\"text\":\"<one-line entry>\",\"source\":\"<session ref>\"}}\n\
- {{\"op\":\"update\",\"id\":\"<entry id>\",\"text\":\"<one-line replacement>\"}}\n\
- {{\"op\":\"supersede\",\"id\":\"<entry id>\"}}\n\
- {{\"op\":\"archive\",\"id\":\"<entry id>\"}}\n\
- {{\"op\":\"link\",\"from\":\"<id>\",\"to\":\"<id>\",\"relation\":\"relates_to|solves|causes|builds_on|replaces\"}}\n\n\
Rules: one line per entry, under 160 chars. Facts and preferences only, \
never task narration. Supersede an existing entry when the new fact \
replaces it. Existing memory (do not repeat it):\n{index}\n\n\
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
            },
            "update" => Op::Update {
                id: text_field("id")?,
                text: text_field("text")?,
            },
            "supersede" => Op::Supersede {
                id: text_field("id")?,
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
) -> Result<String, String> {
    let prompt = extraction_prompt(digest, index);
    let request = Request {
        messages: vec![crate::request::WireMessage::user(&prompt)],
        tools: Vec::new(),
        max_output_tokens: Some(512),
    };
    let mut stream = provider.stream(request);
    let mut collected = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Event::Token { delta } => collected.push_str(&delta),
            Event::Error { message } => return Err(message),
            Event::Limited { message } => return Err(format!("rate limited: {message}")),
            Event::Done => break,
            _ => {}
        }
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
        assert!(matches!(ops[2], Op::Supersede { ref id } if id == "p7"));
        assert!(matches!(ops[4], Op::Link { ref relation, .. } if relation == "solves"));
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
        let prompt = extraction_prompt("digest text", "existing memory");
        assert!(prompt.contains("\"op\":\"add\""));
        assert!(prompt.contains("existing memory"));
    }
}
