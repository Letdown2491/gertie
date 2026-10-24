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
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        directories::ProjectDirs::from("dev", "gertie", "gertie")
            .map(|dirs| dirs.config_dir().join("config.toml"))
    }

    pub fn data_dir() -> Option<PathBuf> {
        directories::ProjectDirs::from("dev", "gertie", "gertie").map(|dirs| dirs.data_dir().to_owned())
    }

    /// Loads the user config; missing file is an empty config, not an error.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(raw) => toml::from_str(&raw).unwrap_or_else(|error| {
                eprintln!("warning: could not parse {}: {error}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
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
        })
    }
}
