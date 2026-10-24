//! The desktop front: gpui application + window over the shared bootstrap.

use gpui_kit::{
    App, Bounds, TitlebarOptions, WindowBounds, WindowOptions, application, open_window,
    prelude::*, px, size,
};
use std::sync::Arc;

/// The Wayland app_id / X11 WM_CLASS, matching the desktop entry.
const APP_IDENTITY: &str = "dev.gertie.gertie";

pub fn run() {
    run_with(None);
}

/// `run`, with the `--config` override from the CLI.
pub fn run_with(config_path: Option<std::path::PathBuf>) {
    // The bootstrap carries config errors in its label — a GUI user
    // never sees stderr, so the window reports them.
    let boot = Arc::new(crate::bootstrap::Bootstrap::load(config_path.as_deref()));

    let app = application().with_assets(gpui_kit::assets::Assets);
    app.run(move |cx: &mut App| {
        gpui_kit::init(cx);
        // M1 dev default; M4 replaces this with a settings-driven theme.
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        open_chat_window(boot.clone(), cx);
        cx.activate(true);
    });
}

fn open_chat_window(boot: Arc<crate::bootstrap::Bootstrap>, cx: &mut App) {
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
        |window, cx| cx.new(|cx| crate::tabs::WorkspaceView::new(boot, window, cx)),
    );
    if let Err(error) = opened {
        eprintln!("could not open the gertie window: {error}");
    }
}
