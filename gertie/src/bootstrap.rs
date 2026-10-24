//! Shared bootstrap for both fronts — the desktop app and `gertie run`:
//! config, session store, provider, the tool registry (built-ins,
//! skills, web, MCP, agent types), broker, snapshots. Everything that
//! must exist identically no matter which surface you talk to.

use gertie_core::Tool as _;
use gertie_providers::OpenAICompatible;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Bootstrap {
    pub config: crate::config::Config,
    /// Config load/parse failure, if any. The GUI reports it in the
    /// status label (a GUI user never sees stderr); headless treats it
    /// as fatal.
    pub config_error: Option<String>,
    pub store: Arc<gertie_core::SessionStore>,
    pub provider: Option<Arc<OpenAICompatible>>,
    /// The raw endpoint, kept for agent-type model overrides.
    pub agent_endpoint: Option<gertie_providers::Endpoint>,
    pub tools: gertie_core::ToolRegistry,
    pub broker: Arc<gertie_core::PermissionBroker>,
    pub snapshots: Option<Arc<gertie_core::SnapshotStore>>,
    pub system_prompt: Option<String>,
    /// Human-readable status line: endpoint, warnings, capability counts.
    pub label: String,
    pub workspace_root: PathBuf,
}

impl Bootstrap {
    /// Builds everything both fronts share. Never exits on its own
    /// except for the one non-negotiable (no session store): callers
    /// decide what their surface does with warnings.
    pub fn load(config_path: Option<&Path>) -> Self {
        // A config that fails to parse is reported, not silent.
        let loaded = match config_path {
            Some(path) => crate::config::Config::load_from(path).map(Some),
            None => crate::config::Config::load().map(Some),
        };
        let (config, config_error) = match loaded {
            Ok(Some(config)) => (config, None),
            Ok(None) => (crate::config::Config::default(), None),
            Err(error) => (crate::config::Config::default(), Some(error)),
        };
        let endpoint = config.selected_endpoint();
        let mut label = if let Some(error) = &config_error {
            format!("config error — {error}")
        } else {
            endpoint
                .as_ref()
                .map(|endpoint| format!("{} · {}", endpoint.name, endpoint.model))
                .unwrap_or_else(|| "no endpoint configured".to_string())
        };
        for warning in config.key_warnings() {
            label.push_str(&format!(" — ⚠ {warning}"));
        }

        let store = match crate::config::Config::data_dir() {
            Some(data_dir) => {
                if let Err(error) = std::fs::create_dir_all(&data_dir) {
                    eprintln!("warning: could not create {}: {error}", data_dir.display());
                    None
                } else {
                    // Conversation history is private: keep the directory
                    // to the owner regardless of umask.
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(
                            &data_dir,
                            std::fs::Permissions::from_mode(0o700),
                        );
                    }
                    gertie_core::SessionStore::open(&data_dir.join("sessions.db"))
                        .map(Arc::new)
                        .map_err(|error| {
                            eprintln!("warning: could not open session store: {error}")
                        })
                        .ok()
                }
            }
            None => None,
        };
        let Some(store) = store else {
            eprintln!("error: no session store; refusing to run without persistence");
            std::process::exit(1);
        };

        // The workspace is wherever gertie was launched; tools resolve
        // paths against it and checkpoints land beside the session store.
        let workspace_root = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let snapshots = crate::config::Config::data_dir().and_then(|data_dir| {
            match gertie_core::SnapshotStore::open(&data_dir.join("snapshots")) {
                Ok(store) => Some(Arc::new(store)),
                Err(error) => {
                    eprintln!("warning: checkpoint store disabled: {error}");
                    None
                }
            }
        });
        // Sandbox: opt-in Landlock isolation for bash children.
        let sandbox = config.security.sandbox_bash.then(|| {
            std::sync::Arc::new(gertie_tools::sandbox::Sandbox::new(workspace_root.clone()))
        });
        let mut tools = gertie_tools::builtin_tools_with(&workspace_root, sandbox);

        // Skills: user dir + project dir (project overrides by name),
        // loaded on demand through load_skill; only the index rides the
        // system prompt. Discovery problems are warnings, never a
        // failed launch. With `--config`, skills live beside that file.
        let skills_user = config_path
            .and_then(|path| path.parent().map(|parent| parent.join("skills")))
            .or_else(|| {
                crate::config::Config::path().map(|path| path.parent().unwrap().join("skills"))
            })
            .unwrap_or_else(|| PathBuf::from(".gertie-skills"));
        let skills_project = workspace_root.join(".gertie/skills");
        let (skills, skill_warnings) =
            gertie_tools::skills::discover(&skills_user, &skills_project);
        if !skills.is_empty() {
            let count = skills.len();
            Arc::get_mut(&mut tools)
                .expect("registry not yet shared")
                .insert(
                    "load_skill".into(),
                    Arc::new(gertie_tools::skills::LoadSkill::new(&skills)),
                );
            label.push_str(&format!(" · skills×{count}"));
        }
        for warning in &skill_warnings {
            label.push_str(&format!(" — ⚠ {warning}"));
        }

        // The system prompt: identity, the one policy paragraph the model
        // must know, the skill index. Terse by the two-loads rule — every
        // line is context paid on every turn.
        let system_prompt = {
            let mut prompt = format!(
                "You are gertie, a coding agent working in {}.\n\
                 Read-only tools run without asking; anything that changes the \
                 world shows the user a card and waits. Write code, run the \
                 tests, report plainly.",
                workspace_root.display()
            );
            let index = gertie_tools::skills::index_prompt(&skills);
            if !index.is_empty() {
                prompt.push_str("\n\n");
                prompt.push_str(&index);
            }
            Some(prompt)
        };

