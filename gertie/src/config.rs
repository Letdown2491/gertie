//! Layered TOML config. M1 reads one file; the full layering
//! (defaults → user → project → flags) arrives with the project file.

use serde::Deserialize;
use std::path::PathBuf;

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
    /// Auto-settle chats idle for at least this many days, on startup.
    /// Absent or zero = off: settle is manual until you opt in.
    #[serde(default)]
    pub auto_settle_days: Option<u64>,
    /// Shell command run in a settling session's recorded workspace —
    /// the reclaim hook (e.g. `cargo clean`). Manual settles only;
    /// the auto-settle sweep never runs commands.
    #[serde(default)]
    pub settle_command: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct SecurityConfig {
    /// Landlock-isolate bash children: system reads, workspace//tmp
    /// writes only. Opt-in; when the kernel can't enforce it, sandboxed
    /// commands fail instead of running free.
    #[serde(default)]
    pub sandbox_bash: bool,
}

#[allow(clippy::derivable_impls)]
impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            sandbox_bash: false, // opt-in: the permission gate is the base layer
        }
    }
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
    /// Triggers only ("Use when…") — never a workflow summary. Context
    /// load paid on every leader turn.
    pub description: String,
    /// `model` (default) = the leader may spawn it; `user` = future UI
    /// dispatch, registered but not offered to the model.
    #[serde(default)]
    pub invocation: Invocation,
    /// Model override on the same endpoint (None = the endpoint's model).
    #[serde(default)]
    pub model: Option<String>,
    /// Named endpoint (`[[endpoints]].name`) this worker talks to — a
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
    /// disabled — gertie doesn't guess windows it doesn't know.
    #[serde(default)]
    pub context_window: Option<u64>,
}

/// `~` or `~/…` at the start of an endpoint's base_url expands to the
/// home directory — a replay endpoint points at a scenario directory
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
/// key-by-key diff — the panel owns these five sections; every other
/// key, table, and comment in the file is untouchable.
#[derive(Debug, Default)]
pub struct ConfigEdits {
    pub endpoints: Vec<EndpointConfig>,
    pub default_endpoint: Option<String>,
    pub sandbox_bash: bool,
    pub mcp_servers: Vec<McpServerConfig>,
    pub agents: Vec<AgentConfig>,
}

impl ConfigEdits {
    /// Comments above a section header are the user's voice about the
    /// section — when the section is rebuilt wholesale, carry that
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
        // knows about is written back — a hand-written `invocation`
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
                "{} does not parse, so it can't be edited from the panel — fix it by hand: {error}",
                path.display()
            )
        })?;
        self.apply_to_document(&mut doc);
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            return Err(format!("could not create {}: {error}", parent.display()));
        }
        std::fs::write(path, doc.to_string())
            .map_err(|error| format!("could not write {}: {error}", path.display()))
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
    /// that fails to parse is a *reported* error, never a silent fallback —
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
                .push("a literal API key is set in config.toml — prefer api_key_env".to_string());
        }
        if let Some(path) = Self::path()
            && let Ok(metadata) = std::fs::metadata(&path)
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                warnings.push(format!(
                    "config.toml is readable by others (mode {mode:o}) — chmod 600 it"
                ));
            }
        }
        for endpoint in &self.endpoints {
            if let Some(wire) = &endpoint.wire
                && gertie_providers::Wire::parse(wire).is_none()
            {
                warnings.push(format!(
                    "endpoint {}: unknown wire {:?} — speaking openai; \
                  valid wires are openai | anthropic | replay",
                    endpoint.name, wire
                ));
            }
        }
        // An agent pinned to an endpoint that isn't configured is skipped
        // at bootstrap — say so here, where the fix is still one edit away.
        for agent in &self.agents {
            if let Some(name) = &agent.endpoint
                && !self.endpoints.iter().any(|endpoint| &endpoint.name == name)
            {
                warnings.push(format!(
                    "agent {}: endpoint {name:?} is not configured — the agent is \
                 skipped; add an [[endpoints]] entry with that name",
                    agent.name
                ));
            }
        }
        // A key that resolves to nothing means a guaranteed 401 on the
        // first request — say so up front, with the exact fix, not after.
        if let Some(selected) = self.selected_endpoint_config()
            && selected.resolve().api_key.is_none()
        {
            let variable = selected
                .api_key_env
                .clone()
                .unwrap_or_else(|| format!("GERTIE_{}_API_KEY", selected.env_name()));
            warnings.push(format!(
                "endpoint {}: no API key resolved — export {variable}=<your key> and restart",
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
    /// the status line. The starter names nothing — no endpoint,
    /// provider, model, or key is built in; the comments point at the
    /// in-app settings and a generic shape. Never called for an
    /// explicit `--config` path — an override pointing at nothing
    /// means the user knows what they're doing.
    pub fn ensure_skeleton(path: &std::path::Path) -> Option<String> {
        if path.exists() {
            return None;
        }
        let skeleton = r#"# gertie's config. Nothing here is pre-configured: endpoints, models,
# and keys are all yours to name. The easiest path is in the app —
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
                "could not create starter config: {error} — config dir {}",
                parent.display()
            ));
        }
        match std::fs::write(path, skeleton) {
            Ok(()) => Some(format!(
                "starter config created at {} — add an endpoint: /settings in the app, or edit the file",
                path.display()
            )),
            Err(error) => Some(format!("could not write starter config: {error}")),
        }
    }
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
        assert!(config.security.sandbox_bash);
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

        // Deleting everything removes the default too — "first one
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
        assert!(config.security.sandbox_bash);
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].name, "docs");
        assert_eq!(
            config.mcp_servers[0].url.as_deref(),
            Some("https://mcp.example.com/mcp")
        );
        assert_eq!(config.mcp_servers[0].args, vec!["--verbose"]);
    }

    /// An unparseable file must never be clobbered by a save — the
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

    /// The panel owns the agents section: every field round-trips —
    /// including a hand-written `invocation = "user"`, which the form
    /// doesn't edit but must not drop — and the comment above the
    /// section survives like any other user voice.
    #[test]
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

        // First run: a starter lands, it parses, and it names nothing —
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

        // Second run: silence — the user's edits are untouchable.
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
        // No env mutation (unsafe under edition 2024) — the variable is
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
