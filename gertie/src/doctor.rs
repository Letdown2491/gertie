//! `gertie doctor` — the first-run diagnostic. Prints what gertie
//! sees: where the config is, which endpoint and key resolved (and
//! from where), MCP servers, the session ledger. Exit 0 means ready.

use std::path::{Path, PathBuf};

pub fn run(config_override: Option<PathBuf>) -> i32 {
    println!("gertie {}", crate::cli::VERSION);
    println!();

    let mut ready = true;

    // Config file.
    let config = match config_override {
        Some(ref path) => {
            println!("config      {} (from --config)", path.display());
            read_config(path)
        }
        None => match crate::config::Config::path() {
            Some(ref path) => {
                let created = crate::config::Config::ensure_skeleton(path);
                if let Some(message) = &created {
                    println!("config      {message}");
                } else {
                    println!("config      {}", path.display());
                }
                read_config(path)
            }
            None => {
                println!("config      (no config dir found)");
                Ok(crate::config::Config::default())
            }
        },
    };
    let config = match config {
        Ok(config) => config,
        Err(error) => {
            println!("            ✗ {error}");
            println!();
            println!("✗ not ready — fix the config file above");
            return 1;
        }
    };

    // Endpoints and the key.
    let endpoints = config.resolved_endpoints();
    if endpoints.is_empty() {
        println!("endpoint    ✗ none configured");
        ready = false;
    } else {
        let selected = endpoints.first().expect("non-empty");
        let wire = match selected.wire {
            gertie_providers::Wire::OpenAI => "openai",
            gertie_providers::Wire::Anthropic => "anthropic",
        };
        println!(
            "endpoint    {} · {} ({wire}) · {}",
            selected.name, selected.model, selected.base_url
        );
        match config
            .selected_endpoint_config()
            .and_then(|endpoint| endpoint.key_source())
        {
            Some(("env", variable)) => println!("api key     ✓ resolved from env {variable}"),
            Some(_source) => {
                println!("api key     ✓ resolved from config.toml (inline — prefer an env var)")
            }
            None => {
                let variable = config
                    .selected_endpoint_config()
                    .and_then(|endpoint| endpoint.api_key_env.clone())
                    .unwrap_or_else(|| {
                        format!(
                            "GERTIE_{}_API_KEY",
                            config
                                .selected_endpoint_config()
                                .map(|endpoint| endpoint.env_name())
                                .unwrap_or_default()
                        )
                    });
                println!("api key     ✗ missing — export {variable}=<your key> and restart");
                ready = false;
            }
        }
        let fallbacks: Vec<String> = endpoints
            .iter()
            .skip(1)
            .map(|endpoint| endpoint.name.clone())
            .collect();
        if !fallbacks.is_empty() {
            println!("fallbacks   {}", fallbacks.join(", "));
        }
    }

    // MCP servers.
    if config.mcp_servers.is_empty() {
        println!("mcp         none configured");
    } else {
        for server in &config.mcp_servers {
            match (&server.url, &server.command) {
                (Some(url), _) => println!("mcp         {} (http: {url})", server.name),
                (None, Some(command)) => println!("mcp         {} (stdio: {command})", server.name),
                (None, None) => {
                    println!("mcp         {} ✗ needs a url or a command", server.name);
                    ready = false;
                }
            }
        }
    }

    // Agent types — custom `[[agents]]` entries, pins called out. A
    // seat on a missing endpoint is skipped at bootstrap; doctor is
    // where that's spelled out, not at dispatch time.
    if config.agents.is_empty() {
        println!("agents      eight built-ins (no custom types)");
    } else {
        for agent in &config.agents {
            match (&agent.endpoint, &agent.model) {
                (Some(endpoint), Some(model)) => {
                    println!("agent       {} → {endpoint} · {model}", agent.name)
                }
                (Some(endpoint), None) => println!("agent       {} → {endpoint}", agent.name),
                (None, Some(model)) => println!("agent       {} · {model}", agent.name),
                (None, None) => println!("agent       {}", agent.name),
            }
            if let Some(name) = &agent.endpoint
                && !config
                    .endpoints
                    .iter()
                    .any(|endpoint| &endpoint.name == name)
            {
                println!(
                    "            ✗ endpoint {name:?} is not configured — this agent is skipped"
                );
                ready = false;
            }
        }
    }

    // Search, sandbox, sessions.
    match &config.search {
        Some(search) => println!("search      {}", search.backend),
        None => println!("search      none (web_search explains how to set one up)"),
    }
    println!(
        "sandbox     {}",
        if config.security.sandbox_bash {
            "Landlock, opt-in, on"
        } else {
            "off (the permission gate still prompts for writes)"
        }
    );

    if let Some(data_dir) = crate::config::Config::data_dir() {
        let store = data_dir.join("sessions.db");
        if store.exists()
            && let Ok(store) = gertie_core::SessionStore::open(&store)
        {
            let sessions = store.list_sessions().map(|list| list.len()).unwrap_or(0);
            let (input, output) = store.total_usage();
            println!("sessions    {sessions} saved · {input} tokens in / {output} out (all time)");
        } else {
            println!("sessions    none yet");
        }
        println!("data dir    {}", data_dir.display());
    } else {
        println!("sessions    (no data dir found)");
    }

    println!();
    if ready {
        println!("✓ ready — `gertie` for the desktop app, `gertie run \"prompt\"` headlessly");
        0
    } else {
        println!("✗ not ready — fix the ✗ items above");
        1
    }
}

fn read_config(path: &Path) -> Result<crate::config::Config, String> {
    crate::config::Config::load_from(path)
}
