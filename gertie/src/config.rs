//! Layered TOML config. M1 reads one file; the full layering
//! (defaults → user → project → flags) arrives with the project file.

use serde::Deserialize;
use std::path::PathBuf;

/// The user's config: `~/.config/gertie/config.toml`.
#[derive(Deserialize, Debug, Default)]
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
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the server process.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize, Debug)]
pub struct EndpointConfig {
    pub name: String,
    /// e.g. `https://api.ppq.ai/v1`
    pub base_url: String,
    pub model: String,
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

        let api_key = chosen
            .api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .or_else(|| {
                std::env::var(format!(
                    "GERTIE_{}_API_KEY",
                    chosen.name.to_uppercase().replace([' ', '-'], "_")
                ))
                .ok()
            })
            .or_else(|| {
                std::env::var(format!(
                    "{}_API_KEY",
                    chosen.name.to_uppercase().replace([' ', '-'], "_")
                ))
                .ok()
            })
            .or_else(|| chosen.api_key.clone());

        Some(gertie_providers::Endpoint {
            name: chosen.name.clone(),
            base_url: chosen.base_url.clone(),
            model: chosen.model.clone(),
            api_key,
            context_window: chosen.context_window,
        })
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
        warnings
    }
}
