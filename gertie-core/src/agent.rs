//! Subagents: a tool that runs a whole other harness loop — the
//! leader/worker pattern, depth 1.
//!
//! The mechanism is "subagents as tools": an agent type from config
//! becomes an `agent_{name}` tool in the leader's registry. Spawning a
//! worker is a tool call (so it prompts, like any mutating action); the
//! worker runs its own `run_turn` with its own history, system prompt,
//! tool registry, and possibly its own model; its final message is the
//! tool result the leader sees.
//!
//! Depth-1 is structural, not prompted: `worker_registry` filters out
//! every `agent_*` tool, so a worker has nothing to recurse with no
//! matter what its brief says.
//!
//! Dispatch is by **brief file** (a path under the workspace root): the
//! leader writes a self-contained brief — task, constraints, acceptance
//! criteria — and dispatches the path. Workers never see leader history;
//! the brief is the whole world.

use crate::ToolRegistry;
use crate::agent_loop::{LoopEvent, SharedHistory, TurnSpec, run_turn};
use crate::permissions::{PermissionBroker, PromptSource};
use crate::snapshot::SnapshotStore;
use crate::tool::{Tool, ToolResult};
use futures_util::StreamExt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

/// One agent type, TOML-shaped in config, plain data here.
#[derive(Clone, Debug)]
pub struct AgentType {
    /// Slug; the tool is `agent_{name}`.
    pub name: String,
    /// Triggers only ("Use when…"), never a workflow summary. Context
    /// load paid on every leader turn — kept short.
    pub description: String,
    /// The worker's system prompt.
    pub system_prompt: String,
    /// Tool names the worker may use; None inherits everything (minus
    /// agent tools — always).
    pub tools: Option<Vec<String>>,
}

/// Untrusted-text hygiene, shared: descriptions from config, MCP
/// servers, or skills never carry control characters or unbounded
/// length into the model-visible surface.
pub fn sanitize_untrusted(raw: &str, cap: usize) -> String {
    let filtered: String = raw
        .chars()
        .filter(|c| !(c.is_control() && !c.is_whitespace()))
        .collect();
    filtered.chars().take(cap).collect()
}

/// The worker's registry: allowlist applied, agent tools stripped —
/// depth-1 enforced by what the map contains, not by prompt discipline.
pub fn worker_registry(all: &ToolRegistry, allowlist: Option<&[String]>) -> ToolRegistry {
    let filtered = all
        .iter()
        .filter(|(name, _)| {
            !name.starts_with("agent_")
                && allowlist.is_none_or(|allowed| allowed.iter().any(|a| a == *name))
        })
        .map(|(name, tool)| (name.clone(), tool.clone()))
        .collect();
    Arc::new(filtered)
}

/// The tool the leader calls to spawn a worker of one type.
pub struct AgentTool {
    /// Leaked at construction: `Tool::name` is `&'static str`, one
    /// allocation per configured agent type, bounded by config.
    tool_name: &'static str,
    description: String,
    system_prompt: String,
    provider: Arc<dyn crate::Provider>,
    worker_tools: ToolRegistry,
    snapshots: Option<Arc<SnapshotStore>>,
    broker: Arc<PermissionBroker>,
    context_window: Option<u64>,
    /// Brief files resolve here, sandboxed like every other path.
    workspace_root: PathBuf,
}

#[derive(serde::Deserialize)]
struct DispatchArgs {
    /// Path to the brief file, workspace-relative.
    brief_path: String,
    /// Optional extra guidance beyond the brief itself.
    guidance: Option<String>,
}

const MAX_DESCRIPTION: usize = 1024;

