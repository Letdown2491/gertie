//! Shared bootstrap for both fronts, the desktop app and `gertie run`:
//! config, session store, provider, the tool registry (built-ins,
//! skills, web, MCP, agent types), broker, snapshots. Everything that
//! must exist identically no matter which surface you talk to.

use gertie_core::Tool as _;
use gertie_providers::AnyProvider;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::channel;

/// The bootstrap parts a settings save replaces live: the provider
/// (and its fallback chain), the preferred agent endpoint, the status
/// label, and the settings snapshot. Tabs re-read these on apply; MCP
/// connections, the store, and already-built worker agents keep their
/// setup until restart, the label and the panel say so.
#[derive(Clone)]
pub struct Reloaded {
    /// The full config as re-read, the settings editor edits the
    /// real fields, not a summary of them.
    pub config: Arc<crate::config::Config>,
    pub provider: Option<Arc<AnyProvider>>,
    pub agent_endpoint: Option<gertie_providers::Endpoint>,
    pub label: String,

    /// Just the identity, `endpoint · model`, for the footer,
    /// where the capability suffix (skills, agents) and key warnings
    /// would be noise. Headless keeps the full label on stderr.
    pub model_label: String,
    /// Discovered prompt templates, `/name` expands from the
    /// composer. Reload keeps the load-time set (templates don't
    /// depend on endpoint config).
    pub templates: Vec<gertie_tools::templates::Template>,
    pub settings: Arc<crate::settings::SettingsSummary>,
}

/// Everything a re-rooted registry carries: the rebuilt tools, the
/// prompt, the always-layer, the memory index and store. `Switched`
/// spreads these into a tab.
pub type RootedRegistry = (
    gertie_core::ToolRegistry,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<Arc<gertie_core::MemoryStore>>,
);

pub struct Bootstrap {
    pub config: crate::config::Config,
    /// The `--config` override, if any, reload reads the same file
    /// the original load did.
    config_path: Option<PathBuf>,
    /// Config load/parse failure, if any. The GUI reports it in the
    /// status label (a GUI user never sees stderr); headless treats it
    /// as fatal.
    pub config_error: Option<String>,
    /// The session store. An ephemeral run (`gertie run --ephemeral`)
    /// opens it in-memory: everything downstream (sessions, messages,
    /// FTS, the memory sidecar) works unchanged against a throwaway
    /// store that evaporates when the process exits.
    pub store: Arc<gertie_core::SessionStore>,
    /// Live-reloadable state (provider, agent endpoint, label,
    /// settings), swapped after a settings save.
    reloaded: std::sync::RwLock<Reloaded>,
    pub tools: gertie_core::ToolRegistry,
    pub broker: Arc<gertie_core::PermissionBroker>,
    /// The front's current session, subagent dispatch reads it to link
    /// worker runs to the chat that dispatched them.
    pub session_scope: gertie_core::SessionScope,
    /// The front's active permission mode, the chat sets it per turn,
    /// worker dispatch reads it so workers inherit the posture.
    pub mode_scope: gertie_core::ModeScope,
    /// Whether bash runs inside the Landlock sandbox. Static config,
    /// mirrored into every turn's spec.
    pub sandbox_bash: bool,
    /// The mode new sessions open in (`[permissions] default_mode`).
    pub default_mode: gertie_core::PermissionMode,
    /// The repo's own overlay file, if one loads, re-resolved
    /// against the live config on every reload.
    overlay: Option<crate::config::ProjectOverlay>,
    /// The overlay's load/parse failure, if any, reported in the
    /// label and the doctor, never fatal.
    pub overlay_error: Option<String>,
    pub snapshots: Option<Arc<gertie_core::SnapshotStore>>,
    pub system_prompt: Option<String>,
    /// The memory index snapshotted at boot for the load-time root.
    pub memory_index: Option<String>,
    /// The shared memory store (both tiers) and the extraction
    /// endpoint name from `[memory]` config. None store = no data dir.
    pub memory: Option<Arc<gertie_core::MemoryStore>>,
    pub memory_extract_provider: Option<Arc<dyn gertie_core::Provider>>,
    /// Session instructions loaded from AGENTS.md files (the
    /// always-layer), resolved for the load-time root. Threads into
    /// every leader session prompt as framing; workers get them in
    /// their brief envelope instead.
    pub instructions: Option<String>,
    /// Human-readable status line: endpoint, warnings, capability counts.
    pub workspace_root: PathBuf,
    /// The label tail that doesn't change on an endpoint switch:
    /// skills, MCP connections, agent types, set up once at load.
    capability_suffix: String,
    skill_count: usize,
    /// Tool names that don't capture the workspace root (web, history,
    /// MCP, skills), a re-rooted registry carries these over as-is
    /// instead of rebuilding or reconnecting them.
    root_independent: Vec<String>,
    /// The skills index prompt section, shared by every registry's
    /// system prompt (skills are repo content, identical per worktree).
    skill_index: String,
    /// Discovered prompt templates, `/name` expansion in the
    /// composer. Load-time setup; reload carries the same set.
    pub templates: Vec<gertie_tools::templates::Template>,
}

