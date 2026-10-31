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
    /// The checkpoint ledger behind the view, when the test wired one.
    snapshots: Option<Arc<gertie_core::SnapshotStore>>,
    /// TempDirs that must outlive the window (snapshot objects, the
    /// workspace the view resolves paths against).
    hold: Vec<tempfile::TempDir>,
}

fn fixture() -> Fixture {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    fixture_with_store(store)
}

fn fixture_with_store(store: Arc<SessionStore>) -> Fixture {
    fixture_built(store, None, None, None)
}

/// A fixture whose view carries a real checkpoint ledger over a real
/// (empty) workspace — the turn-diff path end to end.
fn fixture_with_snapshots() -> Fixture {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let snapshots_dir = tempfile::tempdir().expect("snapshots dir");
    let snapshots =
        Arc::new(gertie_core::SnapshotStore::open(snapshots_dir.path()).expect("snapshot store"));
    let workspace = tempfile::tempdir().expect("workspace dir");
    let mut fixture = fixture_built(
        store,
        Some(snapshots),
        Some(workspace.path().to_path_buf()),
        None,
    );
    fixture.hold = vec![snapshots_dir, workspace];
    fixture
}

fn fixture_built(
    store: Arc<SessionStore>,
    snapshots: Option<Arc<gertie_core::SnapshotStore>>,
    workspace: Option<std::path::PathBuf>,
    settle_command: Option<String>,
) -> Fixture {
    let mut cx = HeadlessAppContext::with_asset_source(
        Arc::new(NoopTextSystem),
        Arc::new(gpui_kit::assets::Assets),
    );
    cx.update(|cx| {
        gpui_kit::init(cx);
        gertie::palette::bind_palette_keys(cx);
        gertie::chat::bind_chat_keys(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
    });
    let mut view_entity = None;
    let window = cx
        .open_window(size(px(900.), px(640.)), |window, cx| {
            let view = cx.new(|cx| {
                let panel = cx.new(|_| {
                    gertie::settings_edit::SettingsPanelView::new(
                        &gertie::config::Config::default(),
                        gertie::settings::SettingsSummary::from_config(
                            &gertie::config::Config::default(),
                        ),
                    )
                });
                ChatView::new(
                    store.clone(),
                    None,
                    snapshots.clone(),
                    gertie_tools::builtin_tools(Path::new(".")),
                    "no endpoint configured".into(),
                    None,
                    Vec::new(),
                    workspace.unwrap_or_else(|| Path::new(".").to_path_buf()),
                    settle_command,
                    Arc::new(gertie_core::PermissionBroker::new()),
                    gertie_core::SessionScope::new(),
                    panel,
                    true,
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
        snapshots,
        hold: Vec::new(),
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

/// A resumed transcript full of markdown and a finished tool row must
/// draw without panicking — the markdown renderer is hand-rolled and
/// byte-index-driven, so the headless draw is the guard against
/// char-boundary panics reaching a user.
#[test]
fn markdown_and_tool_rows_draw_clean() {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let session_id = store
        .create_session("markdown transcript")
        .expect("session");
    let db = store
        .append_message(session_id, None, gertie_core::Role::User, "hello")
        .expect("user row");
    for text in [
        "# Heading\n\nprose with **bold**, *em*, `code`, and a [link](https://x.y)\n",
        "- item one\n- [x] done\n- [ ] open\n  1. nested\n",
        "| a | b |\n|---|---|\n| 1 | 2 |\n",
        "> quoted **prose**\n\n---\n\n```rust\nfn main() {}\n```",
        "closed **span and *nested* inside** end — écrit en français — 🦀 emoji",
    ] {
        store
            .append_message(session_id, Some(db), gertie_core::Role::Assistant, text)
            .expect("assistant row");
    }
    store
        .append_tool_result(
            session_id,
            Some(db),
            "call-1",
            "read_file",
            r#"{"path": "src/main.rs"}"#,
            "   1  fn main() {}\n   2  // three lines",
        )
        .expect("tool row");

    // Open the chat on the stored session and force full draws: every
    // row kind above goes through layout at least once.
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
                let panel = cx.new(|_| {
                    gertie::settings_edit::SettingsPanelView::new(
                        &gertie::config::Config::default(),
                        gertie::settings::SettingsSummary::from_config(
                            &gertie::config::Config::default(),
                        ),
                    )
                });
                ChatView::new(
                    store.clone(),
                    None,
                    None,
                    gertie_tools::builtin_tools(Path::new(".")),
                    "no endpoint configured".into(),
                    None,
                    Vec::new(),
                    Path::new(".").to_path_buf(),
                    None,
                    Arc::new(gertie_core::PermissionBroker::new()),
                    gertie_core::SessionScope::new(),
                    panel,
                    true,
                    window,
                    cx,
                )
            });
            view_entity = Some(view.downgrade());
            cx.new(|cx| Root::new(view, window, cx))
        })
        .expect("chat window");
    settle(&mut cx, window.into());

    // The transcript actually loaded the persisted rows: 5 markdown
    // messages + the user row + the tool card.
    let turns = cx.update(|cx| {
        view_entity
            .expect("view entity")
            .upgrade()
            .expect("view alive")
            .read(cx)
            .turn_count_for_test()
    });
    assert!(turns >= 7, "expected the stored rows to load, got {turns}");
}

/// Seeds a session whose needle word appears once, deep in a long
/// transcript, plus a newer session (so that's the one a fresh window
/// resumes) without the needle.
fn seeded_store() -> Arc<SessionStore> {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let needle = store
        .create_session("the flaky widget hunt")
        .expect("needle");
    for i in 0..40 {
        // The one match sits at turn 6 — early enough that revealing
        // it must move the scroll off the bottom anchor.
        let content = if i == 3 {
            "the flaky widget broke here"
        } else {
            &format!("note {i} about styling")
        };
        let parent = store
            .append_message(needle, None, gertie_core::Role::User, content)
            .expect("row");
        store
            .append_message(needle, Some(parent), gertie_core::Role::Assistant, "nod")
            .expect("row");
    }
    // The newer session wins the resume.
    let chatter = store.create_session("chatter").expect("chatter");
    store
        .append_message(chatter, None, gertie_core::Role::User, "hello again")
        .expect("chatter row");
    store
}

#[test]
fn the_palette_jumps_to_a_hit_in_another_session() {
    let store = seeded_store();
    let mut fixture = fixture_with_store(store);

    // The window resumed the newest session, not the needle's.
    let resumed = fixture
        .cx
        .update(|cx| {
            fixture
                .view
                .upgrade()
                .expect("view alive")
                .read(cx)
                .session_id()
        })
        .expect("resumed session");
    assert_ne!(resumed.0, 1, "the needle session must not be resumed");

    // Ctrl+K opens the palette…
    fixture.act(|window, cx| {
        window.press("ctrl-k", cx);
    });
    assert!(
        fixture.cx.update(|cx| fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .palette_open_for_test()),
        "ctrl-k should open the palette"
    );

    // …typing fills it with the store's answer…
    fixture.act(|window, cx| {
        window.input("widget", cx);
    });
    assert_eq!(
        fixture.cx.update(|cx| fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .palette_hits_for_test()),
        1,
        "the needle word should rank exactly one hit"
    );

    // …and Enter lands on that session with the turn in view.
    fixture.act(|window, cx| {
        window.press("enter", cx);
    });
    let (session, open, scroll) = fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive").read(cx);
        (
            view.session_id(),
            view.palette_open_for_test(),
            view.scroll_top_for_test(),
        )
    });
    assert_eq!(
        session.expect("jumped session").0,
        1,
        "confirm switched sessions"
    );
    assert!(!open, "confirm closes the palette");
    // The transcript left the bottom anchor (40 items) to reveal turn
    // 6 — the whole point of the jump.
    assert!(
        scroll < 40,
        "the hit's turn should be scrolled into view, got {scroll}"
    );
}

#[test]
fn escape_leaves_the_palette_without_jumping() {
    let store = seeded_store();
    let mut fixture = fixture_with_store(store);

    fixture.act(|window, cx| {
        window.press("ctrl-k", cx);
        window.input("widget", cx);
    });
    fixture.act(|window, cx| {
        window.press("escape", cx);
        window.press("escape", cx);
    });
    let (session, open) = fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive").read(cx);
        (view.session_id(), view.palette_open_for_test())
    });
    assert_eq!(session.expect("still the resumed session").0, 2);
    assert!(!open, "two escapes: one clears the query, one closes");
}

