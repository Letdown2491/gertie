//! Bootstrap: config, session store, provider, the gpui application.

use gertie_providers::OpenAICompatible;
use gpui_kit::{App, Bounds, TitlebarOptions, WindowBounds, WindowOptions, application, open_window, prelude::*, px, size};
use std::sync::Arc;

/// The Wayland app_id / X11 WM_CLASS, matching the desktop entry.
const APP_IDENTITY: &str = "dev.gertie.gertie";

pub fn run() {
    let config = crate::config::Config::load();
    let endpoint = config.selected_endpoint();
    let endpoint_label = endpoint
        .as_ref()
        .map(|endpoint| format!("{} · {}", endpoint.name, endpoint.model))
        .unwrap_or_else(|| "no endpoint configured".to_string());

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

    // Constructing the provider spins up its tokio runtime; do it once.
    let provider = match endpoint {
        Some(endpoint) => OpenAICompatible::new(endpoint)
            .map(Arc::new)
            .map_err(|error| eprintln!("warning: endpoint setup failed: {error}"))
            .ok(),
        None => None,
    };

    let app = application().with_assets(gpui_kit::assets::Assets);
    app.run(move |cx: &mut App| {
        gpui_kit::init(cx);
        // M1 dev default; M4 replaces this with a settings-driven theme.
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        open_chat_window(store.clone(), provider.clone(), endpoint_label.clone(), cx);
        cx.activate(true);
    });
}

fn open_chat_window(
    store: Arc<gertie_core::SessionStore>,
    provider: Option<Arc<OpenAICompatible>>,
    endpoint_label: String,
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
            cx.new(|cx| crate::chat::ChatView::new(store, provider, endpoint_label, window, cx))
        },
    );
    if let Err(error) = opened {
        eprintln!("could not open the gertie window: {error}");
    }
}
