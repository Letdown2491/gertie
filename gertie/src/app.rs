//! Bootstrap: the gpui application, the theme, and the main window.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Bounds, TitlebarOptions, WindowBounds, WindowOptions, application, open_window, prelude::*, px, size};

/// The Wayland app_id / X11 WM_CLASS, matching the desktop entry.
const APP_IDENTITY: &str = "dev.gertie.gertie";

pub fn run() {
    let app = application().with_assets(gpui_kit::assets::Assets);
    app.run(|cx: &mut App| {
        gpui_kit::init(cx);
        // M0 dev default; M4 replaces this with a settings-driven theme.
        Theme::change(ThemeMode::Dark, None, cx);
        open_chat_window(cx);
        cx.activate(true);
    });
}

fn open_chat_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(980.), px(720.)), cx);
    let opened = open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(540.), px(420.))),
            app_id: Some(APP_IDENTITY.into()),
            titlebar: Some(TitlebarOptions {
                title: Some("gertie".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        cx,
        |window, cx| cx.new(|cx| crate::chat::ChatView::new(window, cx)),
    );
    if let Err(error) = opened {
        eprintln!("could not open the gertie window: {error}");
    }
}
