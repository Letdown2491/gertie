//! The Tool contract. Lives in core because the loop executes tools and
//! the `agent` tool (a tool that runs a harness) is core logic.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::request::ToolCall;

/// A tool result: Ok(text) becomes the tool message content; Err(text) is
/// still surfaced to the model (as an error report), the model decides
/// what to do about it. Tools only hard-fail the turn via panics/cancel.
pub type ToolResult = Result<String, String>;

/// One step a worker is doing (or just did), the raw materials the
/// leader's UI renders through the same label builder as its own cards.
#[derive(Clone)]
pub struct WorkerStep {
    pub name: String,
    pub arguments: String,
    pub running: bool,
}

/// What a spawned worker is doing right now, keyed by the leader's
/// tool-call id. The worker's loop writes its steps as they happen
/// (inside [`Tool::run_with_call`], which is why that method exists);
/// the leader's UI reads them on its working tick, the same way it
/// watches its own stream. A leader stream parks inside the agent
/// tool while a worker runs, so without this the whole worker run
/// reads as silence. Locks are std and held across no await.
#[derive(Clone, Default)]
pub struct WorkerProgress(Arc<Mutex<HashMap<String, WorkerStep>>>);

impl WorkerProgress {
    /// The worker's current step for the leader's call.
    pub fn set(&self, call_id: &str, step: WorkerStep) {
        self.0
            .lock()
            .expect("worker progress")
            .insert(call_id.to_string(), step);
    }

    /// The leader's call is over; its worker's steps leave the map.
    pub fn clear(&self, call_id: &str) {
        self.0.lock().expect("worker progress").remove(call_id);
    }

    pub fn get(&self, call_id: &str) -> Option<WorkerStep> {
        self.0
            .lock()
            .expect("worker progress")
            .get(call_id)
            .cloned()
    }
}

/// One executable capability the model can call. Tools are async and
/// cancellable by construction (the returned future is dropped to cancel),
/// which is what makes "subagents as tools" a one-crate change later.
/// How much the tool definitions coach the model (#42), a per-model
/// profile setting. Standard: full descriptions, per-field guidance.
/// Terse: one-line descriptions and bare schemas (types, enums,
/// required kept), for models that do not need the coaching; every
/// request carries the tool definitions, so this is tokens on every
/// round. `gertie eval memory` decides which a model gets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolGuidance {
    #[default]
    Standard,
    Terse,
}

impl ToolGuidance {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "standard" => Some(Self::Standard),
            "terse" => Some(Self::Terse),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Terse => "terse",
        }
    }
}

/// The first sentence of a description: up to the first ". " or "; "
/// boundary, whole if it has none.
pub fn first_sentence(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.find(". ") {
        Some(end) => text[..=end].to_string(),
        None => text,
    }
}

/// A schema without its `description` fields, at every depth. Types,
/// enums, required lists and nesting stay: the shape is the contract,
/// the prose is coaching.
pub fn bare_schema(schema: &serde_json::Value) -> serde_json::Value {
    match schema {
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .filter(|(key, value)| !(key.as_str() == "description" && value.is_string()))
                .map(|(key, value)| (key.clone(), bare_schema(value)))
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(bare_schema).collect())
        }
        other => other.clone(),
    }
}

pub trait Tool: Send + Sync {
    /// The stable name the model calls it by.
    fn name(&self) -> &'static str;

    /// What the leader reads when deciding whether to spawn it. Type and
    /// tool discovery quality is description quality.
    fn description(&self) -> String;

    /// The description under [`ToolGuidance::Terse`] (#42): for models
    /// that need the what, not the coaching. Defaults to the first
    /// sentence; a tool whose first sentence drops a rule that matters
    /// says so here.
    fn terse_description(&self) -> String {
        first_sentence(&self.description())
    }

    /// JSON schema for the arguments object.
    fn parameters(&self) -> serde_json::Value;

    /// Whether the tool changes the world. Mutating tools go through the
    /// permission gate (prompt + checkpoint); read-only tools auto-run.
    fn mutates(&self) -> bool {
        false
    }

