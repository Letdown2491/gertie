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

#[derive(Deserialize, Debug)]
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
            base_url: self.base_url.clone(),
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
                 valid wires are openai | anthropic",
                    endpoint.name, wire
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
    /// exist, write a working starter (a real endpoint entry that
    /// needs only an API key) and return the message to surface in the
    /// status line. Never called for an explicit `--config` path — an
    /// override pointing at nothing means the user knows what they're
    /// doing.
    pub fn ensure_skeleton(path: &std::path::Path) -> Option<String> {
        if path.exists() {
            return None;
        }
        let skeleton = r#"# gertie's config. Full options live in the docs' config.example.toml:
# endpoints (wire, fallbacks), MCP servers (stdio or HTTP), agents,
# search backends, sandboxing. Keys resolve from api_key_env first.
[[endpoints]]
name = "ppq"
base_url = "https://api.ppq.ai/v1"
model = "glm-5.3-flash"
api_key_env = "PPQ_API_KEY"

# Any OpenAI-compatible endpoint works (OpenRouter, Ollama, vLLM...):
# [[endpoints]]
# name = "local"
# base_url = "http://127.0.0.1:11434/v1"
# model = "qwen3-coder"

# Anthropic-native endpoints speak a different wire:
# wire = "anthropic"

default_endpoint = "ppq"
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
                "starter config created at {} — set PPQ_API_KEY or edit it",
                path.display()
            )),
            Err(error) => Some(format!("could not write starter config: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_is_created_once_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // First run: a starter lands, and it parses as a real config.
        let note = Config::ensure_skeleton(&path).expect("created");
        assert!(note.contains("starter config created"));
        let config = Config::load_from(&path).expect("skeleton parses");
        assert_eq!(config.selected_endpoint().expect("endpoint").name, "ppq");

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
}
