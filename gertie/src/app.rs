//! Bootstrap: config, session store, provider, the gpui application.

use gertie_core::Tool as _;
use gertie_providers::OpenAICompatible;
use gpui_kit::{
    App, Bounds, TitlebarOptions, WindowBounds, WindowOptions, application, open_window,
    prelude::*, px, size,
};
use std::path::PathBuf;
use std::sync::Arc;

/// The Wayland app_id / X11 WM_CLASS, matching the desktop entry.
const APP_IDENTITY: &str = "dev.gertie.gertie";

pub fn run() {
    // A config that fails to parse is reported in the window's status, not
    // just stderr — a GUI user would never see stderr.
    let (config, config_error) = match crate::config::Config::load() {
        Ok(config) => (config, None),
        Err(error) => (crate::config::Config::default(), Some(error)),
    };
    let endpoint = config.selected_endpoint();
    let mut endpoint_label = if let Some(error) = config_error {
        format!("config error — {error}")
    } else {
        let mut label = endpoint
            .as_ref()
            .map(|endpoint| format!("{} · {}", endpoint.name, endpoint.model))
            .unwrap_or_else(|| "no endpoint configured".to_string());
        for warning in config.key_warnings() {
            label.push_str(&format!(" — ⚠ {warning}"));
        }
        label
    };

    let store = match crate::config::Config::data_dir() {
        Some(data_dir) => {
            if let Err(error) = std::fs::create_dir_all(&data_dir) {
                eprintln!("warning: could not create {}: {error}", data_dir.display());
                None
            } else {
                gertie_core::SessionStore::open(&data_dir.join("sessions.db"))
                    .map(Arc::new)
                    .map_err(|error| eprintln!("warning: could not open session store: {error}"))
                    .ok()
            }
        }
        None => None,
    };
    let Some(store) = store else {
        eprintln!("error: no session store; refusing to run without persistence");
        std::process::exit(1);
    };

    // The workspace is wherever gertie was launched; tools resolve paths
    // against it and checkpoints land beside the session store.
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let snapshots =
        crate::config::Config::data_dir().and_then(
            |data_dir| match gertie_core::SnapshotStore::open(&data_dir.join("snapshots")) {
                Ok(store) => Some(Arc::new(store)),
                Err(error) => {
                    eprintln!("warning: checkpoint store disabled: {error}");
                    None
                }
            },
        );
    let mut tools = gertie_tools::builtin_tools(&workspace_root);

    // Skills: user dir + project dir (project overrides by name), loaded
    // on demand through load_skill; only the index rides the system
    // prompt. Discovery problems are warnings, never a failed launch.
    let skills_user = crate::config::Config::path()
        .map(|path| path.parent().unwrap().join("skills"))
        .unwrap_or_else(|| PathBuf::from(".gertie-skills"));
    let skills_project = workspace_root.join(".gertie/skills");
    let (skills, skill_warnings) = gertie_tools::skills::discover(&skills_user, &skills_project);
    if !skills.is_empty() {
        let count = skills.len();
        Arc::get_mut(&mut tools)
            .expect("registry not yet shared")
            .insert(
                "load_skill".into(),
                Arc::new(gertie_tools::skills::LoadSkill::new(&skills)),
            );
        endpoint_label.push_str(&format!(" · skills×{count}"));
    }
    for warning in &skill_warnings {
        endpoint_label.push_str(&format!(" — ⚠ {warning}"));
    }

    // The system prompt: identity, the one policy paragraph the model
    // must know, the skill index. Terse by the two-loads rule — every
    // line is context paid on every turn.
    let system_prompt = {
        let mut prompt = format!(
            "You are gertie, a coding agent working in {}.\n\
             Read-only tools run without asking; anything that changes the \
             world shows the user a card and waits. Write code, run the \
             tests, report plainly.",
            workspace_root.display()
        );
        let index = gertie_tools::skills::index_prompt(&skills);
        if !index.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&index);
        }
        Some(prompt)
    };

    // MCP servers connect at startup; a server that won't start is a
    // status-line warning, never a failed launch — gertie's local tools
    // carry the session alone if needed.
    if !config.mcp_servers.is_empty() {
        // The registry isn't shared yet, so it can be grown in place.
        let map = Arc::get_mut(&mut tools).expect("registry not yet shared");
        for server in &config.mcp_servers {
            match gertie_tools::mcp::McpClient::connect(
                &server.name,
                &server.command,
                &server.args,
                &server
                    .env
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect::<Vec<_>>(),
            ) {
                Ok(client) => match Arc::new(client).list_tools() {
                    Ok(mcp_tools) => {
                        let count = mcp_tools.len();
                        for tool in mcp_tools {
                            map.insert(tool.name().to_string(), Arc::new(tool));
                        }
                        endpoint_label.push_str(&format!(" · mcp:{}×{count}", server.name));
                    }
                    Err(error) => {
                        endpoint_label.push_str(&format!(" — ⚠ mcp {}: {error}", server.name))
                    }
                },
                Err(error) => endpoint_label.push_str(&format!(" — ⚠ {error}")),
            }
        }
    }

    // Constructing the provider spins up its tokio runtime; do it once.
    let provider = match endpoint {
        Some(endpoint) => OpenAICompatible::new(endpoint)
            .map(Arc::new)
            .map_err(|error| eprintln!("warning: endpoint setup failed: {error}"))
            .ok(),
        None => None,
    };

    let broker = Arc::new(gertie_core::PermissionBroker::new());
    let app = application().with_assets(gpui_kit::assets::Assets);
    app.run(move |cx: &mut App| {
        gpui_kit::init(cx);
        // M1 dev default; M4 replaces this with a settings-driven theme.
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        open_chat_window(
            store.clone(),
            provider.clone(),
            snapshots.clone(),
            tools.clone(),
            endpoint_label.clone(),
            system_prompt.clone(),
            broker.clone(),
            cx,
        );
        cx.activate(true);
    });
}

#[allow(clippy::too_many_arguments)]
fn open_chat_window(
    store: Arc<gertie_core::SessionStore>,
    provider: Option<Arc<OpenAICompatible>>,
    snapshots: Option<Arc<gertie_core::SnapshotStore>>,
    tools: gertie_core::ToolRegistry,
    endpoint_label: String,
    system_prompt: Option<String>,
    broker: Arc<gertie_core::PermissionBroker>,
    cx: &mut App,
) {
    let bounds = Bounds::centered(None, size(px(1080.), px(760.)), cx);
    let opened = open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(720.), px(480.))),
            app_id: Some(APP_IDENTITY.into()),
            titlebar: Some(TitlebarOptions {
                title: Some("gertie".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        cx,
        |window, cx| {
            cx.new(|cx| {
                crate::chat::ChatView::new(
                    store,
                    provider,
                    snapshots,
                    tools,
                    endpoint_label,
                    system_prompt,
                    broker,
                    window,
                    cx,
                )
            })
        },
    );
    if let Err(error) = opened {
        eprintln!("could not open the gertie window: {error}");
    }
}
