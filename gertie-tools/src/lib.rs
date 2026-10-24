//! gertie-tools: gertie's built-in tools and MCP client.
//!
//! v1 ships nine built-ins (read_file, write_file, edit_file, bash, glob,
//! grep, web_fetch, web_search, search_history) plus the stdio MCP client,
//! all landing in M1/M2. For M0 only the tool contract exists, because the
//! agent-tool ("a tool that runs a harness") is core loop logic and its
//! shape constrains this trait.

use futures_core::stream::BoxStream;
use gertie_core::Event;

/// What a tool needs from the harness to run: its parsed arguments plus the
/// permission policy to consult before anything touches the world.
pub struct Invocation {
    /// Whole, parsed arguments — the provider layer normalized the deltas.
    pub arguments: String,
}

/// One executable capability the model can call. Tools are async and
/// cancellable, and report progress as [`Event`]s — the same contract a
/// nested subagent satisfies, which is what makes "subagents as tools"
/// a one-crate change later.
pub trait Tool {
    /// The stable name the model calls it by.
    fn name(&self) -> &'static str;

    /// What the leader reads when deciding whether to spawn it. Type and
    /// tool discovery quality is description quality.
    fn description(&self) -> String;

    /// Runs the tool. Execution must be interruptible; dropping the stream
    /// cancels it. Writes and bash report what they intend to do *before*
    /// doing it, so the UI can gate on the permission policy.
    fn run(&self, invocation: Invocation) -> BoxStream<'static, Event>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Noop;

    impl Tool for Noop {
        fn name(&self) -> &'static str {
            "noop"
        }

        fn description(&self) -> String {
            "does nothing".into()
        }

        fn run(&self, _invocation: Invocation) -> BoxStream<'static, Event> {
            Box::pin(futures_util::stream::iter(vec![Event::Done]))
        }
    }

    #[test]
    fn a_tool_reports_its_contract() {
        let tool = Noop;
        assert_eq!(tool.name(), "noop");
        assert!(!tool.description().is_empty());
    }
}