impl AgentTool {
    pub fn new(
        agent: AgentType,
        provider: Arc<dyn crate::Provider>,
        all_tools: &ToolRegistry,
        snapshots: Option<Arc<SnapshotStore>>,
        broker: Arc<PermissionBroker>,
        context_window: Option<u64>,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            tool_name: Box::leak(
                format!("agent_{}", agent.name.replace(['-', ' '], "_")).into_boxed_str(),
            ),
            description: sanitize_untrusted(&agent.description, MAX_DESCRIPTION),
            system_prompt: agent.system_prompt,
            provider,
            worker_tools: worker_registry(all_tools, agent.tools.as_deref()),
            snapshots,
            broker,
            context_window,
            workspace_root,
        }
    }

    /// Reads the brief under the workspace root — the same escape rule
    /// as every other path tool.
    fn read_brief(&self, brief_path: &str) -> Result<String, String> {
        let target = self.workspace_root.join(brief_path);
        let canonical = target
            .canonicalize()
            .map_err(|error| format!("brief {brief_path}: {error}"))?;
        if !canonical.starts_with(&self.workspace_root) {
            return Err(format!("brief {brief_path}: escapes the workspace"));
        }
        std::fs::read_to_string(&canonical).map_err(|error| format!("brief {brief_path}: {error}"))
    }
}

impl Tool for AgentTool {
    fn name(&self) -> &'static str {
        self.tool_name
    }

    fn description(&self) -> String {
        format!(
            "[agent] Spawn a '{}' subagent with its own tools and context. \
             Write a self-contained brief file (task, constraints, \
             acceptance criteria), pass its workspace path as brief_path. \
             The worker never sees this conversation. Use when: {}",
            self.tool_name.strip_prefix("agent_").unwrap_or(""),
            self.description
        )
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "brief_path": {"type": "string", "description": "workspace path of the brief file"},
                "guidance": {"type": "string", "description": "optional extra guidance beyond the brief"}
            },
            "required": ["brief_path"]
        })
    }

    fn mutates(&self) -> bool {
        // A worker runs tools; spawning one is a mutating action and
        // prompts like one.
        true
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        Box::pin(async move {
            let args: DispatchArgs = serde_json::from_str(arguments)
                .map_err(|error| format!("bad dispatch arguments: {error}"))?;
            let brief = self.read_brief(&args.brief_path)?;
            let guidance = args.guidance.unwrap_or_default();

            let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            let stream = run_turn(TurnSpec {
                provider: self.provider.clone(),
                tools: self.worker_tools.clone(),
                history: history.clone(),
                user_message: format!(
                    "A brief file has been written for you:\n\n---\n{brief}\n---\n\n\
                     {guidance}\n\n\
                     Execute the brief with your tools. Verify your work. \
                     Finish with a report:\n\
                     STATUS: done | blocked\n\
                     EVIDENCE: what you did (commands, files, results)\n\
                     SUMMARY: one line\n\
                     CONCERNS: anything the leader should know"
                ),
                snapshots: self.snapshots.clone(),
                context_window: self.context_window,
                permission_broker: Some(self.broker.clone()),
                prompt_source: PromptSource::Worker,
                system_prompt: Some(self.system_prompt.clone()),
            });

            // Drive the worker turn; its asks are already in the shared
            // broker, so the UI can approve them. Everything else here
            // is bookkeeping.
            let mut failure: Option<String> = None;
            let mut stream = stream;
            while let Some(event) = stream.next().await {
                match event {
                    LoopEvent::Error { message } => {
                        failure = Some(message);
                        break;
                    }
                    LoopEvent::Done => break,
                    _ => {}
                }
            }
            if let Some(message) = failure {
                return Err(format!("worker failed: {message}"));
            }

            // The report is the worker's last assistant message.
            let report = history
                .lock()
                .await
                .iter()
                .rev()
                .find(|message| message.role == crate::Role::Assistant)
                .and_then(|message| message.content.clone());
            match report {
                Some(text) if !text.trim().is_empty() => Ok(text),
                _ => Err("worker finished without a report".to_string()),
            }
        })
    }
}

/// Tests need no provider; the tool call type is re-exported for
/// signature compatibility only.
#[cfg(test)]
mod tests {
    use super::*;
    use futures_core::stream::BoxStream;
    use std::task::{Context as TaskContext, Poll, Waker};
    use std::time::Duration;

