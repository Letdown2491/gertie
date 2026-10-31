//! Layered TOML config: the user's file, plus the repo's own overlay
//! (`<repo>/.gertie/config.toml`) for a safe subset, the overlay
//! suggests, the user's file decides, and every resolved value says
//! which tier it came from.

use serde::{Deserialize, Deserializer};
use std::path::{Path, PathBuf};

/// The user's config: `~/.config/gertie/config.toml`.
#[derive(Deserialize, Debug, Clone, Default)]
pub struct Config {
    #[serde(default)]
    pub endpoints: Vec<EndpointConfig>,
    /// Which endpoint to use; defaults to the first listed.
    #[serde(default)]
    pub default_endpoint: Option<String>,
    /// MCP servers to spawn at startup (stdio transport).
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    /// Subagent types, TOML-defined (M4). Each model-invoked type
    /// becomes an `agent_{name}` tool for the leader.
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    /// Web search backend. Absent = web_search explains how to configure
    /// it instead of failing mysteriously.
    #[serde(default)]
    pub search: Option<SearchConfig>,
    /// Security posture. Absent = defaults (sandbox off).
    #[serde(default)]
    pub security: SecurityConfig,
    /// Permission defaults. Absent = supervised.
    #[serde(default)]
    pub permissions: PermissionsConfig,
    /// Auto-settle chats idle for at least this many days, on startup.
    /// Absent or zero = off: settle is manual until you opt in.
    #[serde(default)]
    pub auto_settle_days: Option<u64>,
    /// Shell command run in a settling session's recorded workspace,
    /// the reclaim hook (e.g. `cargo clean`). Manual settles only;
    /// the auto-settle sweep never runs commands.
    #[serde(default)]
    pub settle_command: Option<String>,
    /// Worktree cleanup policy. Absent = never remove (the default:
    /// agent work is never deleted behind your back).
    #[serde(default)]
    pub worktree: Option<WorktreeConfig>,
    /// Long-term memory. Absent = all defaults (extraction off).
    #[serde(default)]
    pub memory: Option<MemoryConfig>,
    /// Turn budgets. Absent = the loop's built-in defaults.
    #[serde(default)]
    pub turn: Option<TurnConfig>,
}

