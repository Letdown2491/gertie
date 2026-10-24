//! The harness-level request shape. Vendor formats are adapters in
//! gertie-providers; this is the normalized form the loop speaks.

use crate::Role;

/// A tool definition advertised to the model.
#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON schema for the parameters object.
    pub parameters: serde_json::Value,
}

/// A tool call the model wants executed, whole — deltas never leave the
/// provider layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// One message in the canonical conversation. Tool-capable conversations
/// need three shapes the old (Role, String) pair couldn't express:
/// assistant-with-tool-calls, tool-results (keyed by call id), and plain
/// turns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WireMessage {
    pub role: Role,
    pub content: Option<String>,
    /// Assistant-only: calls the model made in this turn.
    pub tool_calls: Vec<ToolCall>,
    /// Tool-only: which call this result answers.
    pub tool_call_id: Option<String>,
    /// Tool-only: the tool's name.
    pub name: Option<String>,
}

impl WireMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            ..Self::default()
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Some(content.into()),
            ..Self::default()
        }
    }

    pub fn tool_result(call: &ToolCall, result: &str) -> Self {
        Self {
            role: Role::Tool,
            content: Some(result.to_string()),
            tool_call_id: Some(call.id.clone()),
            name: Some(call.name.clone()),
            ..Self::default()
        }
    }

    pub fn assistant_with_tool_calls(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls,
            ..Self::default()
        }
    }
}

/// One model request. The loop fills this; providers translate it.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// Canonical message path, oldest first.
    pub messages: Vec<WireMessage>,
    /// Tools the model may call this turn.
    pub tools: Vec<ToolSpec>,
    /// Maximum output tokens the model should produce, when the provider
    /// supports the knob.
    pub max_output_tokens: Option<u64>,
}
