//! The Tool contract. Lives in core because the loop executes tools and
//! the `agent` tool (a tool that runs a harness) is core logic.

use std::future::Future;
use std::pin::Pin;

/// A tool result: Ok(text) becomes the tool message content; Err(text) is
/// still surfaced to the model (as an error report) — the model decides
/// what to do about it. Tools only hard-fail the turn via panics/cancel.
pub type ToolResult = Result<String, String>;

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

    /// Hard policy vet, consulted *before* the permission prompt: an Err
    /// blocks the call outright — no approve button exists for it. This is
    /// for things no user should have to see a card for (fork bombs,
    /// `rm -rf /`, wiping block devices); anything debatable belongs in
    /// the prompt, not here.
    fn vet(&self, _arguments: &str) -> Result<(), String> {
        Ok(())
    }

    /// Runs the tool with the model's JSON arguments.
    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;
}