/// The turn's round budgets: the safety valve's two numbers. A turn
/// that loops past `max_tool_rounds` gets the wrap-up nudge, then
/// `grace_rounds` more to land it before the turn pauses resumable.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct TurnConfig {
    /// Tool rounds before the wrap-up nudge. 0 = the built-in default.
    #[serde(default)]
    pub max_tool_rounds: Option<usize>,
    /// Grace rounds after the nudge before the pause. 0 = the
    /// built-in default.
    #[serde(default)]
    pub grace_rounds: Option<usize>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct WorktreeConfig {
    /// Remove a gertie-made worktree that has been idle this many
    /// days, and only one that is clean (nothing uncommitted) and
    /// holds no commits the main branch lacks (nothing unmerged).
    /// Zero or absent = never.
    #[serde(default)]
    pub cleanup_idle_days: Option<u64>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct SecurityConfig {
    /// Landlock-isolate bash children: system reads, workspace//tmp
    /// writes only. Opt-in; when the kernel can't enforce it, sandboxed
    /// commands fail instead of running free. `Some` = the user's file
    /// set it; `None` = unset, a lower tier decides.
    #[serde(default)]
    pub sandbox_bash: Option<bool>,
    /// Let web_fetch reach private-network targets, localhost dev
    /// servers, the LAN, tailnets. Off by default: the model has no
    /// business probing a router or a cloud-metadata endpoint. `Some`
    /// = the user's file set it; `None` = unset, a lower tier decides.
    #[serde(default)]
    pub allow_private_fetch: Option<bool>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct MemoryConfig {
    /// Long-term memory (DESIGN.md "Memory"). On by default: the
    /// read path costs nothing until the store has entries (an empty
    /// index renders zero tokens), and the anti-evidence targeted
    /// unbounded-injection architectures gertie did not build. Off
    /// disables the session-end extraction AND dreaming; the
    /// memory/search_memory tools stay available either way.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// The frozen index's token budget (roughly; chars/4). Over budget,
    /// whole sections drop, lowest priority first.
    #[serde(default)]
    pub index_budget: Option<usize>,
    /// Extraction model: the name of an endpoint to run the
    /// session-end extraction call on. Absent = the session's own
    /// endpoint.
    #[serde(default)]
    pub extract_endpoint: Option<String>,
    /// Apply proposed ops without the review card. Off = the default
    /// review inbox: proposals render as diffs you approve or deny.
    #[serde(default)]
    pub auto_apply: Option<bool>,
    /// Dreaming cadence: consolidate (merge duplicates, archive
    /// counter-dead entries, rebuild the index) at most this often,
    /// riding the startup sweep. Absent or zero = never.
    #[serde(default)]
    pub dream_days: Option<u64>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct PermissionsConfig {
    /// New sessions' permission mode: "supervised", "accept_edits",
    /// "auto" (sandboxed work runs unasked), or "full_access". A
    /// session keeps whatever mode you set in the composer, per
    /// session; this only sets the starting posture. `Some` = the
    /// user's file set it; `None` = unset, a lower tier decides.
    #[serde(default)]
    pub default_mode: Option<gertie_core::PermissionMode>,
}

/// Explicit inheritance as a value: a project overlay field may hold
/// a value, or the string `"inherit"`, the next tier (the user's
/// config, then the built-in default) decides. Inheritance is a
/// state you can select and the UI can show, not an absence to
/// infer.
#[derive(Clone, Debug, PartialEq)]
pub enum Inheritable<T> {
    Set(T),
    Inherit,
}

impl<T> serde::Serialize for Inheritable<T>
where
    T: serde::Serialize,
{
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Inheritable::Set(value) => value.serialize(serializer),
            Inheritable::Inherit => "inherit".serialize(serializer),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Inheritable<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = toml::Value::deserialize(deserializer)?;
        if value.as_str() == Some("inherit") {
            return Ok(Inheritable::Inherit);
        }
        T::deserialize(value)
            .map(Inheritable::Set)
            .map_err(|error| {
                serde::de::Error::custom(format!("expected a value or \"inherit\": {error}"))
            })
    }
}

/// The repo's own overlay: `<repo>/.gertie/config.toml`. Deliberately
/// a subset, posture, not process: no endpoints, keys, MCP servers,
/// or agent types, because a cloned repo never gets to configure what
/// runs on your machine. The settle command is *not* here either: a
/// shell command is process, not posture, it stays the user's.
/// The overlay sits below the user's config: it suggests, you decide.
#[derive(Deserialize, Debug, Clone, Default)]
pub struct ProjectOverlay {
    #[serde(default)]
    pub permissions: Option<ProjectPermissions>,
    #[serde(default)]
    pub security: Option<ProjectSecurity>,
    #[serde(default)]
    pub auto_settle_days: Option<Inheritable<u64>>,
    #[serde(default)]
    pub worktree: Option<ProjectWorktree>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct ProjectPermissions {
    #[serde(default)]
    pub default_mode: Option<Inheritable<gertie_core::PermissionMode>>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct ProjectSecurity {
    #[serde(default)]
    pub sandbox_bash: Option<Inheritable<bool>>,
    #[serde(default)]
    pub allow_private_fetch: Option<Inheritable<bool>>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct ProjectWorktree {
    /// The prefix on the branch a worktree tab's session names from
    /// its first message ("feat/", "gertie/"). Repo-owned policy;
    /// validated, not rewritten, anything git-hostile falls back to
    /// "gertie/".
    #[serde(default)]
    pub branch_prefix: Option<String>,
}

/// Where a resolved setting's value came from, the label the
/// settings panel and doctor show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The user's config set it: the top tier, always wins.
    User,
    /// The repo's overlay set it: your config didn't, so the project
    /// decided.
    Project,
    /// Nothing set it: the built-in default stands.
    Default,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::User => "user config",
            Origin::Project => "project overlay",
            Origin::Default => "built-in default",
        }
    }
}

/// A resolved setting: the value in force and where it came from.
#[derive(Clone, Debug)]
pub struct Resolved<T> {
    pub value: T,
    pub origin: Origin,
}

/// Every overlay-resolvable field's origin at once, for the
/// settings summary and the doctor's report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Origins {
    pub sandbox_bash: Option<Origin>,
    pub allow_private_fetch: Option<Origin>,
    pub default_mode: Option<Origin>,
    pub auto_settle_days: Option<Origin>,
    pub settle_command: Option<Origin>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct SearchConfig {
    /// "brave" or "searxng".
    pub backend: String,
    /// Env var holding a Brave API key.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// SearXNG instance base URL (must allow format=json).
    #[serde(default)]
    pub base_url: Option<String>,
}

impl SearchConfig {
    /// Resolves to a configured WebSearch, or None with a plain reason.
    pub fn to_backend(&self) -> Result<gertie_tools::web::WebSearch, String> {
        match self.backend.as_str() {
            "brave" => {
                let key = self
                    .api_key_env
                    .as_deref()
                    .and_then(|name| std::env::var(name).ok())
                    .ok_or_else(|| {
                        "brave backend needs api_key_env set and the variable present".to_string()
                    })?;
                Ok(gertie_tools::web::WebSearch::brave(key))
            }
            "searxng" => {
                let url = self
                    .base_url
                    .clone()
                    .ok_or_else(|| "searxng backend needs base_url".to_string())?;
                Ok(gertie_tools::web::WebSearch::searxng(url))
            }
            other => Err(format!("unknown search backend: {other} (brave | searxng)")),
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct AgentConfig {
    /// Slug; the tool is `agent_{name}`.
    pub name: String,
    /// Triggers only ("Use when…"), never a workflow summary. Context
    /// load paid on every leader turn.
    pub description: String,
    /// `model` (default) = the leader may spawn it; `user` = future UI
    /// dispatch, registered but not offered to the model.
    #[serde(default)]
    pub invocation: Invocation,
    /// Model override on the same endpoint (None = the endpoint's model).
    #[serde(default)]
    pub model: Option<String>,
    /// Named endpoint (`[[endpoints]].name`) this worker talks to, a
    /// different provider or wire than the leader's. None = the leader's
    /// endpoint. This is what makes a multi-model council possible.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// The worker's system prompt. A sane default when unset.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Tool-name allowlist; None = everything except agent tools.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
}

#[derive(Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Invocation {
    #[default]
    Model,
    User,
}

#[derive(Deserialize, Debug, Clone)]
pub struct McpServerConfig {
    /// Provenance label; tool names become `mcp_{name}_{tool}`.
    pub name: String,
    /// stdio transport: the process to spawn. Absent when `url` is set.
    #[serde(default)]
    pub command: Option<String>,
    /// Streamable HTTP transport: the server's MCP endpoint URL. One
    /// of `url` or `command` is required; `url` wins if both appear.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the server process (stdio only).
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct EndpointConfig {
    pub name: String,
    /// e.g. `https://api.ppq.ai/v1`
    pub base_url: String,
    pub model: String,
    /// Which wire protocol to speak: `openai` (default) or `anthropic`.
    #[serde(default)]
    pub wire: Option<String>,
    /// Literal key (handy at home; prefer the env variants).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Env var to read the key from, e.g. `PPQ_API_KEY`.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Model context window in tokens. Unset means compaction stays
    /// disabled, gertie doesn't guess windows it doesn't know.
    #[serde(default)]
    pub context_window: Option<u64>,
}

/// `~` or `~/…` at the start of an endpoint's base_url expands to the
/// home directory, a replay endpoint points at a scenario directory
/// on this machine, not at a network host. Everything else (https://…)
/// passes through untouched; `$HOME` unset leaves the value as written.
fn expand_tilde(base_url: &str) -> String {
    let starts_with_home = base_url == "~" || base_url.starts_with("~/");
    if starts_with_home
        && let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return format!("{home}{}", &base_url[1..]);
    }
    base_url.to_string()
}

impl EndpointConfig {
    /// The endpoint with its key resolved: `api_key_env`, then the
    /// conventional `GERTIE_<NAME>_API_KEY` / `<NAME>_API_KEY`
    /// variables, then the literal config value.
    pub fn resolve(&self) -> gertie_providers::Endpoint {
        let api_key = self
            .api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .or_else(|| {
                std::env::var(format!(
                    "GERTIE_{}_API_KEY",
                    self.name.to_uppercase().replace([' ', '-'], "_")
                ))
                .ok()
            })
            .or_else(|| {
                std::env::var(format!(
                    "{}_API_KEY",
                    self.name.to_uppercase().replace([' ', '-'], "_")
                ))
                .ok()
            })
            .or_else(|| self.api_key.clone());

        gertie_providers::Endpoint {
            name: self.name.clone(),
            base_url: expand_tilde(&self.base_url),
            model: self.model.clone(),
            api_key,
            context_window: self.context_window,
            wire: self
                .wire
                .as_deref()
                .and_then(gertie_providers::Wire::parse)
                .unwrap_or_default(),
        }
    }

    /// Where the key comes from, for `gertie doctor`:
    /// ("env", "PPQ_API_KEY") means the named variable; None means
    /// nothing resolved.
    pub fn key_source(&self) -> Option<(&'static str, String)> {
        if let Some(name) = &self.api_key_env
            && std::env::var(name).is_ok_and(|value| !value.is_empty())
        {
            return Some(("env", name.clone()));
        }
        let conventional = format!("GERTIE_{}_API_KEY", self.env_name());
        if std::env::var(&conventional).is_ok_and(|value| !value.is_empty()) {
            return Some(("env", conventional));
        }
        if self.api_key.is_some() {
            return Some(("config", self.name.clone()));
        }
        None
    }

    /// The env-var-friendly endpoint name: `my-endpoint` → `MY_ENDPOINT`.
    pub(crate) fn env_name(&self) -> String {
        self.name.to_uppercase().replace([' ', '-'], "_")
    }
}

/// What the settings panel writes back: the full endpoint list, the
/// default, the sandbox flag, the MCP server list, and the agent
/// types. Deliberately whole-section replacement rather than a
/// key-by-key diff, the panel owns these five sections; every other
/// key, table, and comment in the file is untouchable.
#[derive(Debug, Default)]
pub struct ConfigEdits {
    pub endpoints: Vec<EndpointConfig>,
    pub default_endpoint: Option<String>,
    pub sandbox_bash: bool,
    pub memory_enabled: bool,
    pub memory_dream_days: u64,
    pub mcp_servers: Vec<McpServerConfig>,
    pub agents: Vec<AgentConfig>,
    /// The turn's round budgets (None = leave the file's keys alone,
    /// the defaults read through).
    pub max_tool_rounds: Option<usize>,
    pub grace_rounds: Option<usize>,
}

impl ConfigEdits {
    /// Comments above a section header are the user's voice about the
    /// section, when the section is rebuilt wholesale, carry that
    /// prefix decor onto the first new table.
    fn carry_header_comment(root: &toml_edit::Table, key: &str, first: &mut toml_edit::Table) {
        let comment = root
            .get(key)
            .and_then(|item| item.as_array_of_tables())
            .and_then(|array| array.iter().next())
            .and_then(|table| table.decor().prefix().cloned());
        if let Some(comment) = comment {
            first.decor_mut().set_prefix(comment);
        }
    }

    fn apply_to_document(&self, doc: &mut toml_edit::DocumentMut) {
        let root = doc.as_table_mut();

        // Endpoints: the `[[endpoints]]` array is rebuilt wholesale.
        let mut endpoints = toml_edit::ArrayOfTables::new();
        for endpoint in &self.endpoints {
            let mut table = toml_edit::Table::new();
            table.insert("name", toml_edit::value(endpoint.name.clone()));
            table.insert("base_url", toml_edit::value(endpoint.base_url.clone()));
            table.insert("model", toml_edit::value(endpoint.model.clone()));
            if let Some(wire) = &endpoint.wire {
                table.insert("wire", toml_edit::value(wire.clone()));
            }
            if let Some(api_key_env) = &endpoint.api_key_env {
                table.insert("api_key_env", toml_edit::value(api_key_env.clone()));
            }
            if let Some(api_key) = &endpoint.api_key {
                table.insert("api_key", toml_edit::value(api_key.clone()));
            }
            if let Some(window) = endpoint.context_window {
                table.insert("context_window", toml_edit::value(window as i64));
            }
            endpoints.push(table);
        }
        if let Some(first) = endpoints.iter_mut().next() {
            Self::carry_header_comment(root, "endpoints", first);
        }
        root.insert("endpoints", toml_edit::Item::ArrayOfTables(endpoints));

        // The default: a name points at an endpoint; nothing means
        // "first one wins", so the key is removed rather than written.
        match &self.default_endpoint {
            Some(name) => {
                root.insert("default_endpoint", toml_edit::value(name.clone()));
            }
            None => {
                root.remove("default_endpoint");
            }
        }

        // Memory posture: the panel owns two keys of the [memory]
        // table (the switch and the cadence). The table is mutated in
        // place, so hand-written keys (index_budget, extract_endpoint,
        // auto_apply) survive a panel save untouched.
        if !matches!(root.get("memory"), Some(toml_edit::Item::Table(_))) {
            root.insert("memory", toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let Some(toml_edit::Item::Table(memory)) = root.get_mut("memory") else {
            unreachable!("just inserted as a table")
        };
        memory.insert("enabled", toml_edit::value(self.memory_enabled));
        if self.memory_dream_days > 0 {
            memory.insert(
                "dream_days",
                toml_edit::value(self.memory_dream_days as i64),
            );
        } else {
            // Zero means never: remove the key so the default (7)
            // reads through, the panel switch off is the intent.
            memory.remove("dream_days");
        }

        // Turn budgets: the panel owns two keys of the [turn] table,
        // same mutate-in-place contract as [memory] (hand-written
        // keys survive; None = the panel doesn't own the key, the
        // file's value and the built-in default read through).
        if !matches!(root.get("turn"), Some(toml_edit::Item::Table(_))) {
            root.insert("turn", toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let Some(toml_edit::Item::Table(turn)) = root.get_mut("turn") else {
            unreachable!("just inserted as a table")
        };
        if let Some(rounds) = self.max_tool_rounds {
            turn.insert("max_tool_rounds", toml_edit::value(rounds as i64));
        }
        if let Some(rounds) = self.grace_rounds {
            turn.insert("grace_rounds", toml_edit::value(rounds as i64));
        }

        // Security posture: the flag lives under [security]; a
        // non-table `security` key is pathological, replaced outright.
        if !matches!(root.get("security"), Some(toml_edit::Item::Table(_))) {
            root.insert("security", toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let Some(toml_edit::Item::Table(security)) = root.get_mut("security") else {
            unreachable!("just normalized security to a table");
        };
        security.insert("sandbox_bash", toml_edit::value(self.sandbox_bash));

        // MCP servers: same whole-array replacement as endpoints.
        let mut servers = toml_edit::ArrayOfTables::new();
        for server in &self.mcp_servers {
            let mut table = toml_edit::Table::new();
            table.insert("name", toml_edit::value(server.name.clone()));
            if let Some(url) = &server.url {
                table.insert("url", toml_edit::value(url.clone()));
            }
            if let Some(command) = &server.command {
                table.insert("command", toml_edit::value(command.clone()));
            }
            if !server.args.is_empty() {
                table.insert(
                    "args",
                    toml_edit::value(server.args.iter().cloned().collect::<toml_edit::Array>()),
                );
            }
            servers.push(table);
        }
        if let Some(first) = servers.iter_mut().next() {
            Self::carry_header_comment(root, "mcp_servers", first);
        }
        root.insert("mcp_servers", toml_edit::Item::ArrayOfTables(servers));

        // Agents: same whole-array replacement. Every field the panel
        // knows about is written back, a hand-written `invocation`
        // survives because the draft carries it, not because toml_edit
        // round-trips the table.
        let mut agents = toml_edit::ArrayOfTables::new();
        for agent in &self.agents {
            let mut table = toml_edit::Table::new();
            table.insert("name", toml_edit::value(agent.name.clone()));
            table.insert("description", toml_edit::value(agent.description.clone()));
            if agent.invocation == Invocation::User {
                table.insert("invocation", toml_edit::value("user".to_string()));
            }
            if let Some(model) = &agent.model {
                table.insert("model", toml_edit::value(model.clone()));
            }
            if let Some(endpoint) = &agent.endpoint {
                table.insert("endpoint", toml_edit::value(endpoint.clone()));
            }
            if let Some(prompt) = &agent.prompt {
                table.insert("prompt", toml_edit::value(prompt.clone()));
            }
            if let Some(tools) = &agent.tools {
                table.insert(
                    "tools",
                    toml_edit::value(tools.iter().cloned().collect::<toml_edit::Array>()),
                );
            }
            agents.push(table);
        }
        if let Some(first) = agents.iter_mut().next() {
            Self::carry_header_comment(root, "agents", first);
        }
        root.insert("agents", toml_edit::Item::ArrayOfTables(agents));
    }

    /// Writes these edits to `path`, preserving every comment,
    /// non-replaced key, and unknown section (toml_edit keeps the
    /// document's decor; only the four owned sections are rewritten).
    /// An unparseable existing file is an error, never a clobber.
    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let raw = std::fs::read_to_string(path).unwrap_or_default();
        let mut doc: toml_edit::DocumentMut = raw.parse().map_err(|error| {
            format!(
                "{} does not parse, so it can't be edited from the panel, fix it by hand: {error}",
                path.display()
            )
        })?;
        self.apply_to_document(&mut doc);
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            return Err(format!("could not create {}: {error}", parent.display()));
        }
        write_private(path, &doc.to_string())
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("dev", "gertie", "gertie")
            .map(|dirs| dirs.config_dir().join("config.toml"))
    }

    pub fn data_dir() -> Option<PathBuf> {
        directories::ProjectDirs::from("dev", "gertie", "gertie")
            .map(|dirs| dirs.data_dir().to_owned())
    }

    /// Loads the user config. A missing file is an empty config; a file
    /// that fails to parse is a *reported* error, never a silent fallback,
    /// the caller shows it in the UI.
    pub fn load() -> Result<Self, String> {
        match Self::path() {
            Some(path) => Self::load_from(&path),
            None => Ok(Self::default()),
        }
    }

    /// Loads from an explicit path (the `--config` override). Same
    /// contract as `load`: missing file is an error, parse failure is the
    /// returned error.
    pub fn load_from(path: &std::path::Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        toml::from_str(&raw).map_err(|error| format!("could not parse {}: {error}", path.display()))
    }

    /// Loads the repo's overlay: `<workspace>/.gertie/config.toml`.
    /// A missing file is no overlay and no error; an unreadable or
    /// unparseable one is a reported error (never fatal, never
    /// silent, the label and the doctor carry it).
    pub fn load_project_overlay(workspace_root: &Path) -> (Option<ProjectOverlay>, Option<String>) {
        let path = workspace_root.join(".gertie").join("config.toml");
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (None, None),
            Err(error) => {
                return (
                    None,
                    Some(format!("could not read {}: {error}", path.display())),
                );
            }
        };
        match toml::from_str::<ProjectOverlay>(&raw) {
            Ok(overlay) => {
                // The overlay is the repo speaking, untrusted input.
                // Unknown keys do nothing silently; a typo that does
                // nothing and a key that stopped meaning something both
                // deserve a word. (The user's config accepts unknown
                // keys without complaint: forward compatibility is a
                // feature there, not here.)
                let unknown = Self::unknown_overlay_keys(
                    &toml::from_str::<toml::Value>(&raw)
                        .unwrap_or(toml::Value::Table(Default::default())),
                );
                let warning = match unknown.as_slice() {
                    [] => None,
                    [one] => Some(format!("unrecognized key {one:?}, ignored")),
                    many => Some(format!(
                        "unrecognized keys {}: ignored",
                        many.iter()
                            .map(|key| format!("{key:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                };
                (Some(overlay), warning)
            }
            Err(error) => (
                None,
                Some(format!("could not parse {}: {error}", path.display())),
            ),
        }
    }

    /// The overlay's known keys, for the unknown-key warning. Two
    /// levels: the top-level keys, and the keys of each known section.
    fn unknown_overlay_keys(value: &toml::Value) -> Vec<String> {
        let Some(table) = value.as_table() else {
            return Vec::new();
        };
        let mut unknown = Vec::new();
        for (key, item) in table {
            match key.as_str() {
                "security" => unknown.extend(Self::unknown_section_keys(
                    item,
                    &["sandbox_bash", "allow_private_fetch"],
                )),
                "permissions" => {
                    unknown.extend(Self::unknown_section_keys(item, &["default_mode"]))
                }
                "worktree" => unknown.extend(Self::unknown_section_keys(item, &["branch_prefix"])),
                // auto_settle_days is the leaf that needs no section
                // check; everything else at top level is unrecognized.
                "auto_settle_days" => {}
                other => unknown.push(other.to_string()),
            }
        }
        unknown
    }

    fn unknown_section_keys(item: &toml::Value, known: &[&str]) -> Vec<String> {
        item.as_table()
            .map(|table| {
                table
                    .keys()
                    .filter(|key| !known.contains(&key.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn overlay_sandbox(overlay: Option<&ProjectOverlay>) -> Option<Resolved<bool>> {
        overlay
            .and_then(|overlay| overlay.security.as_ref())
            .and_then(|security| security.sandbox_bash.as_ref())
            .map(|inheritable| match inheritable {
                // Explicit Inherit: the next tier, the built-in
                // default, decides.
                Inheritable::Inherit => Resolved {
                    value: false,
                    origin: Origin::Default,
                },
                Inheritable::Set(value) => Resolved {
                    value: *value,
                    origin: Origin::Project,
                },
            })
    }

    /// The bash sandbox in force: your config, else the repo's
    /// overlay, else off.
    pub fn resolve_sandbox_bash(&self, overlay: Option<&ProjectOverlay>) -> Resolved<bool> {
        if let Some(value) = self.security.sandbox_bash {
            return Resolved {
                value,
                origin: Origin::User,
            };
        }
        Self::overlay_sandbox(overlay).unwrap_or(Resolved {
            value: false,
            origin: Origin::Default,
        })
    }

    fn overlay_fetch(overlay: Option<&ProjectOverlay>) -> Option<Resolved<bool>> {
        overlay
            .and_then(|overlay| overlay.security.as_ref())
            .and_then(|security| security.allow_private_fetch.as_ref())
            .map(|inheritable| match inheritable {
                // Explicit Inherit: the next tier, the built-in
                // default, decides.
                Inheritable::Inherit => Resolved {
                    value: false,
                    origin: Origin::Default,
                },
                Inheritable::Set(value) => Resolved {
                    value: *value,
                    origin: Origin::Project,
                },
            })
    }

    /// The private-fetch gate: your config, else the repo's overlay,
    /// else closed. Open is a *posture* the repo may suggest, its dev
    /// server may be the whole point of the checkout, but only the
    /// user's config or the overlay says so, never a model ask.
    pub fn resolve_allow_private_fetch(&self, overlay: Option<&ProjectOverlay>) -> Resolved<bool> {
        if let Some(value) = self.security.allow_private_fetch {
            return Resolved {
                value,
                origin: Origin::User,
            };
        }
        Self::overlay_fetch(overlay).unwrap_or(Resolved {
            value: false,
            origin: Origin::Default,
        })
    }

    /// The starting permission mode: your config, else the repo's
    /// overlay, else supervised.
    pub fn resolve_default_mode(
        &self,
        overlay: Option<&ProjectOverlay>,
    ) -> Resolved<gertie_core::PermissionMode> {
        if let Some(value) = self.permissions.default_mode {
            return Resolved {
                value,
                origin: Origin::User,
            };
        }
        overlay
            .and_then(|overlay| overlay.permissions.as_ref())
            .and_then(|permissions| permissions.default_mode.as_ref())
            .map(|inheritable| match inheritable {
                Inheritable::Inherit => Resolved {
                    value: gertie_core::PermissionMode::default(),
                    origin: Origin::Default,
                },
                Inheritable::Set(value) => Resolved {
                    value: *value,
                    origin: Origin::Project,
                },
            })
            .unwrap_or(Resolved {
                value: gertie_core::PermissionMode::default(),
                origin: Origin::Default,
            })
    }

    /// The auto-settle threshold in force.
    pub fn memory_enabled(&self) -> bool {
        self.memory
            .as_ref()
            .and_then(|m| m.enabled)
            .unwrap_or(false)
    }

    pub fn memory_index_budget(&self) -> usize {
        self.memory
            .as_ref()
            .and_then(|m| m.index_budget)
            .unwrap_or(600)
    }

    pub fn memory_auto_apply(&self) -> bool {
        self.memory
            .as_ref()
            .and_then(|m| m.auto_apply)
            .unwrap_or(false)
    }

    pub fn memory_dream_days(&self) -> u64 {
        self.memory.as_ref().and_then(|m| m.dream_days).unwrap_or(0)
    }

    /// The turn's tool-round budget: the user's value, else the loop's
    /// built-in default (the loop holds the constant; this resolves
    /// config's say).
    pub fn resolve_max_tool_rounds(&self) -> Option<usize> {
        self.turn.as_ref().and_then(|t| t.max_tool_rounds)
    }

    /// The grace window after the wrap-up nudge, same pattern.
    pub fn resolve_grace_rounds(&self) -> Option<usize> {
        self.turn.as_ref().and_then(|t| t.grace_rounds)
    }

    pub fn resolve_auto_settle_days(
        &self,
        overlay: Option<&ProjectOverlay>,
    ) -> Resolved<Option<u64>> {
        if let Some(value) = self.auto_settle_days {
            return Resolved {
                value: Some(value),
                origin: Origin::User,
            };
        }
        match overlay.and_then(|overlay| overlay.auto_settle_days.as_ref()) {
            Some(Inheritable::Set(value)) => Resolved {
                value: Some(*value),
                origin: Origin::Project,
            },
            _ => Resolved {
                value: None,
                origin: Origin::Default,
            },
        }
    }

    /// The settle reclaim command in force.
    /// The settle reclaim command in force. The user's config only,
    /// the repo's overlay may not set a shell command (posture, not
    /// process), so the overlay parameter is accepted for chain
    /// symmetry and deliberately unread.
    pub fn resolve_settle_command(
        &self,
        _overlay: Option<&ProjectOverlay>,
    ) -> Resolved<Option<String>> {
        if let Some(value) = &self.settle_command {
            return Resolved {
                value: Some(value.clone()),
                origin: Origin::User,
            };
        }
        Resolved {
            value: None,
            origin: Origin::Default,
        }
    }

    /// Every resolvable field's origin at once.
    pub fn resolve_origins(&self, overlay: Option<&ProjectOverlay>) -> Origins {
        Origins {
            sandbox_bash: Some(self.resolve_sandbox_bash(overlay).origin),
            allow_private_fetch: Some(self.resolve_allow_private_fetch(overlay).origin),
            default_mode: Some(self.resolve_default_mode(overlay).origin),
            auto_settle_days: Some(self.resolve_auto_settle_days(overlay).origin),
            settle_command: Some(self.resolve_settle_command(overlay).origin),
        }
    }

    /// The branch prefix the repo's overlay asks for on worktree
    /// branches, "gertie/" when it says nothing (or something
    /// git-hostile).
    pub fn overlay_branch_prefix(overlay: Option<&ProjectOverlay>) -> String {
        crate::worktree::sane_prefix(
            overlay
                .and_then(|overlay| overlay.worktree.as_ref())
                .and_then(|worktree| worktree.branch_prefix.as_deref())
                .unwrap_or("gertie/"),
        )
    }

    /// The endpoint gertie talks to, with its key resolved from the
    /// environment or the config value. None if nothing is configured.
    pub fn selected_endpoint(&self) -> Option<gertie_providers::Endpoint> {
        let chosen = self
            .default_endpoint
            .as_ref()
            .and_then(|name| {
                self.endpoints
                    .iter()
                    .find(|endpoint| &endpoint.name == name)
            })
            .or_else(|| self.endpoints.first())?;

        Some(chosen.resolve())
    }

    /// Every configured endpoint, keys resolved, in fallback order: the
    /// selected one first, then the rest in listed order. When an
    /// endpoint dies before producing content, the provider walks this
    /// list.
    pub fn resolved_endpoints(&self) -> Vec<gertie_providers::Endpoint> {
        let mut ordered = Vec::new();
        if let Some(first) = self.selected_endpoint() {
            ordered.push(first);
        }
        for endpoint in &self.endpoints {
            let resolved = endpoint.resolve();
            if !ordered
                .iter()
                .any(|existing| existing.name == resolved.name)
            {
                ordered.push(resolved);
            }
        }
        ordered
    }

    /// Keys sitting in plaintext config are allowed but get called out,
    /// and a world-readable config file is flagged (DESIGN.md: warn +
    /// 0600, never silently accept).
    pub fn key_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self
            .endpoints
            .iter()
            .any(|endpoint| endpoint.api_key.is_some())
        {
            warnings
                .push("a literal API key is set in config.toml, prefer api_key_env".to_string());
        }
        if let Some(path) = Self::path()
            && let Ok(metadata) = std::fs::metadata(&path)
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                warnings.push(format!(
                    "config.toml is readable by others (mode {mode:o}), chmod 600 it"
                ));
            }
        }
        for endpoint in &self.endpoints {
            if let Some(wire) = &endpoint.wire
                && gertie_providers::Wire::parse(wire).is_none()
            {
                warnings.push(format!(
                    "endpoint {}: unknown wire {:?}, speaking openai; \
                  valid wires are openai | anthropic | replay",
                    endpoint.name, wire
                ));
            }
        }
        // An agent pinned to an endpoint that isn't configured is skipped
        // at bootstrap, say so here, where the fix is still one edit away.
        for agent in &self.agents {
            if let Some(name) = &agent.endpoint
                && !self.endpoints.iter().any(|endpoint| &endpoint.name == name)
            {
                warnings.push(format!(
                    "agent {}: endpoint {name:?} is not configured, the agent is \
                 skipped; add an [[endpoints]] entry with that name",
                    agent.name
                ));
            }
        }
        // A key that resolves to nothing means a guaranteed 401 on the
        // first request, say so up front, with the exact fix, not after.
        if let Some(selected) = self.selected_endpoint_config()
            && selected.resolve().api_key.is_none()
        {
            let variable = selected
                .api_key_env
                .clone()
                .unwrap_or_else(|| format!("GERTIE_{}_API_KEY", selected.env_name()));
            warnings.push(format!(
                "endpoint {}: no API key resolved, export {variable}=<your key> and restart",
                selected.name
            ));
        }
        warnings
    }

    /// The endpoint the provider talks to, pre-resolution.
    pub(crate) fn selected_endpoint_config(&self) -> Option<&EndpointConfig> {
        self.default_endpoint
            .as_ref()
            .and_then(|name| {
                self.endpoints
                    .iter()
                    .find(|endpoint| &endpoint.name == name)
            })
            .or_else(|| self.endpoints.first())
    }

    /// First-run experience: if the default config file does not
    /// exist, write a starter and return the message to surface in
    /// the status line. The starter names nothing, no endpoint,
    /// provider, model, or key is built in; the comments point at the
    /// in-app settings and a generic shape. Never called for an
    /// explicit `--config` path, an override pointing at nothing
    /// means the user knows what they're doing.
    pub fn ensure_skeleton(path: &std::path::Path) -> Option<String> {
        if path.exists() {
            return None;
        }
        let skeleton = r#"# gertie's config. Nothing here is pre-configured: endpoints, models,
# and keys are all yours to name. The easiest path is in the app,
# type /settings in the composer (the settings panel owns endpoints
# and keys). Or add one by hand, any OpenAI-compatible server:
#
# [[endpoints]]
# name = "mine"
# base_url = "https://your-provider.example/v1"
# model = "the-model-id"
# api_key_env = "MY_API_KEY"
#
# Full options live in config.example.toml next to the README: wire
# selection (openai | anthropic), fallback endpoints, MCP servers,
# agents, search backends, sandboxing. `gertie doctor` checks the setup.
"#;
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            return Some(format!(
                "could not create starter config: {error}, config dir {}",
                parent.display()
            ));
        }
        match write_private(path, skeleton) {
            Ok(()) => Some(format!(
                "starter config created at {}, add an endpoint: /settings in the app, or edit the file",
                path.display()
            )),
            Err(error) => Some(format!("could not write starter config: {error}")),
        }
    }
}

/// Writes a config file owner-only: the file may hold a literal API
/// key, and a umask is not a promise, the reader warns on loose
/// modes, so the writer must never create one. 0600 at creation
/// (umask can only strip bits, never add them) and by decree for a
/// pre-existing file, whose mode `open` would otherwise keep.
fn write_private(path: &std::path::Path, contents: &str) -> Result<(), String> {
    use std::io::Write as _;
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|error| format!("could not write {}: {error}", path.display()))?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    file.write_all(contents.as_bytes())
        .map_err(|error| format!("could not write {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(name: &str, model: &str) -> EndpointConfig {
        EndpointConfig {
            name: name.into(),
            base_url: format!("https://{name}.example.com/v1"),
            model: model.into(),
            wire: None,
            api_key: None,
            api_key_env: Some(format!("{name}_KEY")),
            context_window: None,
        }
    }

    /// Explicit Inherit parses as a value-state, not an absence,
    /// and a typo'd marker is an error, never a silent stand-down.
    #[test]
    fn inheritable_holds_a_value_or_the_inherit_marker() {
        #[derive(Deserialize, PartialEq, Debug)]
        struct Field {
            days: Inheritable<u64>,
        }
        let set: Field = toml::from_str("days = 3").unwrap();
        assert_eq!(set.days, Inheritable::Set(3));
        let inherit: Field = toml::from_str("days = \"inherit\"").unwrap();
        assert_eq!(inherit.days, Inheritable::Inherit);
        assert!(toml::from_str::<Field>("days = \"inheret\"").is_err());
        assert!(toml::from_str::<Field>("days = \"soon\"").is_err());
    }

    /// The chain: your config outranks the repo's overlay, which
    /// outranks the built-in default; explicit "inherit" in the
    /// overlay stands down to the default.
    #[test]
    fn settings_resolve_user_over_project_over_default() {
        let overlay: ProjectOverlay = toml::from_str(
            r#"
auto_settle_days = 3

[security]
sandbox_bash = true
allow_private_fetch = true

[permissions]
default_mode = "accept_edits"
"#,
        )
        .unwrap();

        // Nothing anywhere: the defaults, named as defaults.
        let bare = Config::default();
        let resolved = bare.resolve_sandbox_bash(None);
        assert_eq!((resolved.value, resolved.origin), (false, Origin::Default));
        assert_eq!(
            bare.resolve_origins(None),
            Origins {
                sandbox_bash: Some(Origin::Default),
                allow_private_fetch: Some(Origin::Default),
                default_mode: Some(Origin::Default),
                auto_settle_days: Some(Origin::Default),
                settle_command: Some(Origin::Default),
            }
        );
        // The private-fetch gate: closed by default.
        assert!(!bare.resolve_allow_private_fetch(None).value);

        // No user config: the project's values win, labeled as the
        // project's.
        let empty_user = Config::default();
        let resolved = empty_user.resolve_sandbox_bash(Some(&overlay));
        assert_eq!((resolved.value, resolved.origin), (true, Origin::Project));
        let resolved = empty_user.resolve_default_mode(Some(&overlay));
        assert_eq!(
            (resolved.value, resolved.origin),
            (gertie_core::PermissionMode::AcceptEdits, Origin::Project)
        );
        let resolved = empty_user.resolve_auto_settle_days(Some(&overlay));
        assert_eq!(
            (resolved.value, resolved.origin),
            (Some(3), Origin::Project)
        );
        // The private-fetch gate is posture: the repo's overlay may
        // open it, the user's config still outranks it.
        let resolved = empty_user.resolve_allow_private_fetch(Some(&overlay));
        assert_eq!((resolved.value, resolved.origin), (true, Origin::Project));

        // The settle command is the user's alone, an overlay that
        // writes one is ignored (a shell command is process, not
        // posture; a cloned repo never configures what runs).
        let resolved = empty_user.resolve_settle_command(Some(&overlay));
        assert_eq!(
            (resolved.value.as_deref(), resolved.origin),
            (None, Origin::Default)
        );

        // The user's config outranks the overlay, even when it sets
        // the opposite.
        let mut user = Config::default();
        user.security.sandbox_bash = Some(false);
        user.permissions.default_mode = Some(gertie_core::PermissionMode::FullAccess);
        let resolved = user.resolve_sandbox_bash(Some(&overlay));
        assert_eq!((resolved.value, resolved.origin), (false, Origin::User));
        let resolved = user.resolve_default_mode(Some(&overlay));
        assert_eq!(
            (resolved.value, resolved.origin),
            (gertie_core::PermissionMode::FullAccess, Origin::User)
        );

        // Explicit "inherit" in the overlay stands down to the
        // default, even though the field is present.
        let overlay_inherit: ProjectOverlay =
            toml::from_str("[security]\nsandbox_bash = \"inherit\"").unwrap();
        let resolved = empty_user.resolve_sandbox_bash(Some(&overlay_inherit));
        assert_eq!((resolved.value, resolved.origin), (false, Origin::Default));
    }

    /// The overlay loads from `<workspace>/.gertie/config.toml`:
    /// missing is no overlay and no error; broken is a reported
    /// error, never fatal.
    #[test]
    fn the_project_overlay_loads_missing_ok_and_broken_loudly() {
        let root = tempfile::tempdir().unwrap();
        let (overlay, error) = Config::load_project_overlay(root.path());
        assert!(overlay.is_none());
        assert!(error.is_none());

        std::fs::create_dir_all(root.path().join(".gertie")).unwrap();
        std::fs::write(
            root.path().join(".gertie/config.toml"),
            "[security]\nsandbox_bash = true\n",
        )
        .unwrap();
        let (overlay, error) = Config::load_project_overlay(root.path());
        assert!(error.is_none());
        let overlay = overlay.expect("the overlay loads");
        assert_eq!(
            overlay
                .security
                .as_ref()
                .and_then(|s| s.sandbox_bash.as_ref()),
            Some(&Inheritable::Set(true))
        );

        // A broken overlay file is a reported error, not a crash and
        // not a silent absence.
        std::fs::write(root.path().join(".gertie/config.toml"), "not [valid toml").unwrap();
        let (overlay, error) = Config::load_project_overlay(root.path());
        assert!(overlay.is_none());
        assert!(
            error
                .expect("the parse error surfaces")
                .contains("could not parse")
        );
    }

    /// Unknown keys in the repo's overlay are named, not swallowed,
    /// the file is untrusted input, and a typo that does nothing (or a
    /// key that no longer means anything) should say so.
    #[test]
    fn the_overlay_names_keys_it_does_not_know() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".gertie")).unwrap();
        // `settle_command` is a key the overlay once had and must never
        // mean again; `nonsense` is a plain typo.
        std::fs::write(
            root.path().join(".gertie/config.toml"),
            "auto_settle_days = 3\nsettle_command = \"cargo clean\"\n\n[security]\nsandbox_bash = true\nnonsense = 1\n",
        )
        .unwrap();
        let (overlay, warning) = Config::load_project_overlay(root.path());
        assert!(overlay.is_some(), "the known keys still load");
        let warning = warning.expect("the unknown keys are named");
        assert!(warning.contains("settle_command"), "{warning}");
        assert!(warning.contains("nonsense"), "{warning}");

        // A clean overlay is quiet.
        std::fs::write(
            root.path().join(".gertie/config.toml"),
            "[security]\nsandbox_bash = true\n",
        )
        .unwrap();
        let (_, warning) = Config::load_project_overlay(root.path());
        assert!(warning.is_none());
    }

    /// The edited sections are rewritten, the rest of the file is
    /// preserved byte-for-byte in spirit: comments, ordering, unknown
    /// keys.
    #[test]
    fn edits_preserve_comments_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# my personal endpoint, kept by hand\n[[endpoints]]\nname = \"ppq\"\n\
             base_url = \"https://api.ppq.ai/v1\"\nmodel = \"glm-5.3-flash\"\n\
             api_key_env = \"PPQ_API_KEY\"\n\n[extra]\nanything = true\n",
        )
        .unwrap();

        ConfigEdits {
            endpoints: vec![endpoint("other", "m2")],
            default_endpoint: Some("other".into()),
            sandbox_bash: true,
            ..Default::default()
        }
        .save(&path)
        .unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# my personal endpoint"), "comment kept");
        assert!(raw.contains("[extra]"), "unknown section kept");
        assert!(raw.contains("anything = true"));

        // And the written file still parses as a real config.
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.endpoints.len(), 1);
        assert_eq!(config.endpoints[0].name, "other");
        assert_eq!(config.default_endpoint.as_deref(), Some("other"));
        assert_eq!(config.security.sandbox_bash, Some(true));
    }

    /// The [turn] table: the panel owns two keys, mutates in place
    /// (hand-written keys survive), and None leaves the keys alone.
    #[test]
    fn edits_own_turn_keys_and_preserve_hand_written_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# my turn budgets, kept by hand\n[turn]\nmax_tool_rounds = 40\n\
             warn_at_percent = 70\n",
        )
        .unwrap();

        // Saving with None (the panel doesn't own the keys) leaves
        // the file's values and the hand-written extra key intact.
        ConfigEdits {
            max_tool_rounds: None,
            grace_rounds: None,
            ..Default::default()
        }
        .save(&path)
        .unwrap();
        let config = Config::load_from(&path).unwrap();
        assert_eq!(
            config.turn.as_ref().and_then(|t| t.max_tool_rounds),
            Some(40),
            "None leaves the file's value"
        );
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("warn_at_percent"), "hand-written key kept");
        assert!(raw.contains("# my turn budgets"), "comment kept");

        // Saving with values writes them; the file re-parses.
        ConfigEdits {
            max_tool_rounds: Some(60),
            grace_rounds: Some(8),
            ..Default::default()
        }
        .save(&path)
        .unwrap();
        let config = Config::load_from(&path).unwrap();
        assert_eq!(
            config.turn.as_ref().and_then(|t| t.max_tool_rounds),
            Some(60)
        );
        assert_eq!(config.turn.as_ref().and_then(|t| t.grace_rounds), Some(8));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("warn_at_percent = 70"), "still kept");
    }

    #[test]
    fn edits_add_change_and_delete_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        ConfigEdits {
            endpoints: vec![endpoint("ppq", "glm-5.3-flash")],
            default_endpoint: Some("ppq".into()),
            ..Default::default()
        }
        .save(&path)
        .unwrap();

        // Add a second endpoint, switch the default to it.
        let mut second = endpoint("local", "qwen3-coder");
        second.base_url = "http://127.0.0.1:11434/v1".into();
        second.wire = Some("openai".into());
        ConfigEdits {
            endpoints: vec![endpoint("ppq", "glm-5.3-flash"), second],
            default_endpoint: Some("local".into()),
            ..Default::default()
        }
        .save(&path)
        .unwrap();
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.selected_endpoint().unwrap().name, "local");
        assert_eq!(config.resolved_endpoints().len(), 2);

        // Deleting everything removes the default too, "first one
        // wins" is the absence semantics, not a stale name.
        ConfigEdits::default().save(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("default_endpoint"));
        let config = Config::load_from(&path).unwrap();
        assert!(config.endpoints.is_empty());
        assert!(config.selected_endpoint().is_none());
    }

    #[test]
    fn edits_write_mcp_servers_and_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        ConfigEdits {
            sandbox_bash: true,
            mcp_servers: vec![McpServerConfig {
                name: "docs".into(),
                url: Some("https://mcp.example.com/mcp".into()),
                command: None,
                args: vec!["--verbose".into()],
                env: Default::default(),
            }],
            ..Default::default()
        }
        .save(&path)
        .unwrap();

        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.security.sandbox_bash, Some(true));
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].name, "docs");
        assert_eq!(
            config.mcp_servers[0].url.as_deref(),
            Some("https://mcp.example.com/mcp")
        );
        assert_eq!(config.mcp_servers[0].args, vec!["--verbose"]);
    }

    /// An unparseable file must never be clobbered by a save, the
    /// user's hand-edited config survives a bad paste.
    #[test]
    fn save_refuses_to_edit_an_unparseable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[[endpoints]\nname = broken").unwrap();
        let result = ConfigEdits::default().save(&path);
        assert!(result.is_err(), "unparseable file is refused");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[[endpoints]\nname = broken",
            "the file is untouched"
        );
    }

    /// The panel owns [turn]: its two keys save, and a hand-written
    /// budget the panel doesn't own this round (none today, but the
    /// contract is mutate-in-place) survives.
    #[test]
    fn edits_preserve_hand_written_turn_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[turn]\nmax_tool_rounds = 30\ngrace_rounds = 4\n").unwrap();

        ConfigEdits {
            endpoints: vec![endpoint("ppq", "m1")],
            default_endpoint: Some("ppq".into()),
            sandbox_bash: false,
            memory_enabled: false,
            memory_dream_days: 7,
            max_tool_rounds: Some(90),
            grace_rounds: Some(12),
            ..Default::default()
        }
        .save(&path)
        .unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        // The panel's two keys written, in place, nothing else touched.
        assert!(raw.contains("max_tool_rounds = 90"), "{raw}");
        assert!(raw.contains("grace_rounds = 12"), "{raw}");

        // And a save that owns neither key leaves both keys standing.
        ConfigEdits {
            endpoints: vec![endpoint("ppq", "m1")],
            default_endpoint: Some("ppq".into()),
            sandbox_bash: false,
            memory_enabled: false,
            memory_dream_days: 7,
            ..Default::default()
        }
        .save(&path)
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("max_tool_rounds = 90") && raw.contains("grace_rounds = 12"),
            "a None save leaves the file's turn keys standing: {raw}"
        );
    }

    /// The panel owns the agents section: every field round-trips,
    /// including a hand-written `invocation = "user"`, which the form
    /// doesn't edit but must not drop, and the comment above the
    /// section survives like any other user voice.
    #[test]
    #[test]
    fn edits_preserve_hand_written_memory_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "default_endpoint = \"ppq\"\n\n[endpoints]\nname = \"ppq\"\n\
             model = \"m1\"\n\n[memory]\nenabled = true\nindex_budget = 900\n\
             auto_apply = false\n",
        )
        .unwrap();

        ConfigEdits {
            endpoints: vec![endpoint("ppq", "m1")],
            default_endpoint: Some("ppq".into()),
            sandbox_bash: false,
            memory_enabled: false,
            memory_dream_days: 14,
            ..Default::default()
        }
        .save(&path)
        .unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        // The panel's two keys written, the hand-written ones intact.
        assert!(raw.contains("enabled = false"));
        assert!(raw.contains("dream_days = 14"));
        assert!(
            raw.contains("index_budget = 900"),
            "file-only key kept: {raw}"
        );
        assert!(
            raw.contains("auto_apply = false"),
            "file-only key kept: {raw}"
        );
    }

    fn edits_round_trip_agents_with_pins_and_invocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# my council, kept by hand\n[[agents]]\nname = \"council-two\"\n\
             description = \"Use when a council review runs.\"\ninvocation = \"user\"\n\
             endpoint = \"anthropic\"\nmodel = \"claude-opus-4-6\"\n\
             prompt = \"You are a council seat.\"\ntools = [\"read_file\", \"grep\"]\n",
        )
        .unwrap();

        ConfigEdits {
            agents: vec![AgentConfig {
                name: "council-two".into(),
                description: "Use when a council review runs.".into(),
                invocation: Invocation::User,
                model: Some("claude-opus-4-6".into()),
                endpoint: Some("anthropic".into()),
                prompt: Some("You are a council seat.".into()),
                tools: Some(vec!["read_file".into(), "grep".into()]),
            }],
            ..Default::default()
        }
        .save(&path)
        .unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# my council, kept by hand"), "comment kept");

        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.agents.len(), 1);
        let agent = &config.agents[0];
        assert_eq!(agent.invocation, Invocation::User, "invocation survives");
        assert_eq!(agent.endpoint.as_deref(), Some("anthropic"));
        assert_eq!(agent.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(agent.prompt.as_deref(), Some("You are a council seat."));
        assert_eq!(agent.tools, Some(vec!["read_file".into(), "grep".into()]));

        // And a panel-less edit in the other direction: dropping the
        // agent list removes the section rather than leaving a stub.
        ConfigEdits::default().save(&path).unwrap();
        let config = Config::load_from(&path).unwrap();
        assert!(config.agents.is_empty(), "agents removed wholesale");
    }

    #[test]
    fn skeleton_is_created_once_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // First run: a starter lands, it parses, and it names nothing,
        // no endpoint, provider, model, or key is built in.
        let note = Config::ensure_skeleton(&path).expect("created");
        assert!(note.contains("starter config created"));
        assert!(!note.contains("ppq") && !note.contains("PPQ"));
        let config = Config::load_from(&path).expect("skeleton parses");
        assert!(config.endpoints.is_empty(), "skeleton invents no endpoint");
        assert!(config.selected_endpoint().is_none());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("ppq") && !raw.contains("glm"),
            "skeleton carries no provider or model"
        );

        // Second run: silence, the user's edits are untouchable.
        std::fs::write(&path, "[[endpoints]]\nname = \"mine\"").unwrap();
        assert!(Config::ensure_skeleton(&path).is_none());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("mine"), "skeleton must not overwrite");
    }

    #[test]
    fn missing_key_warning_names_the_variable() {
        let mut config = Config::default();
        config.endpoints.push(EndpointConfig {
            name: "ppq".into(),
            base_url: "https://api.ppq.ai/v1".into(),
            model: "glm-5.3-flash".into(),
            wire: None,
            api_key: None,
            api_key_env: Some("GERTIE_TEST_UNSET_KEY_8347".into()),
            context_window: None,
        });
        config.default_endpoint = Some("ppq".into());
        // No env mutation (unsafe under edition 2024), the variable is
        // simply too improbable for anything to set it.

        let warnings = config.key_warnings();
        let warning = warnings
            .iter()
            .find(|warning| warning.contains("no API key resolved"))
            .expect("warning present");
        assert!(
            warning.contains("export GERTIE_TEST_UNSET_KEY_8347="),
            "the warning names the fix: {warning}"
        );
    }

    /// An agent pinned to an endpoint that isn't configured is a named
    /// warning (the agent gets skipped at bootstrap), never a silent
    /// fallback onto some other provider's model.
    #[test]
    fn agent_endpoints_must_be_configured() {
        let mut config = Config::default();
        config.agents.push(AgentConfig {
            name: "council-two".into(),
            description: "Use when a council review runs.".into(),
            invocation: Default::default(),
            model: None,
            endpoint: Some("nowhere".into()),
            prompt: None,
            tools: None,
        });

        let warnings = config.key_warnings();
        let warning = warnings
            .iter()
            .find(|warning| warning.contains("council-two"))
            .expect("warning present");
        assert!(
            warning.contains("\"nowhere\"") && warning.contains("skipped"),
            "the warning names the agent, the endpoint, and the skip: {warning}"
        );
    }
}
