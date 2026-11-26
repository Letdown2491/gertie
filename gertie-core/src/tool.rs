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
pub trait Tool: Send + Sync {
    /// The stable name the model calls it by.
    fn name(&self) -> &'static str;

    /// What the leader reads when deciding whether to spawn it. Type and
    /// tool discovery quality is description quality.
    fn description(&self) -> String;

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

    /// Runs the tool with the full call (id included) and the leader's
    /// worker-progress handle. Only tools that run their own loop (the
    /// agent tools) need either; everything else keeps the plain run.
    fn run_with_call<'a>(
        &'a self,
        call: &'a ToolCall,
        progress: &'a WorkerProgress,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        let _ = progress;
        self.run(&call.arguments)
    }
}
