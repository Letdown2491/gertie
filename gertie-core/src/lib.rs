//! gertie-core: the headless agent harness.
//!
//! This crate is the whole harness minus the UI. It must never depend on a
//! GUI framework: it is embeddable from a CLI, a server, or an editor. The
//! UI-facing `gertie` crate is one client of many.
//!
//! M0 ships only the foundational types the rest of the design hangs off:
//! the tree-shaped message model and the typed provider event stream. The
//! agent loop, permissions, checkpoints, and the `agent` tool land in M1/M2.

/// Identifies one message in a session's message tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageId(pub u64);

/// Who authored a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    /// Output produced by a tool invocation.
    Tool,
}

/// One node in a session's message tree. Sessions are never flat lists:
/// every message knows its parent, so forking, "retry from here", and
/// subagent transcripts-as-branches need no migration later.
///
/// A message's *canonical* rendering is the path from its node to the root.
#[derive(Clone, Debug)]
pub struct Message {
    pub id: MessageId,
    pub parent_id: Option<MessageId>,
    pub role: Role,
    pub content: String,
}

/// A single event on a provider's response stream. This enum is gertie's
/// stable internal contract: every provider adapts its wire format to it,
/// and neither the agent loop nor the UI ever sees vendor-shaped deltas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The provider accepted the request and is about to stream.
    Started,
    /// The next piece of assistant text. Providers coalesce to whatever
    /// granularity arrives over the wire; the loop may re-coalesce.
    Token { delta: String },
    /// A tool call the model wants executed. Deltas are normalized inside
    /// the provider layer, so this arrives whole, with arguments parsed.
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// Final usage accounting for the request.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// The stream finished normally.
    Done,
    /// The stream failed. Retries are the provider layer's job; by the time
    /// an `Error` surfaces, the policy in DESIGN.md has been exhausted.
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_form_a_tree() {
        let root = Message {
            id: MessageId(1),
            parent_id: None,
            role: Role::User,
            content: "hi".into(),
        };
        let fork = Message {
            id: MessageId(2),
            parent_id: Some(root.id),
            role: Role::Assistant,
            content: "hello".into(),
        };
        let retry = Message {
            id: MessageId(3),
            parent_id: Some(root.id),
            role: Role::Assistant,
            content: "hey".into(),
        };
        assert_eq!(fork.parent_id, retry.parent_id);
        assert_ne!(fork.id, retry.id);
    }
}
