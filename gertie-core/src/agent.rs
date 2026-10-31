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

use crate::SessionId;
use crate::SessionScope;
use crate::SessionStore;
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

/// The registry shipped with gertie: one worker per distinct *posture*
/// (what it may touch, what it answers for), not per vibe. A config
/// `[[agents]]` entry with the same name replaces the built-in; extra
/// entries are added on top.
pub fn builtin_agent_types() -> Vec<AgentType> {
    const READ: &[&str] = &["read_file", "glob", "grep", "search_history", "load_skill"];
    vec![
        AgentType {
            name: "explore".into(),
            description: "Use when a question needs broad reading — where is X, how does Y \
                          work, what calls Z. Read-only; returns findings with file paths."
                .into(),
            system_prompt: "You are an exploration agent. Answer the brief by reading the \
                            codebase — never by guessing. Cite file paths for every claim. \
                            Say plainly what you could not find."
                .into(),
            tools: Some(
                READ.iter()
                    .copied()
                    .chain(["web_fetch"])
                    .map(String::from)
                    .collect(),
            ),
        },
        AgentType {
            name: "plan".into(),
            description: "Use to turn a fuzzy goal into a concrete step-by-step plan before \
                          any code changes. Read-only; returns steps with rationale."
                .into(),
            system_prompt: "You are a planning agent. Read enough of the codebase to ground \
                            the plan in reality, then return ordered steps: what, where \
                            (file), why, and what could go wrong. You never touch code — \
                            your only output is the plan."
                .into(),
            tools: Some(READ.iter().copied().map(String::from).collect()),
        },
        AgentType {
            name: "implement".into(),
            description: "Use for delegated code changes within a defined scope: implement \
                          the brief exactly, run the relevant checks, report what changed."
                .into(),
            system_prompt: "You are an implementation agent. Do exactly what the brief asks \
                            — no scope creep. Match surrounding code style. Verify with the \
                            project's own checks where possible. Report what changed and \
                            how you verified it."
                .into(),
            tools: None,
        },
        AgentType {
            name: "test".into(),
            description: "Use to run test suites, reproduce failures, and verify behavior. \
                          Reports findings; does not fix code."
                .into(),
            system_prompt: "You are a testing agent. Reproduce, run, and observe — you do \
                            not fix code. Report exact commands, exact output, and your \
                            verdict on whether the behavior matches the brief."
                .into(),
            tools: Some(
                ["bash", "read_file", "glob", "grep", "load_skill"]
                    .iter()
                    .copied()
                    .map(String::from)
                    .collect(),
            ),
        },
        AgentType {
            name: "review".into(),
            description: "Use as a critique gate on written code: correctness first, then \
                          security, then clarity. Read-only; every finding cites a location."
                .into(),
            system_prompt: "You are a review agent. Judge correctness first, then security, \
                            then clarity. Every finding cites a file and line. Verify claims \
                            by reading, never by assuming. Say what is good, too — briefly."
                .into(),
            tools: Some(READ.iter().copied().map(String::from).collect()),
        },
        AgentType {
            name: "debug".into(),
            description: "Use for root-cause diagnosis: reproduce, instrument, narrow down. \
                          Returns evidence and a diagnosis, not patches."
                .into(),
            system_prompt: "You are a diagnosis agent. Reproduce the failure, gather \
                            evidence, narrow the cause. You may run commands to observe, but \
                            your deliverable is a diagnosis with evidence — not a patch."
                .into(),
            tools: Some(
                ["bash", "read_file", "glob", "grep", "search_history"]
                    .iter()
                    .copied()
                    .map(String::from)
                    .collect(),
            ),
        },
        AgentType {
            name: "research".into(),
            description: "Use for web research: current docs, releases, issue threads. \
                          Returns findings with URLs."
                .into(),
            system_prompt: "You are a research agent. Use web_fetch and web_search; prefer \
                            primary sources. Every finding carries its URL. Flag anything \
                            you could not verify."
                .into(),
            tools: Some(
                ["web_fetch", "web_search", "read_file", "glob", "grep"]
                    .iter()
                    .copied()
                    .map(String::from)
                    .collect(),
            ),
        },
        AgentType {
            name: "librarian".into(),
            description: "Use to mine past gertie sessions — what we did before, what was \
                          decided, what broke last time. Returns hits with session titles."
                .into(),
            system_prompt: "You are a memory agent. Search past sessions with search_history \
                            and read the current workspace to connect the dots. Report what \
                            was found, in which session, and what remains unknown."
                .into(),
            tools: Some(
                ["search_history", "read_file", "glob", "grep"]
                    .iter()
                    .copied()
                    .map(String::from)
                    .collect(),
            ),
        },
    ]
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
    /// Where worker runs persist as child sessions. None (tests,
    /// store-less boots) means the run is transient.
    store: Option<Arc<SessionStore>>,
    /// The front's current session, read at dispatch to link the run.
    scope: SessionScope,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent: AgentType,
        provider: Arc<dyn crate::Provider>,
        all_tools: &ToolRegistry,
        snapshots: Option<Arc<SnapshotStore>>,
        broker: Arc<PermissionBroker>,
        context_window: Option<u64>,
        workspace_root: PathBuf,
        store: Option<Arc<SessionStore>>,
        scope: SessionScope,
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
            store,
            scope,
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

    /// The agent type's name, for run titles: `agent_explore` → `explore`.
    fn worker_label(&self) -> String {
        self.tool_name
            .strip_prefix("agent_")
            .unwrap_or(self.tool_name)
            .to_string()
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

            // The run persists as a child session of the dispatching
            // chat — inspectable in the agents panel, searchable like
            // any message. A persistence failure never blocks the run;
            // it just stays unrecorded.
            let run: Option<SessionId> = self.store.as_ref().and_then(|store| {
                let brief_name = std::path::Path::new(&args.brief_path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(&args.brief_path);
                let title = format!("⟡ {} · {}", self.worker_label(), brief_name);
                match store.create_worker_run(self.scope.get(), &title) {
                    Ok(session) => {
                        let _ = store.append_message(
                            session,
                            None,
                            crate::Role::User,
                            &format!(
                                "Brief {}{}\n\n---\n{brief}\n---",
                                args.brief_path,
                                if guidance.is_empty() {
                                    String::new()
                                } else {
                                    format!("\n\nGuidance: {guidance}")
                                }
                            ),
                        );
                        Some(session)
                    }
                    Err(_) => None,
                }
            });

            let history: SharedHistory = Arc::new(tokio::sync::Mutex::new(Vec::new()));
            let stream = run_turn(TurnSpec {
                provider: self.provider.clone(),
                tools: self.worker_tools.clone(),
                history: history.clone(),
                // Workers don't take steering — the brief is the job.
                steer: None,
                user_attachments: Vec::new(),
                user_message: Some(format!(
                    "A brief file has been written for you:\n\n---\n{brief}\n---\n\n\
                     {guidance}\n\n\
                     Execute the brief with your tools. Verify your work. \
                     Finish with a report:\n\
                     STATUS: done | blocked\n\
                     EVIDENCE: what you did (commands, files, results)\n\
                     SUMMARY: one line\n\
                     CONCERNS: anything the leader should know"
                )),
                snapshots: self.snapshots.clone(),
                checkpoint_session: run,
                workspace_root: Some(self.workspace_root.clone()),
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
                if let (Some(store), Some(session)) = (&self.store, run) {
                    let _ = store.append_message(
                        session,
                        None,
                        crate::Role::Assistant,
                        &format!("worker failed: {message}"),
                    );
                }
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
                Some(text) if !text.trim().is_empty() => {
                    if let (Some(store), Some(session)) = (&self.store, run) {
                        let _ = store.append_message(session, None, crate::Role::Assistant, &text);
                    }
                    Ok(text)
                }
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
            None,
            crate::SessionScope::new(),
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

    #[test]
    fn a_worker_run_persists_as_a_child_session() {
        struct ReportProvider;
        impl crate::Provider for ReportProvider {
            fn stream(&self, _request: crate::Request) -> BoxStream<'static, crate::Event> {
                Box::pin(futures_util::stream::iter(vec![
                    crate::Event::Token {
                        delta: "STATUS: done".into(),
                    },
                    crate::Event::Done,
                ]))
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("brief.md"), "Do the thing.").unwrap();

        let store = Arc::new(crate::SessionStore::open(std::path::Path::new(":memory:")).unwrap());
        let parent = store.create_session("the chat").unwrap();
        let scope = crate::SessionScope::new();
        scope.set(Some(parent));

        let broker = Arc::new(PermissionBroker::new());
        let tool = AgentTool::new(
            AgentType {
                name: "explore".into(),
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
            Some(store.clone()),
            scope,
        );

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
        result.expect("worker report");

        // The run is a child session: linked to the dispatching chat,
        // hidden from the sidebar, holding brief and report.
        let children = store.list_child_sessions(parent).unwrap();
        assert_eq!(children.len(), 1);
        assert!(children[0].title.starts_with("⟡ explore"));
        let messages = store.load_messages(children[0].id).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, crate::Role::User);
        assert!(messages[0].content.contains("Do the thing."));
        assert_eq!(messages[1].role, crate::Role::Assistant);
        assert!(messages[1].content.contains("STATUS: done"));
        // The sidebar is chats only — worker runs live in the panel.
        assert_eq!(store.list_sessions().unwrap().len(), 1);
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
    fn builtin_types_have_unique_names_and_safe_allowlists() {
        let builtins = builtin_agent_types();
        assert!(
            builtins.len() >= 6,
            "registry collapsed: {}",
            builtins.len()
        );
        let mut names: Vec<&str> = builtins.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        let unique = names.len() == names.iter().collect::<std::collections::HashSet<_>>().len();
        assert!(unique, "duplicate built-in names: {names:?}");
        for t in &builtins {
            // "implement" is the only full-toolset type; everything else
            // is allowlisted, and no allowlist may reach agent_* tools —
            // though depth-1 stripping makes that unreachable anyway.
            if let Some(allow) = &t.tools {
                assert!(
                    !allow.iter().any(|n| n.starts_with("agent_")),
                    "{} allowlists an agent tool",
                    t.name
                );
            } else {
                assert_eq!(t.name, "implement", "only implement ships unlisted");
            }
            assert!(
                !t.description.trim().is_empty(),
                "{}: no description — the leader chooses by description",
                t.name
            );
            assert!(
                !t.system_prompt.trim().is_empty(),
                "{}: empty prompt",
                t.name
            );
        }
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
            None,
            crate::SessionScope::new(),
        );
        // Path outside the root: canonicalize keeps it outside — rejected.
        assert!(tool.read_brief("../../etc/passwd").is_err());
    }
}
