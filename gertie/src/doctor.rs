//! `gertie doctor`, the first-run diagnostic. Prints what gertie
//! sees: where the config is, which endpoint and key resolved (and
//! from where), MCP servers, the session ledger. Exit 0 means ready.

use std::path::{Path, PathBuf};

pub fn run(config_override: Option<PathBuf>, clean_worktrees: bool) -> i32 {
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
            println!("✗ not ready, fix the config file above");
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
            gertie_providers::Wire::Replay => "replay (scripted)",
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
                println!("api key     ✓ resolved from config.toml (inline, prefer an env var)")
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
                println!("api key     ✗ missing, export {variable}=<your key> and restart");
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

    // Agent types, custom `[[agents]]` entries, pins called out. A
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
                    "            ✗ endpoint {name:?} is not configured, this agent is skipped"
                );
                ready = false;
            }
        }
    }

    // Search, sandbox, sessions. The sandbox resolves through the
    // chain, your config, else the repo's overlay, else off, and
    // the doctor says which tier decided.
    match &config.search {
        Some(search) => println!("search      {}", search.backend),
        None => println!("search      none (web_search explains how to set one up)"),
    }
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let (overlay, overlay_error) = crate::config::Config::load_project_overlay(&workspace_root);
    if let Some(error) = &overlay_error {
        println!("overlay     ✗ {error}");
        ready = false;
    } else if overlay.is_some() {
        println!(
            "overlay     {}",
            workspace_root.join(".gertie").join("config.toml").display()
        );
    }
    let sandbox = config.resolve_sandbox_bash(overlay.as_ref());
    println!(
        "sandbox     {} ({})",
        if sandbox.value {
            "Landlock, opt-in, on"
        } else {
            "off (the permission gate still prompts for writes)"
        },
        sandbox.origin.as_str()
    );

    // The permission mode's honest reach: what the default mode can
    // and cannot enforce. gertie owns every tool, so enforcement is
    // uniform, the caveats are the mode's real edges, not per-provider
    // fallbacks; a mode is never silently downgraded.
    let mode = config.resolve_default_mode(overlay.as_ref()).value;
    let (promise, caveat) = match mode {
        gertie_core::PermissionMode::Supervised => {
            ("every write, bash, and MCP call asks; reads run free", None)
        }
        gertie_core::PermissionMode::AcceptEdits => (
            "checkpointed file edits run unasked (the ledger holds their before-state)",
            Some("bash and MCP calls still ask"),
        ),
        gertie_core::PermissionMode::Auto => {
            if sandbox.value {
                (
                    "file edits and Landlock-sandboxed bash run unasked",
                    Some("MCP calls still ask"),
                )
            } else {
                (
                    "file edits run unasked; bash still asks, the sandbox is off",
                    Some("auto without the kernel is accept-edits, and says so"),
                )
            }
        }
        gertie_core::PermissionMode::FullAccess => (
            "nothing asks, every tool runs to completion",
            Some("the destructive-command vet still fires beneath it"),
        ),
    };
    println!("mode        default {}: {promise}", mode.as_str());
    if let Some(caveat) = caveat {
        println!("            {caveat}");
    }

    if let Some(data_dir) = crate::config::Config::data_dir() {
        let store = data_dir.join("sessions.db");
        if store.exists()
            && let Ok(store) = gertie_core::SessionStore::open(&store)
        {
            let sessions = store.list_sessions().map(|list| list.len()).unwrap_or(0);
            let (input, output, cached) = store.total_usage();
            if cached > 0 {
                println!(
                    "sessions    {sessions} saved · {input} tokens in / {output} out, {cached} cached (all time)"
                );
            } else {
                println!(
                    "sessions    {sessions} saved · {input} tokens in / {output} out (all time)"
                );
            }
        } else {
            println!("sessions    none yet");
        }
        println!("data dir    {}", data_dir.display());
    } else {
        println!("sessions    (no data dir found)");
    }

    // The worktree ledger: every gertie-made worktree of this repo,
    // removed or kept and why. Report-only until --clean-worktrees,
    // a diagnostic never deletes; the flag is a deliberate choice.
    let policy = crate::worktree::CleanupPolicy {
        idle_days: config.worktree.as_ref().and_then(|w| w.cleanup_idle_days),
    };
    match crate::worktree::sweep(&workspace_root, &policy, clean_worktrees) {
        Ok(report) if report.entries.is_empty() => {
            println!(
                "worktrees   none under {}/.gertie/worktrees",
                workspace_root.display()
            );
        }
        Ok(report) => {
            println!(
                "worktrees   {} under {}/.gertie/worktrees{}",
                report.entries.len(),
                workspace_root.display(),
                if clean_worktrees {
                    format!(", {} removed", report.removed())
                } else {
                    String::new()
                }
            );
            for entry in &report.entries {
                let verdict = match &entry.verdict {
                    crate::worktree::Verdict::Removed(why) => format!("removed ({why})"),
                    crate::worktree::Verdict::Removable(why) => {
                        format!("removable, rerun with --clean-worktrees ({why})")
                    }
                    crate::worktree::Verdict::Kept(why) => format!("kept: {why}"),
                };
                println!("  {} [{}] {}", entry.path.display(), entry.branch, verdict);
            }
            if clean_worktrees {
                println!(
                    "            a session streaming in a removed worktree fails its next tool call"
                );
            }
        }
        Err(error) => {
            println!("worktrees   (not evaluated: {error})");
        }
    }

    println!();
    if ready {
        println!("✓ ready, `gertie` for the desktop app, `gertie run \"prompt\"` headlessly");
        0
    } else {
        println!("✗ not ready, fix the ✗ items above");
        1
    }
}

fn read_config(path: &Path) -> Result<crate::config::Config, String> {
    crate::config::Config::load_from(path)
}
