//! Shared bootstrap for both fronts — the desktop app and `gertie run`:
//! config, session store, provider, the tool registry (built-ins,
//! skills, web, MCP, agent types), broker, snapshots. Everything that
//! must exist identically no matter which surface you talk to.

use gertie_core::Tool as _;
use gertie_providers::AnyProvider;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The bootstrap parts a settings save replaces live: the provider
/// (and its fallback chain), the preferred agent endpoint, the status
/// label, and the settings snapshot. Tabs re-read these on apply; MCP
/// connections, the store, and already-built worker agents keep their
/// setup until restart — the label and the panel say so.
#[derive(Clone)]
pub struct Reloaded {
    /// The full config as re-read — the settings editor edits the
    /// real fields, not a summary of them.
    pub config: Arc<crate::config::Config>,
    pub provider: Option<Arc<AnyProvider>>,
    pub agent_endpoint: Option<gertie_providers::Endpoint>,
    pub label: String,

    /// Just the identity — `ppq · glm-5.3-flash` — for the footer,
    /// where the capability suffix (skills, agents) and key warnings
    /// would be noise. Headless keeps the full label on stderr.
    pub model_label: String,
    /// Discovered prompt templates — `/name` expands from the
    /// composer. Reload keeps the load-time set (templates don't
    /// depend on endpoint config).
    pub templates: Vec<gertie_tools::templates::Template>,
    pub settings: Arc<crate::settings::SettingsSummary>,
}

pub struct Bootstrap {
    pub config: crate::config::Config,
    /// The `--config` override, if any — reload reads the same file
    /// the original load did.
    config_path: Option<PathBuf>,
    /// Config load/parse failure, if any. The GUI reports it in the
    /// status label (a GUI user never sees stderr); headless treats it
    /// as fatal.
    pub config_error: Option<String>,
    pub store: Arc<gertie_core::SessionStore>,
    /// Live-reloadable state (provider, agent endpoint, label,
    /// settings), swapped after a settings save.
    reloaded: std::sync::RwLock<Reloaded>,
    pub tools: gertie_core::ToolRegistry,
    pub broker: Arc<gertie_core::PermissionBroker>,
    /// The front's current session — subagent dispatch reads it to link
    /// worker runs to the chat that dispatched them.
    pub session_scope: gertie_core::SessionScope,
    pub snapshots: Option<Arc<gertie_core::SnapshotStore>>,
    pub system_prompt: Option<String>,
    /// Human-readable status line: endpoint, warnings, capability counts.
    pub workspace_root: PathBuf,
    /// The label tail that doesn't change on an endpoint switch:
    /// skills, MCP connections, agent types — set up once at load.
    capability_suffix: String,
    skill_count: usize,
    /// Tool names that don't capture the workspace root (web, history,
    /// MCP, skills) — a re-rooted registry carries these over as-is
    /// instead of rebuilding or reconnecting them.
    root_independent: Vec<String>,
    /// The skills index prompt section, shared by every registry's
    /// system prompt (skills are repo content, identical per worktree).
    skill_index: String,
    /// Discovered prompt templates — `/name` expansion in the
    /// composer. Load-time setup; reload carries the same set.
    pub templates: Vec<gertie_tools::templates::Template>,
}

impl Bootstrap {
    /// Live parts, for readers (tabs clone them per construction).
    pub fn provider(&self) -> Option<Arc<AnyProvider>> {
        self.reloaded.read().unwrap().provider.clone()
    }

    pub fn agent_endpoint(&self) -> Option<gertie_providers::Endpoint> {
        self.reloaded.read().unwrap().agent_endpoint.clone()
    }

    pub fn label(&self) -> String {
        self.reloaded.read().unwrap().label.clone()
    }

    /// The footer identity: just provider · model, no wiring.
    pub fn model_label(&self) -> String {
        self.reloaded.read().unwrap().model_label.clone()
    }