/// Everything a chat needs to move itself to another workspace: the
/// re-rooted registry, the prompt that names it, and the per-root
/// config answers. Built by [`Bootstrap::workspace_for_root`].
pub struct Switched {
    pub tools: gertie_core::ToolRegistry,
    pub system_prompt: Option<String>,
    /// The root's own AGENTS.md text (sanitized), project tier of the
    /// always-layer; the global tier rides the boot's instructions.
    pub instructions: Option<String>,
    /// The memory index for this root, snapshotted at session start.
    pub memory_index: Option<String>,
    pub memory: Option<Arc<gertie_core::MemoryStore>>,
    pub memory_extract_provider: Option<Arc<dyn gertie_core::Provider>>,
    /// The canonicalized root, containment checks and the footer's
    /// word mean the one true path.
    pub workspace_root: PathBuf,
    pub templates: Vec<gertie_tools::templates::Template>,
    /// Whether bash sandboxes in the new root (the live config's
    /// answer, resolved against the *new* root's overlay).
    pub sandbox_bash: bool,
    /// Overlay and discovery problems for the new root, the chat
    /// surfaces them as transcript notices, never silently.
    pub warnings: Vec<String>,
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

    /// The settle reclaim command in force, the live config's, else
    /// the repo overlay's (your config outranks the project's).
    pub fn settle_command(&self) -> Option<String> {
        self.live_config()
            .resolve_settle_command(self.overlay.as_ref())
            .value
    }

    /// Where every overlay-resolvable setting came from, resolved
    /// against the live config.
    pub fn origins(&self) -> crate::config::Origins {
        self.live_config().resolve_origins(self.overlay.as_ref())
    }

    fn skills_user_dir(&self) -> PathBuf {
        self.config_path
            .as_ref()
            .and_then(|path| path.parent().map(|parent| parent.join("skills")))
            .or_else(|| {
                crate::config::Config::path().map(|path| path.parent().unwrap().join("skills"))
            })
            .unwrap_or_else(|| PathBuf::from(".gertie-skills"))
    }

    fn templates_user_dir(&self) -> PathBuf {
        self.config_path
            .as_ref()
            .and_then(|path| path.parent().map(|parent| parent.join("templates")))
            .or_else(|| {
                crate::config::Config::path().map(|path| path.parent().unwrap().join("templates"))
            })
            .unwrap_or_else(|| PathBuf::from(".gertie-templates"))
    }

    /// The branch prefix the repo's overlay asks for on worktree
    /// branches.
    pub fn worktree_branch_prefix(&self) -> String {
        crate::config::Config::overlay_branch_prefix(self.overlay.as_ref())
    }

    pub fn settings(&self) -> Arc<crate::settings::SettingsSummary> {
        self.reloaded.read().unwrap().settings.clone()
    }

    /// The live config as re-read (a settings save swaps it in).
    pub fn live_config(&self) -> Arc<crate::config::Config> {
        self.reloaded.read().unwrap().config.clone()
    }

    /// A fresh settings editor over the live config.
    /// Whether session-end memory extraction runs: `[memory] enabled`,
    /// default off. Read per use (config can reload).
    /// The turn's round budgets, resolved from config (None = the
    /// loop's built-in defaults).
    pub fn max_tool_rounds(&self) -> Option<usize> {
        self.live_config().resolve_max_tool_rounds()
    }

    pub fn grace_rounds(&self) -> Option<usize> {
        self.live_config().resolve_grace_rounds()
    }

    pub fn memory_enabled(&self) -> bool {
        self.live_config().memory_enabled()
    }

