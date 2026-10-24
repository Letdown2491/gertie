//! Headless UI regression tests for the chat surface: typing and Enter
//! must reach the session store, with no provider configured at all —
//! the exact path a fresh install walks first.

use gertie::chat::ChatView;
use gertie_core::SessionStore;
use gpui_kit::component::Root;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext, HeadlessAppContext, NoopTextSystem, WeakEntity, WindowHandle, px,
    size,
};
use std::path::Path;
use std::sync::Arc;

/// Runs the executor until quiet, then forces a few full draws so pending
/// layout work has nowhere left to hide.
fn settle(cx: &mut HeadlessAppContext, window: AnyWindowHandle) {
    for _ in 0..3 {
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw fixture");
    }
}

struct Fixture {
    cx: HeadlessAppContext,
    window: WindowHandle<Root>,
    /// Weak on purpose: a strong handle held across `cx`'s drop trips
    /// gpui's leak detector.
    view: WeakEntity<ChatView>,
    store: Arc<SessionStore>,
}

fn fixture() -> Fixture {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let mut cx = HeadlessAppContext::with_asset_source(
        Arc::new(NoopTextSystem),
        Arc::new(gpui_kit::assets::Assets),
    );
    cx.update(|cx| {
        gpui_kit::init(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
    });
    let mut view_entity = None;
    let window = cx
        .open_window(size(px(900.), px(640.)), |window, cx| {
            let view = cx.new(|cx| {
                ChatView::new(
                    store.clone(),
                    None,
                    None,
                    gertie_tools::builtin_tools(Path::new(".")),
                    "no endpoint configured".into(),
                    None,
                    window,
                    cx,
                )
            });
            view_entity = Some(view.downgrade());
            cx.new(|cx| Root::new(view, window, cx))
        })
        .expect("chat window");
    settle(&mut cx, window.into());
    Fixture {
        cx,
        window,
        view: view_entity.expect("view entity"),
        store,
    }
}

impl Fixture {
    fn act(&mut self, f: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App)) {
        let window = self.window;
        self.cx
            .update_window(window.into(), |_, window, cx| f(window, cx))
            .expect("fixture window");
        settle(&mut self.cx, window.into());
    }
}

#[test]
fn typing_and_enter_persists_the_user_message() {
    let mut fixture = fixture();

    // A fresh install: no session rows yet — they are created lazily.
    assert!(fixture.store.list_sessions().unwrap().is_empty());

    fixture.act(|window, cx| {
        window.input("hello gertie", cx);
        window.press("enter", cx);
    });

    // send() ran end to end: the session row exists, titled by the message,
    // and the user turn is persisted.
    let session_id = fixture
        .cx
        .update(|cx| {
            fixture
                .view
                .upgrade()
                .expect("view alive")
                .read(cx)
                .session_id()
        })
        .expect("session created lazily");
    let messages = fixture.store.load_messages(session_id).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "hello gertie");
    assert_eq!(messages[0].role, gertie_core::Role::User);
    assert_eq!(fixture.store.list_sessions().unwrap().len(), 1);
}

#[test]
fn the_sidebar_shows_only_real_conversations() {
    let mut fixture = fixture();

    // No sends yet: no session rows, so no phantom "new chat" entries.
    assert!(fixture.store.list_sessions().unwrap().is_empty());
    assert!(
        fixture
            .cx
            .update(|cx| {
                fixture
                    .view
                    .upgrade()
                    .expect("view alive")
                    .read(cx)
                    .session_id()
            })
            .is_none()
    );
}

#[test]
fn enter_on_an_empty_input_does_nothing() {
    let mut fixture = fixture();
    fixture.act(|window, cx| {
        window.press("enter", cx);
    });
    assert!(fixture.store.list_sessions().unwrap().is_empty());
}