    /// Whether the tool owes the human a card before it runs, even
    /// though it changes nothing. The default rides `mutates()`: the
    /// gate is for mutation. A tool that reaches outward instead, a
    /// fetch whose destination the model chose, overrides this: what
    /// it moves is not a file the checkpoint can restore, it is
    /// whatever the body carries, so the prompt is the only checkpoint
    /// there is.
    fn asks(&self) -> bool {
        self.mutates()
    }

    /// Hard policy vet, consulted *before* the permission prompt: an Err
    /// blocks the call outright, no approve button exists for it. This is
    /// for things no user should have to see a card for (fork bombs,
    /// `rm -rf /`, wiping block devices); anything debatable belongs in
    /// the prompt, not here.
    fn vet(&self, _arguments: &str) -> Result<(), String> {
        Ok(())
    }

    /// Whether this one call stays inside what the workspace already
    /// shows, so the auto mode may run it unasked, as it does bash in
    /// the sandbox. The default is no; a tool says yes only for calls
    /// it can prove local (a screenshot of a workspace file or of a
    /// dev server on this machine, never the screen).
    fn contained(&self, _arguments: &str) -> bool {
        false
    }

    /// Whether the capability comes from outside gertie, an MCP server,
    /// whose path handling and side effects gertie cannot vouch for.
    /// Third-party tools face the prompt in every mode except the one
    /// where the human asked for nothing to ask (full_access): a mode
    /// that trusts gertie's own checkpoint-and-revert machinery must
    /// not silently extend that trust to code it didn't write.
    fn third_party(&self) -> bool {
        false
    }

    /// Runs the tool with the model's JSON arguments.
    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

    /// Runs the tool with the full call (id included), the leader's
    /// worker-progress handle, and the turn's routing token. Only
    /// tools that run their own loop (the agent tools) need any of
    /// it; everything else keeps the plain run.
    fn run_with_call<'a>(
        &'a self,
        call: &'a ToolCall,
        progress: &'a WorkerProgress,
        turn_token: Option<crate::TurnToken>,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        let _ = (progress, turn_token);
        self.run(&call.arguments)
    }

    /// Runs the tool and returns images beside its text: data URLs the
    /// model should see (a picture `read_file` opened, a screenshot).
    /// The loop calls this; the default adds none, so only tools that
    /// produce images override it. Images travel apart from the text
    /// on purpose: text a tool returns is never scanned for image
    /// markers, so a file that merely contains one can't make gertie
    /// upload a path of its choosing.
    fn run_with_images<'a>(
        &'a self,
        call: &'a ToolCall,
        progress: &'a WorkerProgress,
        turn_token: Option<crate::TurnToken>,
    ) -> Pin<Box<dyn Future<Output = (ToolResult, Vec<String>)> + Send + 'a>> {
        Box::pin(async move {
            (
                self.run_with_call(call, progress, turn_token).await,
                Vec::new(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #42: terse keeps the contract (types, enums, required, nesting)
    /// and drops the coaching, and a description cuts at its first
    /// sentence.
    #[test]
    fn terse_keeps_the_schema_shape_and_the_first_sentence() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "supersede"], "description": "what to do"},
                "evidence": {"type": "array", "items": {"type": "string", "description": "a path"}},
                "description": {"type": "string", "description": "a field named description stays"}
            },
            "required": ["action"]
        });
        let bare = bare_schema(&schema);
        assert_eq!(bare["properties"]["action"]["enum"][1], "supersede");
        assert!(bare["properties"]["action"].get("description").is_none());
        assert!(
            bare["properties"]["evidence"]["items"]
                .get("description")
                .is_none()
        );
        assert_eq!(bare["required"][0], "action");
        assert_eq!(
            bare["properties"]["description"]["type"], "string",
            "a property named description is data"
        );
        assert_eq!(
            first_sentence("Read a file.  Returns numbered lines. Use offset."),
            "Read a file."
        );
        assert_eq!(first_sentence("no boundary here"), "no boundary here");
    }
}