    /// The extraction provider for the session-end memory call:
    /// `[memory] extract_endpoint` names an endpoint, absent = the
    /// leader's. None provider = no extraction is possible (the
    /// caller reports a notice, never an error).
    pub fn memory_extract_provider(&self) -> Option<Arc<dyn gertie_core::Provider>> {
        let config = self.live_config();
        if let Some(name) = config
            .memory
            .as_ref()
            .and_then(|memory| memory.extract_endpoint.as_deref())
        {
            let endpoint = config
                .endpoints
                .iter()
                .find(|e| e.name == name)
                .map(|e| e.resolve())?;
            let provider = gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).ok()?;
            return Some(Arc::new(provider));
        }
        // Absent: the session's own endpoint. AnyProvider already
        // unsize-coerces through Arc's target; the map makes it
        // explicit for the trait-object return type.
        self.provider()
            .map(|provider| provider as Arc<dyn gertie_core::Provider>)
    }

    pub fn settings_panel(&self) -> crate::settings_edit::SettingsPanelView {
        let reloaded = self.reloaded.read().unwrap();
        crate::settings_edit::SettingsPanelView::new(&reloaded.config, (*reloaded.settings).clone())
    }

    /// A copy of the live state, for fanning an apply out to tabs.
    pub fn snapshot(&self) -> Reloaded {
        self.reloaded.read().unwrap().clone()
    }

    /// Rebuilds the live parts from the config file, what a settings
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
            label.push_str(&format!(", ⚠ {warning}"));
        }
        label.push_str(&self.capability_suffix);
        let (provider, agent_endpoint) = provider_chain(&config);
        let mut settings = crate::settings::SettingsSummary::from_config(&config);
        // The summary shows what's in force, and from where: the
        // repo's overlay answers where your config is silent.
        let sandbox = config.resolve_sandbox_bash(self.overlay.as_ref());
        settings.sandbox = sandbox.value;
        settings.sandbox_origin = sandbox.origin;
        // Memory's on/off is user-config only (the overlay never
        // carries it: extraction is process, not posture), so the
        // origin is user-config vs default, no overlay resolution.
        settings.memory_enabled = config.memory_enabled();
        settings.memory_dream_days = config.memory_dream_days();
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
    pub fn load(config_path: Option<&Path>, ephemeral: bool) -> Self {
        // First run with the default path: no config file yet means
        // gertie writes a starter of comments and pointers (no endpoint
        // is built in) and says so. An explicit --config override never
        // gets one written.
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
        // The workspace is wherever gertie was launched; tools resolve
        // paths against it and checkpoints land beside the session
        // store. The repo's own overlay is found (or not) here,
        // before the label, so its problems surface with the rest.
        let workspace_root = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let (overlay, overlay_error) = crate::config::Config::load_project_overlay(&workspace_root);
        let endpoint = config.selected_endpoint();
        let model_label = if let Some(error) = &config_error {
            format!("config error, {error}")
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
            label.push_str(&format!(", ⚠ {warning}"));
        }
        if let Some(error) = &overlay_error {
            label.push_str(&format!(", ⚠ project overlay: {error}"));
        }
        // Everything appended from here on (skills, MCP, agents) is
        // load-time setup, the reload path reuses it verbatim as the
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
        // An ephemeral run swaps the real store for an in-memory one:
        // the boot's shape never changes, and the throwaway evaporates
        // at exit. Nothing downstream knows or cares.
        let store = if ephemeral {
            gertie_core::SessionStore::open(std::path::Path::new(":memory:"))
                .map(Arc::new)
                .map_err(|error| eprintln!("warning: ephemeral store: {error}"))
                .ok()
        } else {
            store
        };
        let Some(store) = store else {
            eprintln!("error: no session store; refusing to run without persistence");
            std::process::exit(1);
        };

        // The startup sweep: chats idle past the configured days come
        // back settled, state only, never the reclaim command (that
        // runs when a human presses settle). Skipped entirely until
        // something opts in, your config or the repo's overlay.
        let auto_settle = config.resolve_auto_settle_days(overlay.as_ref());
        if let Some(days) = auto_settle.value
            && days > 0
            && let Err(error) = store.settle_idle_sessions(days)
        {
            eprintln!("warning: auto-settle sweep failed: {error}");
        }

        let snapshots = crate::config::Config::data_dir().and_then(|data_dir| {
            match gertie_core::SnapshotStore::open(&data_dir.join("snapshots")) {
                Ok(store) => Some(Arc::new(store)),
                Err(error) => {
                    eprintln!("warning: checkpoint store disabled: {error}");
                    None
                }
            }
        });
        // Sandbox: opt-in Landlock isolation for bash children,
        // yours, else the repo overlay's, else off.
        let sandbox = config
            .resolve_sandbox_bash(overlay.as_ref())
            .value
            .then(|| {
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
            label.push_str(&format!(", ⚠ {warning}"));
        }

        // Prompt templates: the same two-root convention as skills,
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
            label.push_str(&format!(", ⚠ {warning}"));
        }

        // The system prompt: identity, the one policy paragraph the model
        // must know, the skill index. Terse by the two-loads rule, every
        // line is context paid on every turn.
        let instructions = load_instructions(&workspace_root);
        let system_prompt =
            system_prompt_for(&workspace_root, &skill_index, instructions.as_deref());

        // Web tools: fetch always (private-network targets gated by
        // the user's config, the model may not choose to open them);
        // search gracefully-disabled unless a backend resolves; history
        // search rides the session store.
        let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
        map.insert(
            "web_fetch".into(),
            Arc::new(gertie_tools::web::WebFetch::new(
                config.resolve_allow_private_fetch(overlay.as_ref()).value,
            )),
        );
        root_independent.push("web_fetch".into());
        let search_tool = match &config.search {
            Some(search) => match search.to_backend() {
                Ok(backend) => backend,
                Err(error) => {
                    label.push_str(&format!(", ⚠ search: {error}"));
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

        let (provider, agent_endpoint) = provider_chain(&config);
        // Memory: one store over the repo's tier and the user's global
        // tier, same sidecar as the sessions. Root-independent as
        // registered here; worktree tabs re-root the project tier
        // through registry_for_root, the global tier stays shared.
        let memory = Arc::new(gertie_core::MemoryStore::open(
            workspace_root.join(".gertie/memory"),
            memory_global_root(),
            store.clone(),
        ));
        memory
            .sync()
            .unwrap_or_else(|error| eprintln!("warning: memory sync: {error}"));
        // Dreaming rides the same sweep: idle-gated, opt-in, and only
        // when memory is on. The changelog lands in the tier's dreams/.
        if config.memory_enabled()
            && config.memory_dream_days() > 0
            && let Some(changelog) = dream(&memory, &workspace_root.join(".gertie/memory"))
        {
            eprintln!("{changelog}");
        }
        let memory_index = render_memory_index(&memory);
        let memory_extract_provider: Option<Arc<dyn gertie_core::Provider>> = config
            .memory
            .as_ref()
            .and_then(|memory| memory.extract_endpoint.as_deref())
            .and_then(|name| config.endpoints.iter().find(|e| e.name == name))
            .and_then(|e| gertie_providers::AnyProvider::with_endpoints(vec![e.resolve()]).ok())
            .map(|provider| Arc::new(provider) as Arc<dyn gertie_core::Provider>)
            .or_else(|| {
                provider
                    .clone()
                    .map(|provider| provider as Arc<dyn gertie_core::Provider>)
            });
        map.insert(
            "memory".into(),
            Arc::new(gertie_tools::memory::Memory::new(memory.clone())),
        );
        root_independent.push("memory".into());
        map.insert(
            "search_memory".into(),
            Arc::new(gertie_tools::memory::SearchMemory::new(memory.clone())),
        );
        root_independent.push("search_memory".into());

        // The structured question: the loop intercepts `ask_user`
        // before any tool's run (the answer rides the permission
        // broker, not a tool result), so this stub exists only so the
        // call resolves in the registry and its schema reaches the
        // model. The description is the discipline: the ask is for
        // genuine decision points, not an escape hatch from thinking.
        map.insert("ask_user".into(), Arc::new(gertie_tools::AskUser));
        root_independent.push("ask_user".into());

        // MCP servers connect at startup; a server that won't start is a
        // status-line warning, never a failed launch, gertie's local
        // tools carry the session alone if needed.
        if !config.mcp_servers.is_empty() {
            // MCP servers connect in parallel: startup costs the
            // slowest server's own connect time, not the sum. npm-based
            // servers legitimately take tens of seconds to boot,
            // serially, each added one to the wait. Each thread owns
            // its server's config (cheap clones); answers come back
            // through a channel and apply in config order, so the
            // status label reads the same as a serial connect.
            let servers = &config.mcp_servers;
            let mut results: Vec<Option<Result<Vec<gertie_tools::mcp::McpTool>, String>>> =
                (0..servers.len()).map(|_| None).collect();
            {
                let (tx, rx) = channel();
                for (index, server) in servers.iter().enumerate() {
                    let name = server.name.clone();
                    let url = server.url.clone();
                    let command = server.command.clone();
                    let args = server.args.clone();
                    let env: Vec<(String, String)> = server
                        .env
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect();
                    let tx = tx.clone();
                    let spawned = std::thread::Builder::new()
                        .name(format!("mcp-{name}-connect"))
                        .spawn(move || {
                            // The error text is label-ready: connect
                            // failures already name the server; the
                            // others are given its name here.
                            let connected = match (&url, &command) {
                                (Some(url), _) => {
                                    gertie_tools::mcp::McpClient::connect_url(&name, url)
                                }
                                (None, Some(command)) => gertie_tools::mcp::McpClient::connect(
                                    &name, command, &args, &env,
                                ),
                                (None, None) => {
                                    Err(format!("mcp {name}: needs a url or a command"))
                                }
                            };
                            let result = match connected {
                                Ok(client) => match Arc::new(client).list_tools() {
                                    Ok(mcp_tools) => Ok(mcp_tools),
                                    Err(error) => Err(format!("mcp {name}: {error}")),
                                },
                                Err(error) => Err(error),
                            };
                            let _ = tx.send((index, result));
                        });
                    if let Err(error) = spawned {
                        results[index] = Some(Err(format!("mcp {}: {error}", server.name)));
                    }
                }
                drop(tx); // the iteration ends when every thread has answered
                for (index, result) in rx {
                    results[index] = Some(result);
                }
            }
            let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
            for (index, result) in results.into_iter().enumerate() {
                let server = &servers[index];
                match result.expect("every configured server reports") {
                    Ok(mcp_tools) => {
                        let count = mcp_tools.len();
                        for tool in mcp_tools {
                            root_independent.push(tool.name().to_string());
                            map.insert(tool.name().to_string(), Arc::new(tool));
                        }
                        label.push_str(&format!(" · mcp:{}×{count}", server.name));
                    }
                    Err(error) => label.push_str(&format!(", ⚠ {error}")),
                }
            }
        }

        // Constructing the provider spins up its tokio runtime; do it
        // once. Every configured endpoint rides the provider, the
        // selected one first, then same-wire endpoints as fallbacks
        // when an endpoint dies before producing content (model names
        // don't translate across wires, so the chain never switches).
        // The preferred endpoint is kept for agent-type model
        // overrides on the same wire. Built above (the memory
        // extraction provider falls back to it); nothing here rebuilds.

        let broker = Arc::new(gertie_core::PermissionBroker::new());
        let session_scope = gertie_core::SessionScope::new();
        let mode_scope = gertie_core::ModeScope::new();
        let sandbox_bash = config.resolve_sandbox_bash(overlay.as_ref()).value;
        let default_mode = config.resolve_default_mode(overlay.as_ref()).value;

        // read_session: the model's way back into the record a context
        // compaction selected away. Scoped to the front's open session
        //, the same scope worker dispatch uses for lineage, and
        // read-only, so it auto-runs.
        {
            let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
            map.insert(
                "read_session".into(),
                Arc::new(gertie_tools::web::ReadSession::new(
                    store.clone(),
                    session_scope.clone(),
                )),
            );
            root_independent.push("read_session".into());
        }

        // Agent types: each model-invoked type becomes an agent_{name}
        // tool. Construction order is safe either way, worker registries
        // strip every agent_* tool, so workers can never see each other.
        if let (Some(endpoint), Some(leader_provider)) = (&agent_endpoint, &provider) {
            // The shipped registry first; a config `[[agents]]` entry with
            // a matching name replaces the built-in (and may pin its own
            // model or endpoint), extras are added on top. Built-ins
            // inherit the leader provider.
            let (types, type_warnings) = agent_types(&config, endpoint);
            for warning in type_warnings {
                label.push_str(&format!(", ⚠ {warning}"));
            }
            // Build first (borrowing `tools` immutably), insert after.
            let mut additions: Vec<(String, Arc<dyn gertie_core::Tool>)> = Vec::new();
            for target in &types {
                let (worker_provider, context_window) =
                    match worker_backend(target, endpoint, leader_provider) {
                        Ok(backend) => backend,
                        Err(error) => {
                            label.push_str(&format!(
                                ", ⚠ agent {}: {error}",
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
                    mode_scope.clone(),
                    sandbox_bash,
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
            mode_scope,
            sandbox_bash,
            default_mode,
            overlay,
            overlay_error,
            snapshots,
            system_prompt,
            instructions,
            memory_index,
            memory: Some(Arc::clone(&memory)),
            memory_extract_provider,
            workspace_root,
            root_independent,
            skill_index,
        }
    }
    /// Everything a chat needs to move itself to another workspace:
    /// the re-rooted registry, the prompt that names it, and the
    /// per-root config answers. Built by [`Bootstrap::workspace_for_root`].
    pub fn workspace_for_root(&self, root: &Path) -> Result<Switched, String> {
        let root = root
            .canonicalize()
            .map_err(|error| format!("{}: {error}", root.display()))?;
        let config = self.live_config();
        let (overlay, overlay_error) = crate::config::Config::load_project_overlay(&root);
        let mut warnings = Vec::new();
        if let Some(error) = &overlay_error {
            warnings.push(format!("project overlay: {error}"));
        }

        let sandbox_bash = config.resolve_sandbox_bash(overlay.as_ref()).value;
        let sandbox = sandbox_bash
            .then(|| std::sync::Arc::new(gertie_tools::sandbox::Sandbox::new(root.clone())));
        let mut tools = gertie_tools::builtin_tools_with(&root, sandbox);
        {
            let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
            for name in &self.root_independent {
                if let Some(tool) = self.tools.get(name) {
                    map.insert(name.clone(), tool.clone());
                }
            }
        }
        // Memory's project tier follows the (possibly new) root; the
        // global tier and the sidecar are the boot's shared state.
        let memory = Arc::new(gertie_core::MemoryStore::open(
            root.join(".gertie/memory"),
            memory_global_root(),
            self.store.clone(),
        ));
        memory
            .sync()
            .unwrap_or_else(|error| eprintln!("warning: memory sync: {error}"));
        let memory_index = render_memory_index(&memory);
        {
            let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
            map.insert(
                "memory".into(),
                Arc::new(gertie_tools::memory::Memory::new(memory.clone())),
            );
            map.insert(
                "search_memory".into(),
                Arc::new(gertie_tools::memory::SearchMemory::new(Arc::clone(&memory))),
            );
        }

        // Skills: the user dir rides along; the project dir is the new
        // root's. The index and the load_skill tool rebuild from the
        // fresh discovery, the prompt must promise what the registry
        // can deliver.
        let skills_user = self.skills_user_dir();
        let builtin_warnings = materialize_builtin_skills(&skills_user, skills_user.parent());
        let (skills, skill_warnings) =
            gertie_tools::skills::discover(&skills_user, &root.join(".gertie/skills"));
        warnings.extend(skill_warnings);
        warnings.extend(builtin_warnings);
        let skill_index = gertie_tools::skills::index_prompt(&skills);
        if !skills.is_empty() {
            let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
            map.insert(
                "load_skill".into(),
                Arc::new(gertie_tools::skills::LoadSkill::new(&skills)),
            );
        }

        // Templates: the same two-root convention as skills.
        let templates_user = self.templates_user_dir();
        let (templates, template_warnings) =
            gertie_tools::templates::discover(&templates_user, &root.join(".gertie/templates"));
        warnings.extend(template_warnings);

        // The private-fetch gate is the new root's overlay's answer,
        // not the startup one the carried tool captured.
        let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
        map.insert(
            "web_fetch".into(),
            Arc::new(gertie_tools::web::WebFetch::new(
                config.resolve_allow_private_fetch(overlay.as_ref()).value,
            )),
        );

        // Agent tools rebuild against the new base, exactly as a
        // worktree tab's do, a worker dispatched from this chat
        // works in the tree the chat works in.
        if let (Some(endpoint), Some(leader_provider)) = (self.agent_endpoint(), self.provider()) {
            let (types, type_warnings) = agent_types(&self.config, &endpoint);
            warnings.extend(type_warnings);
            let mut additions: Vec<(String, Arc<dyn gertie_core::Tool>)> = Vec::new();
            for target in &types {
                let (worker_provider, context_window) =
                    match worker_backend(target, &endpoint, &leader_provider) {
                        Ok(backend) => backend,
                        Err(error) => {
                            warnings.push(format!("agent {}: {error}", target.agent_type.name));
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
                    root.clone(),
                    Some(self.store.clone()),
                    self.session_scope.clone(),
                    self.mode_scope.clone(),
                    sandbox_bash,
                );
                additions.push((tool.name().to_string(), Arc::new(tool)));
            }
            let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
            for (name, tool) in additions {
                map.insert(name, tool);
            }
        }

        // Instructions: the global tier once, the project tier per
        // root. Both sanitized at the entrance (agent.rs's
        // sanitize_untrusted), 2048 characters total.
        let global = load_global_instructions();
        let project = load_project_instructions(&root);
        let instructions = join_instructions(global.as_deref(), project.as_deref());
        let system_prompt = system_prompt_for(&root, &skill_index, project.as_deref());
        Ok(Switched {
            tools,
            system_prompt,
            instructions,
            memory_index,
            memory: Some(Arc::clone(&memory)),
            memory_extract_provider: self.memory_extract_provider(),
            workspace_root: root,
            templates,
            sandbox_bash,
            warnings,
        })
    }

    /// A registry re-rooted at `root`, plus its system prompt: the
    /// built-ins are rebuilt (they capture the workspace root), agent
    /// tools are rebuilt against the new base (a worker dispatched from
    /// this registry works in the same tree), and root-independent
    /// tools, web, history, MCP connections, skills, are carried over
    /// as-is rather than duplicated. Shared infrastructure (broker,
    /// store, snapshots) is untouched: one approval queue, one history.
    pub fn registry_for_root(&self, root: &Path) -> RootedRegistry {
        let sandbox = self
            .live_config()
            .resolve_sandbox_bash(self.overlay.as_ref())
            .value
            .then(|| std::sync::Arc::new(gertie_tools::sandbox::Sandbox::new(root.to_path_buf())));
        let mut tools = gertie_tools::builtin_tools_with(root, sandbox);
        {
            let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
            for name in &self.root_independent {
                if let Some(tool) = self.tools.get(name) {
                    map.insert(name.clone(), tool.clone());
                }
            }
        }
        // Memory's project tier follows the tab's root; the global tier
        // and the sidecar are the boot's shared state.
        let memory = Arc::new(gertie_core::MemoryStore::open(
            root.join(".gertie/memory"),
            memory_global_root(),
            self.store.clone(),
        ));
        memory
            .sync()
            .unwrap_or_else(|error| eprintln!("warning: memory sync: {error}"));
        let memory_index = render_memory_index(&memory);
        {
            let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
            map.insert(
                "memory".into(),
                Arc::new(gertie_tools::memory::Memory::new(memory.clone())),
            );
            map.insert(
                "search_memory".into(),
                Arc::new(gertie_tools::memory::SearchMemory::new(Arc::clone(&memory))),
            );
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
                    self.mode_scope.clone(),
                    self.sandbox_bash,
                );
                additions.push((tool.name().to_string(), Arc::new(tool)));
            }
            let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
            for (name, tool) in additions {
                map.insert(name, tool);
            }
        }
        // Per-root instructions: the project tier only, the global
        // tier is shared with the boot and rides along.
        let project = load_project_instructions(root);
        let instructions = join_instructions(self.instructions.as_deref(), project.as_deref());
        let system_prompt = system_prompt_for(root, &self.skill_index, project.as_deref());
        (
            tools,
            system_prompt,
            instructions,
            memory_index,
            Some(Arc::clone(&memory)),
        )
    }
}

/// The memory index, rendered once per boot/switch and frozen: the
/// caller hands the string to every turn of a session and never
/// re-renders mid-session (cache discipline). An empty store renders
/// no index at all: a session without memory pays nothing.
fn render_memory_index(memory: &gertie_core::MemoryStore) -> Option<String> {
    let index = memory.render_index(600);
    (index != "# Memory\n").then_some(index)
}

/// The global memory tier's root. The session store's open has already
/// proved the data dir exists (gertie refuses to run without
/// persistence), so the fallback is unreachable in practice and harmless
/// when reached: a temp dir, never the repo.
fn memory_global_root() -> PathBuf {
    crate::config::Config::data_dir()
        .map(|dir| dir.join("memory"))
        .unwrap_or_else(|| std::env::temp_dir().join("gertie-memory"))
}

/// The always-layer: AGENTS.md files read at session start and
/// injected as framing for every request of the session. The global
/// tier is the user's posture; the project tier is the repo's policy
/// and may arrive from a clone, so both pass through the same
/// sanitizer as MCP and skill text. Absent files are normal; an
/// unreadable one is skipped silently.
fn load_instructions(root: &Path) -> Option<String> {
    join_instructions(
        load_global_instructions().as_deref(),
        load_project_instructions(root).as_deref(),
    )
}

fn load_global_instructions() -> Option<String> {
    let path = crate::config::Config::path()
        .and_then(|config| config.parent().map(|parent| parent.join("AGENTS.md")))?;
    std::fs::read_to_string(&path).ok()
}

fn load_project_instructions(root: &Path) -> Option<String> {
    let canonical = root
        .canonicalize()
        .ok()
        .unwrap_or_else(|| root.to_path_buf());
    let path = canonical.join("AGENTS.md");
    if !path.is_file() {
        return None;
    }
    std::fs::read_to_string(&path).ok()
}

fn join_instructions(global: Option<&str>, project: Option<&str>) -> Option<String> {
    let mut joined = String::new();
    if let Some(text) = global {
        joined.push_str("## Instructions\n\n");
        joined.push_str(text.trim_end());
        joined.push('\n');
    }
    if let Some(text) = project {
        joined.push_str("## Instructions\n\n");
        joined.push_str(text.trim_end());
        joined.push('\n');
    }
    if joined.is_empty() {
        return None;
    }
    Some(gertie_core::agent::sanitize_untrusted(&joined, 2048))
}

/// The provider over every configured endpoint (selected first, then
/// same-wire fallbacks), plus the preferred endpoint for agent-type
/// model overrides. Shared by `load` and `reload`, an endpoint
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
/// `Some` = a dedicated single-endpoint provider, a model override on
/// the leader's endpoint, or a pin to a different configured endpoint
/// (a different provider or wire) entirely.
struct AgentTarget {
    agent_type: gertie_core::AgentType,
    endpoint: Option<gertie_providers::Endpoint>,
}

/// The shipped agent types first; a config `[[agents]]` entry with a
/// matching name replaces the built-in (and may pin its own model or
/// endpoint), extras are added on top. A pin to an endpoint that isn't
/// configured is a warning and the entry is skipped, the built-in
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
                        "agent {}: endpoint {name:?} is not configured, skipping",
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
/// its own endpoint, no fallback chain, so a council member fails
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

/// Dreaming (M6 PR 6): the consolidation pass that keeps memory from
/// rotting. No model call: duplicates and near-duplicates are a text
/// problem (normalized prefix comparison), not a reasoning one. Runs
/// only from the startup sweep, never mid-session (the frozen-index
/// discipline means mid-session rewrites of the store would desync
/// every open session's view). Delta ops only, by construction: the
/// merge archives the loser, nothing deletes.
fn dream(store: &gertie_core::MemoryStore, project_root: &std::path::Path) -> Option<String> {
    let mut ops: Vec<gertie_core::Op> = Vec::new();
    let entries = store.scan();
    // Near-duplicate pass: two valid entries in the same file whose
    // normalized text (lowercased, non-alphanumeric stripped, first
    // 48 chars) agree. The older id survives (stable ids in links);
    // the merged text is the newer entry's (later knowledge wins).
    let mut normalized: std::collections::HashMap<String, &gertie_core::MemoryEntry> =
        std::collections::HashMap::new();
    for entry in &entries {
        if entry.valid_until.is_some() {
            continue;
        }
        let key: String = entry
            .text
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric())
            .take(48)
            .collect();
        if key.is_empty() {
            continue;
        }
        match normalized.get(&key) {
            Some(existing) => {
                // Same tier only (cross-tier merges are human edits);
                // the entry later in the scan is newer knowledge.
                if existing.id.chars().last() == entry.id.chars().last()
                    && existing.file == entry.file
                {
                    ops.push(gertie_core::Op::Merge {
                        keep: existing.id.clone(),
                        drop: entry.id.clone(),
                        text: entry.text.clone(),
                    });
                }
            }
            None => {
                normalized.insert(key, entry);
            }
        }
    }
    // Counter-dead pass: harmful outweighs helpful by 2+ and the entry
    // has any votes at all. Retirement is counter-driven, never
    // time-decay (no decay schedule has survived validation anywhere).
    for entry in &entries {
        if entry.valid_until.is_some() {
            continue;
        }
        let votes = entry.helpful + entry.harmful;
        if votes >= 2 && entry.harmful >= entry.helpful + 2 {
            ops.push(gertie_core::Op::Archive {
                id: entry.id.clone(),
            });
        }
    }
    if ops.is_empty() {
        return None;
    }
    let report = store.apply(&ops, 4096).ok()?;
    if report.applied.is_empty() {
        return None;
    }
    // The changelog: one markdown file per dream in the project tier's
    // dreams/ directory, the audit surface the card cites.
    let date = store.today().ok()?;
    let dir = project_root.join("dreams");
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("dream-{date}.md"));
    let entry_text = format!(
        "## dream {date}\n\n{}\n",
        report
            .applied
            .iter()
            .map(|line| format!("- {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    if let Ok(mut handle) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&file)
    {
        use std::io::Write;
        let _ = handle.write_all(entry_text.as_bytes());
    }
    Some(format!("memory dream: {} ops", report.applied.len()))
}

/// The voice block: always on, part of every request. This is what
/// keeps chat replies from reading like chatbot output, the writing
/// skill carries the full discipline for kept prose, but a two-line
/// answer never loads a skill, so the default lives here.
const VOICE_SECTION: &str = "## Voice\n\n\
     Write like a person, not a press release. Lead with the answer; keep \
     the reasoning that would change the reader's decision. No staged \
     openers (\"Let's dive in\", \"Great question\") and no closers (\"I \
     hope this helps\"). No \"it's not X, it's Y\" contrasts. No forced \
     triples. Em dashes: at most one per paragraph, never in a row. No \
     bold labels on list items; headings in sentence case. Plain verbs \
     (is, has, not \"serves as\", \"boasts\"). Specifics beat \
     significance: a fact, never a testament. Vary sentence length. For \
     prose the reader will keep, docs, READMEs, commit messages, load \
     the writing skill.";

/// The system prompt: identity, the one policy paragraph the model
/// must know, the voice block, the skill index. Terse by the two-loads
/// rule, every line is context paid on every turn.
fn system_prompt_for(root: &Path, skill_index: &str, instructions: Option<&str>) -> Option<String> {
    let mut prompt = format!(
        "You are Gertie, a coding agent working in {}.\n\
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
    if instructions.is_some() {
        prompt.push_str("\n\n");
        prompt.push_str("Instructions loaded from AGENTS.md; they bind this session.");
    }
    Some(prompt)
}

/// The built-in writing skill, embedded at compile time and
/// materialized into the user skills dir on launch, write-if-absent,
/// so from the first run the file is the user's to edit or shadow
/// (a project skill named `writing` overrides it, same rule as for
/// any other skill). Same posture as the starter config.toml.
const WRITING_SKILL: &str = include_str!("skills/writing/SKILL.md");
const COUNCIL_SKILL: &str = include_str!("skills/council/SKILL.md");
const EXPLAIN_TEMPLATE: &str = include_str!("templates/explain.md");
const COMMIT_TEMPLATE: &str = include_str!("templates/commit.md");

/// Writes the built-in templates if absent, same write-if-absent
/// contract as the writing skill: a user edit is theirs, and survives
/// every launch. Returns warnings, never a failed launch.
fn materialize_builtin_templates(templates_user: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    for (name, contents) in [
        ("explain.md", EXPLAIN_TEMPLATE),
        ("commit.md", COMMIT_TEMPLATE),
    ] {
        let file = templates_user.join(name);
        if file.exists() {
            continue;
        }
        if let Err(error) =
            std::fs::create_dir_all(templates_user).and_then(|_| std::fs::write(&file, contents))
        {
            warnings.push(format!("built-in {name} template: {error}"));
        }
    }
    warnings
}

/// Write-if-absent materialization of one built-in skill directory,
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
        // voice, remove it.
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
        let prompt = system_prompt_for(Path::new("/w"), "", None).unwrap();
        assert!(prompt.contains("## Voice"));

        // The skill index still appends when present.
        let prompt = system_prompt_for(Path::new("/w"), "## Skills\n\n- **x**, y\n", None).unwrap();
        assert!(prompt.contains("## Voice"));
        assert!(prompt.contains("**x**"));
    }

    #[test]
    fn instructions_get_a_pointer_in_the_prompt_but_never_contents() {
        let plain = system_prompt_for(Path::new("/w"), "", None).unwrap();
        let loaded = system_prompt_for(Path::new("/w"), "", Some("no em dashes")).unwrap();
        assert!(!plain.contains("Instructions loaded"));
        assert!(loaded.contains("Instructions loaded from AGENTS.md"));
        // The contents ride the injected block, never the static prompt:
        // the pointer stays byte-stable regardless of what the files say.
        assert!(!loaded.contains("no em dashes"));
    }

    #[test]
    fn the_loader_joins_tiers_and_sanitizes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Both absent: nothing.
        assert_eq!(load_instructions(root), None);

        // Project tier only.
        std::fs::write(root.join("AGENTS.md"), "be terse\n").unwrap();
        assert_eq!(
            load_instructions(root).unwrap(),
            "## Instructions\n\nbe terse\n"
        );

        // An unreadable project file is skipped, silently. A
        // directory where the file belongs exercises that arm in any
        // environment (permission bits prove nothing under root, the
        // container's test user, so the chmod route is out).
        std::fs::remove_file(root.join("AGENTS.md")).unwrap();
        std::fs::create_dir(root.join("AGENTS.md")).unwrap();
        assert_eq!(load_instructions(root), None);

        // The global tier is the user's config dir; the join shape is
        // covered through join_instructions directly.
        let joined = join_instructions(Some("global rules"), Some("project rules")).unwrap();
        assert!(joined.starts_with("## Instructions\n\nglobal rules\n"));
        assert!(joined.contains("## Instructions\n\nproject rules\n"));
        // Invisible characters are stripped at the entrance, same as
        // MCP and skill text.
        let dirty = join_instructions(Some("a\u{200B}b"), None).unwrap();
        assert_eq!(dirty, "## Instructions\n\nab\n");
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
        // slot on the leader's provider, never a silent substitute.
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