#[test]
fn a_sent_message_mints_a_turn_window_and_the_panel_opens() {
    let mut fixture = fixture_with_snapshots();

    // No provider configured: the turn still mints — the ledger
    // anchors to the user row before the loop ever runs.
    fixture.act(|window, cx| {
        window.input("do a thing", cx);
        window.press("enter", cx);
    });
    let session = fixture
        .cx
        .update(|cx| {
            fixture
                .view
                .upgrade()
                .expect("view alive")
                .read(cx)
                .session_id()
        })
        .expect("session born");
    let user = fixture
        .store
        .load_messages(session)
        .expect("rows")
        .remove(0);

    // The ledger answers for that message — the seam a tool card's
    // "turn diff" resolves through, live or after a reload.
    let snapshots = fixture.snapshots.as_ref().expect("wired ledger");
    let turn = snapshots
        .turn_for_message(session, user.id)
        .expect("query")
        .expect("turn minted by send");
    assert_eq!(turn.user_message_id, user.id);

    // The panel opens on it — empty, since nothing mutated yet —
    // showing the no-changes answer rather than an error.
    let view = fixture.view.clone();
    fixture.act(|_, cx| {
        view.upgrade()
            .expect("view alive")
            .update(cx, |view, cx| view.open_turn_diff_for_message(user.id, cx));
    });
    assert_eq!(
        fixture.cx.update(|cx| fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .turn_diff_for_test()),
        Some(0),
        "panel open with zero files listed"
    );
}

