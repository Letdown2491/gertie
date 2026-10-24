//! Settings: the live configuration, visible in-app. gertie's config is
//! a file on purpose (reviewable, versionable, no hidden state) — the
//! panel shows what's actually in force and where to change it, rather
//! than becoming a second source of truth. The one thing the panel does
//! write is the theme choice, which is UI state, not configuration: it
//! lives in its own file under the data dir, never in the user's
//! config.toml.

use gpui_kit::component::ThemeMode;
use serde_json::json;
use std::path::PathBuf;

/// One configured endpoint as the panel shows it.
pub struct EndpointInfo {
    pub name: String,
    pub model: String,
    /// The selected/default endpoint.
    pub active: bool,
    pub key_resolved: bool,
}

/// A snapshot of the effective configuration, computed once at startup.
pub struct SettingsSummary {
    pub endpoints: Vec<EndpointInfo>,
    /// Landlock sandbox for bash children (opt-in).
    pub sandbox: bool,
    /// Search backend in force ("brave", "searxng", or "none").
    pub search: String,
    pub mcp_servers: Vec<String>,
    pub skills: usize,
    /// Model-invocable agent type names.
    pub agents: Vec<String>,
    /// Where the config lives, for the "go change it" footer.
    pub config_path: Option<PathBuf>,
}

impl SettingsSummary {
    /// Reads the effective config. Both the config error path and an
    /// empty config degrade to an honest "nothing configured" panel.
    pub fn from_config(config: &crate::config::Config) -> Self {
        let endpoints = config
            .endpoints
            .iter()
            .map(|endpoint| {
                let selected = config
                    .selected_endpoint()
                    .map(|selected| selected.name == endpoint.name)
                    .unwrap_or(false);
                EndpointInfo {
                    name: endpoint.name.clone(),
                    model: endpoint.model.clone(),
                    active: selected,
                    key_resolved: endpoint.resolve().api_key.is_some(),
                }
            })
            .collect();
        let search = config
            .search
            .as_ref()
            .map(|search| search.backend.clone())
            .unwrap_or_else(|| "none".into());
        Self {
            endpoints,
            sandbox: config.security.sandbox_bash,
            search,
            mcp_servers: config
                .mcp_servers
                .iter()
                .map(|server| server.name.clone())
                .collect(),
            skills: 0, // filled in by bootstrap after discovery
            agents: config
                .agents
                .iter()
                .filter(|agent| agent.invocation == crate::config::Invocation::Model)
                .map(|agent| agent.name.clone())
                .collect(),
            config_path: crate::config::Config::path(),
        }
    }
}

/// The theme choice, persisted separately from the user's config.toml.
fn ui_state_path() -> Option<PathBuf> {
    crate::config::Config::data_dir().map(|dir| dir.join("ui.json"))
}

fn load_theme_from(path: &std::path::Path) -> Option<ThemeMode> {
    let raw = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<serde_json::Value>(&raw)
        .ok()?
        .get("theme")?
        .as_str()?
    {
        "light" => Some(ThemeMode::Light),
        "dark" => Some(ThemeMode::Dark),
        _ => None,
    }
}

pub fn load_theme() -> Option<ThemeMode> {
    load_theme_from(&ui_state_path()?)
}

fn store_theme_at(path: &std::path::Path, mode: ThemeMode) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        return Err(error.to_string());
    }
    let state = json!({ "theme": mode.name() });
    std::fs::write(
        path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&state).unwrap_or_default()
        ),
    )
    .map_err(|error| error.to_string())
}

pub fn store_theme(mode: ThemeMode) {
    let Some(path) = ui_state_path() else {
        return;
    };
    if let Err(error) = store_theme_at(&path, mode) {
        eprintln!("warning: could not save the theme choice: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_choice_round_trips_through_the_ui_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.json");
        assert_eq!(load_theme_from(&path), None, "nothing stored yet");
        store_theme_at(&path, ThemeMode::Light).unwrap();
        assert_eq!(load_theme_from(&path), Some(ThemeMode::Light));
        store_theme_at(&path, ThemeMode::Dark).unwrap();
        assert_eq!(load_theme_from(&path), Some(ThemeMode::Dark));
    }
}