    #[test]
    fn a_worker_turn_reports_its_final_message() {
        // The worker's provider: one round, final report, no tools.
        struct ReportProvider;
        impl crate::Provider for ReportProvider {
            fn stream(&self, _request: crate::Request) -> BoxStream<'static, crate::Event> {
                Box::pin(futures_util::stream::iter(vec![
                    crate::Event::Token {
                        delta:
                            "STATUS: done\nEVIDENCE: ran the check\nSUMMARY: fine\nCONCERNS: none"
                                .into(),
                    },
                    crate::Event::Done,
                ]))
            }
        }

        // A brief in a real workspace tempdir.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("brief.md"), "Do the thing.").unwrap();

        let broker = Arc::new(PermissionBroker::new());
        let tool = AgentTool::new(
            AgentType {
                name: "worker".into(),
                description: "does the thing".into(),
                system_prompt: "work hard".into(),
                tools: None,
            },
            Arc::new(ReportProvider),
            &worker_registry(&Arc::new(Default::default()), None),
            None,
            broker,
            None,
            tmp.path().to_path_buf(),
        );

        // Poll to completion — no runtime needed for these futures.
        let future = tool.run(r#"{"brief_path": "brief.md"}"#);
        let waker = Waker::noop();
        let mut cx = TaskContext::from_waker(waker);
        let mut future = Box::pin(future);
        let result = loop {
            match Pin::new(&mut future).poll(&mut cx) {
                Poll::Ready(result) => break result,
                Poll::Pending => std::thread::sleep(Duration::from_millis(10)),
            }
        };

        let report = result.expect("worker report");
        assert!(report.contains("STATUS: done"));
        assert!(report.contains("SUMMARY: fine"));
    }

    fn registry_with(agent_tools: usize) -> ToolRegistry {
        use crate::tool::Tool;
        let mut map = std::collections::BTreeMap::new();
        for name in ["read_file", "write_file", "bash"] {
            map.insert(
                name.to_string(),
                Arc::new(NoopTool { name }) as Arc<dyn Tool>,
            );
        }
        for index in 0..agent_tools {
            map.insert(
                format!("agent_worker{index}"),
                Arc::new(NoopTool { name: "agent" }),
            );
        }
        Arc::new(map)
    }

    struct NoopTool {
        name: &'static str,
    }

    impl Tool for NoopTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> String {
            "noop".into()
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        fn run<'a>(
            &'a self,
            _arguments: &'a str,
        ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    #[test]
    fn depth_one_is_structural_agent_tools_are_stripped() {
        let all = registry_with(2);
        let worker = worker_registry(&all, None);
        assert!(!worker.contains_key("agent_worker0"));
        assert!(!worker.contains_key("agent_worker1"));
        assert!(worker.contains_key("read_file"));
        assert!(worker.contains_key("bash"));
    }

    #[test]
    fn allowlists_narrow_the_worker_registry() {
        let all = registry_with(0);
        let worker = worker_registry(&all, Some(&["read_file".into()]));
        assert!(worker.contains_key("read_file"));
        assert!(!worker.contains_key("write_file"));
        assert!(!worker.contains_key("bash"));
    }

    #[test]
    fn briefs_may_not_escape_the_workspace() {
        struct NoopProvider;
        impl crate::Provider for NoopProvider {
            fn stream(
                &self,
                _request: crate::Request,
            ) -> futures_core::stream::BoxStream<'static, crate::Event> {
                Box::pin(futures_util::stream::iter(vec![crate::Event::Done]))
            }
        }
        let tool = AgentTool::new(
            AgentType {
                name: "test".into(),
                description: "test".into(),
                system_prompt: String::new(),
                tools: None,
            },
            Arc::new(NoopProvider),
            &registry_with(0),
            None,
            Arc::new(PermissionBroker::new()),
            None,
            PathBuf::from("/tmp"),
        );
        // Path outside the root: canonicalize keeps it outside — rejected.
        assert!(tool.read_brief("../../etc/passwd").is_err());
    }
}