/// Seeds a session deeper than the transcript's load window: 810 rows,
/// so paging lands 400 at a time and the last page is short.
fn archive_store() -> Arc<SessionStore> {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let session = store.create_session("the archive").expect("session");
    let mut parent = None;
    for i in 0..810 {
        parent = Some(
            store
                .append_message(
                    session,
                    parent,
                    gertie_core::Role::User,
                    &format!("row {i}"),
                )
                .expect("row"),
        );
    }
    store
}

/// An archive deeper than the load window pages in: the newest 400 rows
/// render, "load earlier" brings one batch at a time, and the last
/// click retires the pagination row. The store keeps every row
/// throughout — paging is a view concern, the record is not.
#[test]
fn a_long_transcript_loads_in_pages_and_load_earlier_exhausts_cleanly() {
    let store = archive_store();
    let mut fixture = fixture_with_store(store);

    let turns = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .turn_count_for_test()
    });
    assert_eq!(turns, 401, "400 newest rows plus the load-earlier row");

    // The pagination row lives at the top; bring it on screen (the
    // programmatic stand-in for the user scrolling up) and click it
    // for real.
    for expected in [801, 810] {
        fixture.cx.update(|cx| {
            fixture
                .view
                .upgrade()
                .expect("view alive")
                .update(cx, |view, _| view.scroll_to_item_for_test(0));
        });
        fixture.act(|window, cx| {
            window.click("load-earlier", cx);
        });
        let turns = fixture.cx.update(|cx| {
            fixture
                .view
                .upgrade()
                .expect("view alive")
                .read(cx)
                .turn_count_for_test()
        });
        assert_eq!(turns, expected, "one page of 400 per click");
        let scroll = fixture.cx.update(|cx| {
            fixture
                .view
                .upgrade()
                .expect("view alive")
                .read(cx)
                .scroll_top_for_test()
        });
        assert_eq!(scroll, 0, "the reader stays on the oldest content");
    }

    // Exhausted: the pagination row retired — no stale control, and
    // nothing was deleted from the record.
    fixture.act(|window, _| {
        assert!(
            window.try_find("load-earlier").is_none(),
            "the pagination row must retire when the archive is exhausted"
        );
    });
    assert_eq!(fixture.store.list_sessions().unwrap().len(), 1);
    assert_eq!(
        fixture
            .store
            .load_messages(gertie_core::SessionId(1))
            .unwrap()
            .len(),
        810
    );
}