    pub fn settings(&self) -> Arc<crate::settings::SettingsSummary> {
        self.reloaded.read().unwrap().settings.clone()
    }

    /// The live config as re-read (a settings save swaps it in).
    pub fn live_config(&self) -> Arc<crate::config::Config> {
        self.reloaded.read().unwrap().config.clone()
    }

    /// A fresh settings editor over the live config.
    pub fn settings_panel(&self) -> crate::settings_edit::SettingsPanelView {
        let reloaded = self.reloaded.read().unwrap();
        crate::settings_edit::SettingsPanelView::new(&reloaded.config, (*reloaded.settings).clone())
    }

    /// A copy of the live state, for fanning an apply out to tabs.
    pub fn snapshot(&self) -> Reloaded {
        self.reloaded.read().unwrap().clone()
    }

    /// Rebuilds the live parts from the config file — what a settings
    /// save calls to make endpoint changes apply to open chats without
    /// a restart. MCP servers, sandbox, and existing worker agents are
    /// load-time setup; the label keeps saying what's live.
    pub fn reload(&self) -> Result<(), String> {
        let config = match &self.config_path {
            Some(path) => crate::config::Config::load_from(path)?,
            None => crate::config::Config::load()?,
        };
        let endpoint = config.selected_endpoint();
        let model_label = endpoint
            .as_ref()
            .map(|endpoint| format!("{} · {}", endpoint.name, endpoint.model))
            .unwrap_or_else(|| "no endpoint configured".to_string());
        let mut label = model_label.clone();
        for warning in config.key_warnings() {
            label.push_str(&format!(" — ⚠ {warning}"));
        }
        label.push_str(&self.capability_suffix);
        let (provider, agent_endpoint) = provider_chain(&config);
        let mut settings = crate::settings::SettingsSummary::from_config(&config);
        settings.skills = self.skill_count;
        *self.reloaded.write().unwrap() = Reloaded {
            config: Arc::new(config),
            provider,
            agent_endpoint,
            label,
            model_label,
            templates: self.templates.clone(),
            settings: Arc::new(settings),
        };
        Ok(())
    }
    /// Builds everything both fronts share. Never exits on its own
    /// except for the one non-negotiable (no session store): callers
    /// decide what their surface does with warnings.
    pub fn load(config_path: Option<&Path>) -> Self {
        // First run with the default path: no config file yet means
        // gertie writes a working starter (one endpoint, env key) and
        // says so — the user edits instead of staring at a blank wall.
        // An explicit --config override never gets one written.
        let skeleton_note = match config_path {
            None => crate::config::Config::path()
                .and_then(|path| crate::config::Config::ensure_skeleton(&path)),
            Some(_) => None,
        };
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
        let model_label = if let Some(error) = &config_error {
            format!("config error — {error}")
        } else if let Some(note) = &skeleton_note {
            note.clone()
        } else {
            endpoint
                .as_ref()
                .map(|endpoint| format!("{} · {}", endpoint.name, endpoint.model))
                .unwrap_or_else(|| "no endpoint configured".to_string())
        };
        let mut label = model_label.clone();
        for warning in config.key_warnings() {
            label.push_str(&format!(" — ⚠ {warning}"));
        }
        // Everything appended from here on (skills, MCP, agents) is
        // load-time setup — the reload path reuses it verbatim as the
        // label tail, since only endpoints change live.
        let capability_start = label.len();

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
        let mut root_independent: Vec<String> = Vec::new();

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
        // Built-ins materialize before discovery so the writing skill is
        // found this launch. The voice-sample cache derives from the
        // same dir skills live in (beside config.toml).
        let builtin_warnings = materialize_builtin_skills(&skills_user, skills_user.parent());
        let (skills, skill_warnings) =
            gertie_tools::skills::discover(&skills_user, &skills_project);
        let mut skill_warnings = skill_warnings;
        skill_warnings.extend(builtin_warnings);
        let skill_index = gertie_tools::skills::index_prompt(&skills);
        if !skills.is_empty() {
            let count = skills.len();
            Arc::get_mut(&mut tools)
                .expect("registry not yet shared")
                .insert(
                    "load_skill".into(),
                    Arc::new(gertie_tools::skills::LoadSkill::new(&skills)),
                );
            label.push_str(&format!(" · skills×{count}"));
            root_independent.push("load_skill".into());
        }
        for warning in &skill_warnings {
            label.push_str(&format!(" — ⚠ {warning}"));
        }

        // Prompt templates: the same two-root convention as skills —
        // `/name` in the composer expands from a file; the builtins
        // /compact and /export are UI-side and always win. Discovery
        // problems are warnings, never a failed launch.
        let templates_user = config_path
            .and_then(|path| path.parent().map(|parent| parent.join("templates")))
            .or_else(|| {
                crate::config::Config::path().map(|path| path.parent().unwrap().join("templates"))
            })
            .unwrap_or_else(|| PathBuf::from(".gertie-templates"));
        let templates_project = workspace_root.join(".gertie/templates");
        let builtin_template_warnings = materialize_builtin_templates(&templates_user);
        let (templates, template_warnings) =
            gertie_tools::templates::discover(&templates_user, &templates_project);
        for warning in builtin_template_warnings.iter().chain(&template_warnings) {
            label.push_str(&format!(" — ⚠ {warning}"));
        }

        // The system prompt: identity, the one policy paragraph the model
        // must know, the skill index. Terse by the two-loads rule — every
        // line is context paid on every turn.
        let system_prompt = system_prompt_for(&workspace_root, &skill_index);

        // Web tools: fetch always; search gracefully-disabled unless a
        // backend resolves; history search rides the session store.
        let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
        map.insert("web_fetch".into(), Arc::new(gertie_tools::web::WebFetch));
        root_independent.push("web_fetch".into());
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
        root_independent.push("web_search".into());
        map.insert(
            "search_history".into(),
            Arc::new(gertie_tools::web::SearchHistory::new(store.clone())),
        );
        root_independent.push("search_history".into());

        // MCP servers connect at startup; a server that won't start is a
        // status-line warning, never a failed launch — gertie's local
        // tools carry the session alone if needed.
        if !config.mcp_servers.is_empty() {
            let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
            for server in &config.mcp_servers {
                // Streamable HTTP when a URL is configured; stdio
                // (spawned process) otherwise.
                let connection = match &server.url {
                    Some(url) => gertie_tools::mcp::McpClient::connect_url(&server.name, url),
                    None => match &server.command {
                        Some(command) => gertie_tools::mcp::McpClient::connect(
                            &server.name,
                            command,
                            &server.args,
                            &server
                                .env
                                .iter()
                                .map(|(key, value)| (key.clone(), value.clone()))
                                .collect::<Vec<_>>(),
                        ),
                        None => {
                            label.push_str(&format!(
                                " — ⚠ mcp {}: needs a url or a command",
                                server.name
                            ));
                            continue;
                        }
                    },
                };
                match connection {
                    Ok(client) => match Arc::new(client).list_tools() {
                        Ok(mcp_tools) => {
                            let count = mcp_tools.len();
                            for tool in mcp_tools {
                                root_independent.push(tool.name().to_string());
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
        // once. Every configured endpoint rides the provider — the
        // selected one first, then same-wire endpoints as fallbacks
        // when an endpoint dies before producing content (model names
        // don't translate across wires, so the chain never switches).
        // The preferred endpoint is kept for agent-type model
        // overrides on the same wire.
        let (provider, agent_endpoint) = provider_chain(&config);

        let broker = Arc::new(gertie_core::PermissionBroker::new());
        let session_scope = gertie_core::SessionScope::new();

        // Agent types: each model-invoked type becomes an agent_{name}
        // tool. Construction order is safe either way — worker registries
        // strip every agent_* tool, so workers can never see each other.
        if let (Some(endpoint), Some(leader_provider)) = (&agent_endpoint, &provider) {
            // The shipped registry first; a config `[[agents]]` entry with
            // a matching name replaces the built-in (and may pin its own
            // model or endpoint), extras are added on top. Built-ins
            // inherit the leader provider.
            let (types, type_warnings) = agent_types(&config, endpoint);
            for warning in type_warnings {
                label.push_str(&format!(" — ⚠ {warning}"));
            }
            // Build first (borrowing `tools` immutably), insert after.
            let mut additions: Vec<(String, Arc<dyn gertie_core::Tool>)> = Vec::new();
            for target in &types {
                let (worker_provider, context_window) =
                    match worker_backend(target, endpoint, leader_provider) {
                        Ok(backend) => backend,
                        Err(error) => {
                            label.push_str(&format!(
                                " — ⚠ agent {}: {error}",
                                target.agent_type.name
                            ));
                            continue;
                        }
                    };
                let tool = gertie_core::AgentTool::new(
                    target.agent_type.clone(),
                    worker_provider,
                    &tools,
                    snapshots.clone(),
                    broker.clone(),
                    context_window,
                    workspace_root.clone(),
                    Some(store.clone()),
                    session_scope.clone(),
                );
                match &target.endpoint {
                    Some(pinned) => label.push_str(&format!(
                        " · agent:{}@{}",
                        target.agent_type.name, pinned.name
                    )),
                    None => label.push_str(&format!(" · agent:{}", target.agent_type.name)),
                }
                additions.push((tool.name().to_string(), Arc::new(tool)));
            }
            let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
            for (name, tool) in additions {
                map.insert(name, tool);
            }
        }

        // The settings panel's snapshot of the effective config.
        let mut settings = crate::settings::SettingsSummary::from_config(&config);
        settings.skills = skills.len();
        let live_config = Arc::new(config.clone());

        Self {
            config,
            config_path: config_path.map(Path::to_path_buf),
            config_error,
            store,
            capability_suffix: label[capability_start..].to_string(),
            skill_count: skills.len(),
            templates: templates.clone(),
            reloaded: std::sync::RwLock::new(Reloaded {
                config: live_config,
                provider,
                agent_endpoint,
                label,
                model_label,
                templates,
                settings: Arc::new(settings),
            }),
            tools,
            broker,
            session_scope,
            snapshots,
            system_prompt,
            workspace_root,
            root_independent,
            skill_index,
        }
    }
    /// A registry re-rooted at `root`, plus its system prompt: the
    /// built-ins are rebuilt (they capture the workspace root), agent
    /// tools are rebuilt against the new base (a worker dispatched from
    /// this registry works in the same tree), and root-independent
    /// tools — web, history, MCP connections, skills — are carried over
    /// as-is rather than duplicated. Shared infrastructure (broker,
    /// store, snapshots) is untouched: one approval queue, one history.
    pub fn registry_for_root(&self, root: &Path) -> (gertie_core::ToolRegistry, Option<String>) {
        let sandbox =
            self.config.security.sandbox_bash.then(|| {
                std::sync::Arc::new(gertie_tools::sandbox::Sandbox::new(root.to_path_buf()))
            });
        let mut tools = gertie_tools::builtin_tools_with(root, sandbox);
        {
            let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
            for name in &self.root_independent {
                if let Some(tool) = self.tools.get(name) {
                    map.insert(name.clone(), tool.clone());
                }
            }
        }
        if let (Some(endpoint), Some(leader_provider)) = (self.agent_endpoint(), self.provider()) {
            let (types, type_warnings) = agent_types(&self.config, &endpoint);
            for warning in type_warnings {
                eprintln!("warning: {warning}");
            }
            let mut additions: Vec<(String, Arc<dyn gertie_core::Tool>)> = Vec::new();
            for target in &types {
                let (worker_provider, context_window) =
                    match worker_backend(target, &endpoint, &leader_provider) {
                        Ok(backend) => backend,
                        Err(error) => {
                            eprintln!("warning: agent {}: {error}", target.agent_type.name);
                            continue;
                        }
                    };
                let tool = gertie_core::AgentTool::new(
                    target.agent_type.clone(),
                    worker_provider,
                    &tools,
                    self.snapshots.clone(),
                    self.broker.clone(),
                    context_window,
                    root.to_path_buf(),
                    Some(self.store.clone()),
                    self.session_scope.clone(),
                );
                additions.push((tool.name().to_string(), Arc::new(tool)));
            }
            let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
            for (name, tool) in additions {
                map.insert(name, tool);
            }
        }
        (tools, system_prompt_for(root, &self.skill_index))
    }
}

/// The provider over every configured endpoint (selected first, then
/// same-wire fallbacks), plus the preferred endpoint for agent-type
/// model overrides. Shared by `load` and `reload` — an endpoint
/// switch must produce exactly what a restart would have.
fn provider_chain(
    config: &crate::config::Config,
) -> (Option<Arc<AnyProvider>>, Option<gertie_providers::Endpoint>) {
    let all_endpoints = config.resolved_endpoints();
    let agent_endpoint = all_endpoints.first().cloned();
    let selected_wire = all_endpoints
        .first()
        .map(|endpoint| endpoint.wire.clone())
        .unwrap_or_default();
    let chain: Vec<gertie_providers::Endpoint> = all_endpoints
        .into_iter()
        .filter(|endpoint| endpoint.wire == selected_wire)
        .collect();
    let provider = match chain.first() {
        Some(_) => gertie_providers::AnyProvider::with_endpoints(chain)
            .map(Arc::new)
            .map_err(|error| eprintln!("warning: endpoint setup failed: {error}"))
            .ok(),
        None => None,
    };
    (provider, agent_endpoint)
}

/// An agent type plus the endpoint its worker talks to. `None` = the
/// leader's provider (built-ins, and config agents that pin nothing);
/// `Some` = a dedicated single-endpoint provider — a model override on
/// the leader's endpoint, or a pin to a different configured endpoint
/// (a different provider or wire) entirely.
struct AgentTarget {
    agent_type: gertie_core::AgentType,
    endpoint: Option<gertie_providers::Endpoint>,
}

/// The shipped agent types first; a config `[[agents]]` entry with a
/// matching name replaces the built-in (and may pin its own model or
/// endpoint), extras are added on top. A pin to an endpoint that isn't
/// configured is a warning and the entry is skipped — the built-in
/// keeps its slot, because a council member that silently answers as
/// some other model is worse than a missing one. Warnings come back
/// for the caller to surface.
fn agent_types(
    config: &crate::config::Config,
    leader_endpoint: &gertie_providers::Endpoint,
) -> (Vec<AgentTarget>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut types: Vec<AgentTarget> = gertie_core::builtin_agent_types()
        .into_iter()
        .map(|agent_type| AgentTarget {
            agent_type,
            endpoint: None,
        })
        .collect();
    for agent in &config.agents {
        if agent.invocation == crate::config::Invocation::User {
            // Registered later, when UI dispatch exists.
            continue;
        }
        let endpoint = match &agent.endpoint {
            Some(name) => match config.endpoints.iter().find(|e| &e.name == name) {
                Some(endpoint_config) => {
                    let mut resolved = endpoint_config.resolve();
                    if let Some(model) = &agent.model {
                        resolved.model = model.clone();
                    }
                    Some(resolved)
                }
                None => {
                    warnings.push(format!(
                        "agent {}: endpoint {name:?} is not configured — skipping",
                        agent.name
                    ));
                    continue;
                }
            },
            None => agent.model.clone().map(|model| gertie_providers::Endpoint {
                model,
                ..leader_endpoint.clone()
            }),
        };
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
        let entry = AgentTarget {
            agent_type,
            endpoint,
        };
        match types.iter_mut().find(|t| t.agent_type.name == agent.name) {
            Some(slot) => *slot = entry,
            None => types.push(entry),
        }
    }
    (types, warnings)
}

/// What a dispatched worker talks to: its provider and the context
/// window its compaction should assume. A pinned worker gets exactly
/// its own endpoint — no fallback chain — so a council member fails
/// loudly instead of quietly answering as some other model.
fn worker_backend(
    target: &AgentTarget,
    leader_endpoint: &gertie_providers::Endpoint,
    leader_provider: &Arc<AnyProvider>,
) -> Result<(Arc<dyn gertie_core::Provider>, Option<u64>), String> {
    match &target.endpoint {
        Some(endpoint) => {
            let provider = gertie_providers::AnyProvider::with_endpoints(vec![endpoint.clone()])
                .map_err(|error| error.to_string())?;
            Ok((Arc::new(provider), endpoint.context_window))
        }
        None => Ok((leader_provider.clone(), leader_endpoint.context_window)),
    }
}

/// The voice block: always on, part of every request. This is what
/// keeps chat replies from reading like chatbot output — the writing
/// skill carries the full discipline for kept prose, but a two-line
/// answer never loads a skill, so the default lives here.
const VOICE_SECTION: &str = "## Voice\n\n\
     Write like a person, not a press release. Lead with the answer; keep \
     the reasoning that would change the reader's decision. No staged \
     openers (\"Let's dive in\", \"Great question\") and no closers (\"I \
     hope this helps\"). No \"it's not X, it's Y\" contrasts. No forced \
     triples. Em dashes: at most one per paragraph, never in a row. No \
     bold labels on list items; headings in sentence case. Plain verbs \
     (is, has — not \"serves as\", \"boasts\"). Specifics beat \
     significance: a fact, never a testament. Vary sentence length. For \
     prose the reader will keep — docs, READMEs, commit messages — load \
     the writing skill.";

/// The system prompt: identity, the one policy paragraph the model
/// must know, the voice block, the skill index. Terse by the two-loads
/// rule — every line is context paid on every turn.
fn system_prompt_for(root: &Path, skill_index: &str) -> Option<String> {
    let mut prompt = format!(
        "You are gertie, a coding agent working in {}.\n\
         Read-only tools run without asking; anything that changes the \
         world shows the user a card and waits. Write code, run the \
         tests, report plainly.",
        root.display()
    );
    prompt.push_str("\n\n");
    prompt.push_str(VOICE_SECTION);
    if !skill_index.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(skill_index);
    }
    Some(prompt)
}

/// The built-in writing skill, embedded at compile time and
/// materialized into the user skills dir on launch — write-if-absent,
/// so from the first run the file is the user's to edit or shadow
/// (a project skill named `writing` overrides it, same rule as for
/// any other skill). Same posture as the starter config.toml.
const WRITING_SKILL: &str = include_str!("skills/writing/SKILL.md");
const COUNCIL_SKILL: &str = include_str!("skills/council/SKILL.md");
const EXPLAIN_TEMPLATE: &str = include_str!("templates/explain.md");

/// Writes the built-in templates if absent — same write-if-absent
/// contract as the writing skill: a user edit is theirs, and survives
/// every launch. Returns warnings, never a failed launch.
fn materialize_builtin_templates(templates_user: &Path) -> Vec<String> {
    let file = templates_user.join("explain.md");
    if file.exists() {
        return vec![];
    }
    if let Err(error) = std::fs::create_dir_all(templates_user)
        .and_then(|_| std::fs::write(&file, EXPLAIN_TEMPLATE))
    {
        vec![format!("built-in explain template: {error}")]
    } else {
        vec![]
    }
}

/// Write-if-absent materialization of one built-in skill directory —
/// same contract as the starter config.toml: the user's edit (or a
/// project shadow) is theirs and survives every launch. Returns
/// warnings, never a failed launch.
fn materialize_skill(skills_user: &Path, name: &str, contents: &str) -> Vec<String> {
    let dir = skills_user.join(name);
    let skill_file = dir.join("SKILL.md");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        return vec![format!("built-in {name} skill: {error}")];
    }
    if !skill_file.exists()
        && let Err(error) = std::fs::write(&skill_file, contents)
    {
        return vec![format!("built-in {name} skill: {error}")];
    }
    vec![]
}

/// Writes the built-in skills if absent, and refreshes the voice-sample
/// cache: the user-level sample (`voice.md` beside config.toml) lives
/// outside any workspace, where `read_file` can't follow, so a copy
/// rides in the writing skill's dir and is re-copied (or removed) every
/// launch. Project-level samples (workspace `.gertie/voice.md`) are read
/// in place and need no cache. Returns warnings, never a failed launch.
fn materialize_builtin_skills(skills_user: &Path, user_config_dir: Option<&Path>) -> Vec<String> {
    let mut warnings = materialize_skill(skills_user, "writing", WRITING_SKILL);
    warnings.extend(materialize_skill(skills_user, "council", COUNCIL_SKILL));

    let writing_dir = skills_user.join("writing");
    let sample = user_config_dir.map(|dir| dir.join("voice.md"));
    let cache = writing_dir.join("voice.md");
    match sample {
        Some(sample) if sample.exists() => {
            if let Err(error) =
                std::fs::read_to_string(&sample).and_then(|sample| std::fs::write(&cache, sample))
            {
                warnings.push(format!("voice sample copy: {error}"));
            }
        }
        // No user sample: a stale cache would speak in a deleted
        // voice — remove it.
        _ => {
            if let Err(error) = std::fs::remove_file(&cache)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                warnings.push(format!("voice sample copy: {error}"));
            }
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn the_writing_skill_materializes_once_and_is_the_users_after() {
        let dir = tempdir().unwrap();
        let skills_user = dir.path().join("skills");

        let warnings = materialize_builtin_skills(&skills_user, None);
        assert!(warnings.is_empty(), "{warnings:?}");
        let skill_file = skills_user.join("writing/SKILL.md");
        let first = std::fs::read_to_string(&skill_file).unwrap();
        assert!(first.starts_with("---"), "parse_skill demands frontmatter");
        assert!(first.contains("name: writing"));
        let council_file = skills_user.join("council/SKILL.md");
        assert!(
            council_file.exists()
                && std::fs::read_to_string(&council_file)
                    .unwrap()
                    .contains("name: council"),
            "council materialized alongside"
        );

        // Discovery picks the built-in up like any other skill.
        let (skills, _) =
            gertie_tools::skills::discover(&skills_user, &dir.path().join(".gertie/skills"));
        let writing = skills.iter().find(|skill| skill.name == "writing");
        assert!(writing.is_some(), "built-in writing skill not discovered");
        assert!(!writing.unwrap().description.is_empty());

        // A user edit survives the next launch.
        std::fs::write(&skill_file, "mine").unwrap();
        let warnings = materialize_builtin_skills(&skills_user, None);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(std::fs::read_to_string(&skill_file).unwrap(), "mine");
    }

    #[test]
    fn the_voice_sample_cache_follows_the_user_file() {
        let dir = tempdir().unwrap();
        let config_dir = dir.path().join("config");
        let skills_user = dir.path().join("skills");
        std::fs::create_dir_all(&config_dir).unwrap();
        let cache = skills_user.join("writing/voice.md");

        // No sample: no cache.
        let warnings = materialize_builtin_skills(&skills_user, Some(&config_dir));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(!cache.exists());

        // A sample is copied on launch.
        std::fs::write(config_dir.join("voice.md"), "Short sentences. Period.").unwrap();
        materialize_builtin_skills(&skills_user, Some(&config_dir));
        assert_eq!(
            std::fs::read_to_string(&cache).unwrap(),
            "Short sentences. Period."
        );

        // Edits propagate on the next launch.
        std::fs::write(config_dir.join("voice.md"), "updated").unwrap();
        materialize_builtin_skills(&skills_user, Some(&config_dir));
        assert_eq!(std::fs::read_to_string(&cache).unwrap(), "updated");

        // Deletion removes the stale cache.
        std::fs::remove_file(config_dir.join("voice.md")).unwrap();
        materialize_builtin_skills(&skills_user, Some(&config_dir));
        assert!(!cache.exists());
    }

    #[test]
    fn the_system_prompt_always_carries_the_voice() {
        let prompt = system_prompt_for(Path::new("/w"), "").unwrap();
        assert!(prompt.contains("## Voice"));

        // The skill index still appends when present.
        let prompt = system_prompt_for(Path::new("/w"), "## Skills\n\n- **x** — y\n").unwrap();
        assert!(prompt.contains("## Voice"));
        assert!(prompt.contains("**x**"));
    }

    /// Council seats resolve to their pinned endpoint; a pin to an
    /// endpoint that isn't configured warns and keeps the built-in.
    #[test]
    fn agent_pins_resolve_or_warn() {
        let mut config = crate::config::Config::default();
        config.endpoints.push(crate::config::EndpointConfig {
            name: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            model: "claude-opus-4-6".into(),
            wire: Some("anthropic".into()),
            api_key: None,
            api_key_env: Some("ANTHROPIC_API_KEY".into()),
            context_window: Some(200_000),
        });
        let mut seat = crate::config::AgentConfig {
            name: "council-anthropic".into(),
            description: "Use when a council review runs.".into(),
            invocation: Default::default(),
            model: None,
            endpoint: Some("anthropic".into()),
            prompt: None,
            tools: None,
        };
        config.agents.push(seat.clone());
        seat.name = "review".into(); // replaces the built-in…
        seat.endpoint = Some("nowhere".into()); // …but the pin is broken
        config.agents.push(seat);
        config.agents.push(crate::config::AgentConfig {
            name: "cheap-explore".into(),
            description: "Use for cheap exploration.".into(),
            invocation: Default::default(),
            model: Some("glm-5.3-flash".into()),
            endpoint: None,
            prompt: None,
            tools: None,
        });

        let leader = gertie_providers::Endpoint {
            name: "ppq".into(),
            base_url: "https://api.ppq.ai/v1".into(),
            model: "glm-5.3-flash".into(),
            api_key: None,
            context_window: Some(1_000_000),
            wire: Default::default(),
        };

        let (types, warnings) = agent_types(&config, &leader);

        // The good seat: pinned to its endpoint, wire included.
        let seat = types
            .iter()
            .find(|t| t.agent_type.name == "council-anthropic")
            .expect("seat present");
        let pinned = seat.endpoint.as_ref().expect("seat pinned");
        assert_eq!(pinned.name, "anthropic");
        assert_eq!(pinned.model, "claude-opus-4-6");

        // A model-only override swaps the model on the leader's endpoint.
        let cheap = types
            .iter()
            .find(|t| t.agent_type.name == "cheap-explore")
            .expect("extra present");
        let pinned = cheap.endpoint.as_ref().expect("model pin");
        assert_eq!(pinned.name, "ppq");
        assert_eq!(pinned.model, "glm-5.3-flash");

        // The broken pin: a warning, and the built-in review keeps its
        // slot on the leader's provider — never a silent substitute.
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("agent review") && w.contains("nowhere")),
            "warning names the agent and the endpoint: {warnings:?}"
        );
        let review = types
            .iter()
            .find(|t| t.agent_type.name == "review")
            .expect("built-in kept");
        assert!(review.endpoint.is_none(), "built-in unpinned");
    }
}