        // Web tools: fetch always; search gracefully-disabled unless a
        // backend resolves; history search rides the session store.
        let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
        map.insert("web_fetch".into(), Arc::new(gertie_tools::web::WebFetch));
        let search_tool = match &config.search {
            Some(search) => match search.to_backend() {
                Ok(backend) => backend,
                Err(error) => {
                    label.push_str(&format!(" — ⚠ search: {error}"));
                    gertie_tools::web::WebSearch::unconfigured()
                }
            },
            None => gertie_tools::web::WebSearch::unconfigured(),
        };
        map.insert("web_search".into(), Arc::new(search_tool));
        map.insert(
            "search_history".into(),
            Arc::new(gertie_tools::web::SearchHistory::new(store.clone())),
        );

        // MCP servers connect at startup; a server that won't start is a
        // status-line warning, never a failed launch — gertie's local
        // tools carry the session alone if needed.
        if !config.mcp_servers.is_empty() {
            let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
            for server in &config.mcp_servers {
                match gertie_tools::mcp::McpClient::connect(
                    &server.name,
                    &server.command,
                    &server.args,
                    &server
                        .env
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect::<Vec<_>>(),
                ) {
                    Ok(client) => match Arc::new(client).list_tools() {
                        Ok(mcp_tools) => {
                            let count = mcp_tools.len();
                            for tool in mcp_tools {
                                map.insert(tool.name().to_string(), Arc::new(tool));
                            }
                            label.push_str(&format!(" · mcp:{}×{count}", server.name));
                        }
                        Err(error) => label.push_str(&format!(" — ⚠ mcp {}: {error}", server.name)),
                    },
                    Err(error) => label.push_str(&format!(" — ⚠ {error}")),
                }
            }
        }

        // Constructing the provider spins up its tokio runtime; do it
        // once. The endpoint is kept for agent-type model overrides.
        let (provider, agent_endpoint) = match endpoint {
            Some(endpoint) => {
                let for_agents = endpoint.clone();
                (
                    OpenAICompatible::new(endpoint)
                        .map(Arc::new)
                        .map_err(|error| eprintln!("warning: endpoint setup failed: {error}"))
                        .ok(),
                    Some(for_agents),
                )
            }
            None => (None, None),
        };

        let broker = Arc::new(gertie_core::PermissionBroker::new());

        // Agent types: each model-invoked type becomes an agent_{name}
        // tool. Construction order is safe either way — worker registries
        // strip every agent_* tool, so workers can never see each other.
        if let (Some(endpoint), Some(leader_provider)) = (&agent_endpoint, &provider) {
            // The shipped registry first; a config `[[agents]]` entry with
            // a matching name replaces the built-in (and may pin its own
            // model), extras are added on top. Built-ins inherit the
            // leader model.
            let mut types: Vec<(gertie_core::AgentType, Option<String>)> =
                gertie_core::builtin_agent_types()
                    .into_iter()
                    .map(|t| (t, None))
                    .collect();
            for agent in &config.agents {
                if agent.invocation == crate::config::Invocation::User {
                    // Registered later, when UI dispatch exists.
                    continue;
                }
                let agent_type = gertie_core::AgentType {
                    name: agent.name.clone(),
                    description: agent.description.clone(),
                    system_prompt: agent.prompt.clone().unwrap_or_else(|| {
                        "You are a focused subagent. You have your own tools \
                         and context; the leader sees only your final report. \
                         Work exactly within the brief, verify what you build, \
                         and report honestly."
                            .into()
                    }),
                    tools: agent.tools.clone(),
                };
                let entry = (agent_type, agent.model.clone());
                match types.iter_mut().find(|(t, _)| t.name == agent.name) {
                    Some(slot) => *slot = entry,
                    None => types.push(entry),
                }
            }
            // Build first (borrowing `tools` immutably), insert after.
            let mut additions: Vec<(String, Arc<dyn gertie_core::Tool>)> = Vec::new();
            for (agent_type, model) in &types {
                let worker_provider: Arc<dyn gertie_core::Provider> = match model {
                    Some(model) => {
                        match OpenAICompatible::new(gertie_providers::Endpoint {
                            model: model.clone(),
                            ..endpoint.clone()
                        }) {
                            Ok(provider) => Arc::new(provider),
                            Err(error) => {
                                label.push_str(&format!(" — ⚠ agent {}: {error}", agent_type.name));
                                continue;
                            }
                        }
                    }
                    None => leader_provider.clone(),
                };
                let tool = gertie_core::AgentTool::new(
                    agent_type.clone(),
                    worker_provider,
                    &tools,
                    snapshots.clone(),
                    broker.clone(),
                    endpoint.context_window,
                    workspace_root.clone(),
                );
                label.push_str(&format!(" · agent:{}", agent_type.name));
                additions.push((tool.name().to_string(), Arc::new(tool)));
            }
            let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
            for (name, tool) in additions {
                map.insert(name, tool);
            }
        }

        Self {
            config,
            config_error,
            store,
            provider,
            agent_endpoint,
            tools,
            broker,
            snapshots,
            system_prompt,
            label,
            workspace_root,
        }
    }
}