impl Fixture {
    /// The view's settled session ids, for the lifecycle tests.
    fn view_settled(&mut self) -> Vec<gertie_core::SessionId> {
        let view = self.view.clone();
        self.cx.update(|cx| {
            view.upgrade()
                .expect("view alive")
                .read(cx)
                .settled_ids_for_test()
        })
    }
}

/// Two stored sessions; the fixture resumes the newest (id 2).
fn two_session_store() -> Arc<SessionStore> {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let first = store.create_session("the older chore").expect("session");
    store
        .append_message(first, None, gertie_core::Role::User, "old work")
        .expect("row");
    let second = store.create_session("the newer chat").expect("session");
    store
        .append_message(second, None, gertie_core::Role::User, "new work")
        .expect("row");
    store
}

/// Settle is a lifecycle, not a filter: ✓ folds a session out of the
/// active list, the fold expands, ↩ (or just opening the session)
/// brings it back. The record is untouched throughout.
#[test]
fn settling_folds_a_session_and_opening_brings_it_back() {
    let store = two_session_store();
    let mut fixture = fixture_with_store(store.clone());

    // Settle the older session by its row's ✓.
    fixture.act(|window, cx| {
        window.click("settle-1", cx);
    });
    assert_eq!(fixture.view_settled(), [gertie_core::SessionId(1)]);
    assert!(
        store
            .list_sessions()
            .unwrap()
            .iter()
            .find(|summary| summary.id == gertie_core::SessionId(1))
            .expect("session survives settling")
            .settled,
        "settled state persists in the store, not just the view"
    );

    // The fold is collapsed: the settled row paints nothing — its
    // restore click must miss until the fold expands.
    fixture.act(|window, cx| {
        assert!(window.try_find("session-1").is_none());
        window.click("settled-fold", cx);
    });

    // Open the settled session from the expanded fold — opening it
    // is activity: it comes back to the active list on its own.
    fixture.act(|window, cx| {
        window.click("session-1", cx);
    });
    assert!(
        fixture.view_settled().is_empty(),
        "opening a settled session un-settles it"
    );
}

/// Settling by hand folds; the fold's own ↩ restores without opening.
#[test]
fn the_folds_restore_button_unsettles_without_opening() {
    let store = two_session_store();
    let mut fixture = fixture_with_store(store);
    let view = fixture.view.clone();

    fixture.act(|window, cx| {
        window.click("settle-1", cx);
    });
    fixture.act(|window, cx| {
        window.click("settled-fold", cx);
    });
    fixture.act(|window, cx| {
        window.click("settle-1", cx);
    });
    assert!(fixture.view_settled().is_empty());
    // Restore never opened the session: the transcript kept its chat.
    let active = fixture
        .cx
        .update(|cx| view.upgrade().expect("view alive").read(cx).tab_title());
    assert_eq!(active, "the newer chat");
}

/// The settle command (config: settle_command) runs in the settling
/// session's recorded workspace — the reclaim hook end to end: send
/// records the root, ✓ runs the command off the UI thread, the
/// transcript reports the outcome.
#[test]
fn settling_runs_the_reclaim_command_in_the_recorded_workspace() {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let workspace = tempfile::tempdir().expect("workspace dir");
    let marker = workspace.path().join("reclaimed.marker");
    let command = format!("touch '{}'", marker.display());
    let mut fixture = fixture_built(
        store,
        None,
        Some(workspace.path().to_path_buf()),
        Some(command),
    );

    // A send records the workspace on the session — the settle hook's
    // anchor. (No provider: the turn fails loudly and ends; the
    // session and its root are real either way.)
    fixture.act(|window, cx| {
        window.input("hello", cx);
        window.press("enter", cx);
    });
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
        .expect("session created by send");

    // Settle it: the command touches a marker inside the recorded root.
    fixture.act(|window, cx| {
        window.click(format!("settle-{}", session_id.0), cx);
    });
    assert!(
        marker.exists(),
        "the settle command ran in the recorded workspace"
    );
}
