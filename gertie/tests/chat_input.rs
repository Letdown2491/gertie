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
    fixture_built(store, None, None, None, None, None, Vec::new(), None)
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
        None,
        None,
        Vec::new(),
        None,
    );
    fixture.hold = vec![snapshots_dir, workspace];
    fixture
}

#[allow(clippy::too_many_arguments)]
fn fixture_built(
    store: Arc<SessionStore>,
    snapshots: Option<Arc<gertie_core::SnapshotStore>>,
    workspace: Option<std::path::PathBuf>,
    settle_command: Option<String>,
    // A scripted provider turns the fixture into a live turn: the pump,
    // permission cards, and the store writes all run for real.
    provider: Option<Arc<gertie_providers::AnyProvider>>,
    // Share the view's broker: the test registers asks the way a
    // worker's loop does and holds the oneshot the answer resolves.
    broker: Option<Arc<gertie_core::PermissionBroker>>,
    // Extra tools beside the builtins (None = builtins alone).
    extra_tools: Vec<Arc<dyn gertie_core::Tool>>,
    // The rewind safety guard (None = single-tab tests, nothing ever
    // streams elsewhere).
    guard: Option<gertie::chat::LiveElsewhere>,
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
    let guard = guard.unwrap_or_else(|| Arc::new(|_, _, _| Ok(())));
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
                    provider,
                    snapshots.clone(),
                    merge_tools(extra_tools, workspace.as_deref().unwrap_or(Path::new("."))),
                    "no endpoint configured".into(),
                    None,
                    Vec::new(),
                    workspace.unwrap_or_else(|| Path::new(".").to_path_buf()),
                    settle_command,
                    broker.unwrap_or_else(|| Arc::new(gertie_core::PermissionBroker::new())),
                    gertie_core::SessionScope::new(),
                    gertie_core::ModeScope::new(),
                    false,
                    gertie_core::PermissionMode::Supervised,
                    panel,
                    true,
                    guard.clone(),
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

/// Builtins plus whatever test doubles the fixture asked for. The
/// registry is a plain map, so merging is one insert per extra tool.
/// Rooted at the fixture's workspace, the way a real session's is —
/// a tool that writes relative paths writes there, not at the CWD.
fn merge_tools(extra: Vec<Arc<dyn gertie_core::Tool>>, root: &Path) -> gertie_core::ToolRegistry {
    let mut map = (*gertie_tools::builtin_tools(root)).clone();
    for tool in extra {
        map.insert(tool.name().to_string(), tool);
    }
    Arc::new(map)
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
fn invisible_characters_are_stripped_when_a_message_is_sent() {
    let mut fixture = fixture();

    // Bidi and zero-width characters ride in with whatever they were
    // pasted from; entering the message strips them at the system's
    // entrance.
    fixture.act(|window, cx| {
        window.input("pay\u{200B}to\u{202E}x\u{200F}", cx);
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
        .expect("session created lazily");
    // The record and the session title hold the cleaned words only.
    let messages = fixture.store.load_messages(session_id).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "paytox");
    assert_eq!(fixture.store.list_sessions().unwrap()[0].title, "paytox");
    // The user hears that cleaning happened.
    let notices = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .notice_texts_for_test()
    });
    assert!(
        notices
            .iter()
            .any(|text| text.contains("Invisible characters"))
    );
}

#[test]
fn the_composer_chip_cycles_the_permission_mode_and_persists_it() {
    let mut fixture = fixture();

    // A session is born with the chip's posture — supervised until the
    // chip moves.
    fixture.act(|window, cx| {
        window.input("go", cx);
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
        .expect("session created lazily");
    let store_mode = |fixture: &Fixture| {
        fixture
            .store
            .list_sessions()
            .unwrap()
            .iter()
            .find(|summary| summary.id == session_id)
            .expect("summary")
            .mode
    };
    assert_eq!(
        store_mode(&fixture),
        gertie_core::PermissionMode::Supervised
    );

    // One click per ring position; the store records each stop, and
    // the ring wraps back to supervised.
    for expected in [
        gertie_core::PermissionMode::AcceptEdits,
        gertie_core::PermissionMode::Auto,
        gertie_core::PermissionMode::FullAccess,
        gertie_core::PermissionMode::Supervised,
    ] {
        fixture.act(|window, cx| {
            window.click("mode-chip", cx);
        });
        assert_eq!(store_mode(&fixture), expected, "chip click persists");
    }
}

#[test]
fn accept_edits_runs_the_checkpointed_edit_without_asking() {
    // A scripted turn that writes a file: the write_file call carries
    // a path, so the checkpoint hook covers it — exactly the class
    // accept_edits runs unasked. Empty step texts keep replay pacing
    // deterministic.
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("edits.toml"),
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"write_file\"\n\
         arguments = '{\"path\": \"scratch.txt\", \"content\": \"auto\"}'\n\n\
         [[steps]]\ntext = ''\n",
    )
    .expect("scenario file");
    let workspace = tempfile::tempdir().expect("workspace dir");
    let snapshots_dir = tempfile::tempdir().expect("snapshots dir");
    let snapshots =
        Arc::new(gertie_core::SnapshotStore::open(snapshots_dir.path()).expect("snapshot store"));
    let endpoint = gertie_providers::Endpoint {
        name: "replay".into(),
        base_url: scenarios.path().to_string_lossy().into_owned(),
        model: "edits".into(),
        api_key: None,
        context_window: None,
        wire: gertie_providers::Wire::Replay,
    };
    let provider = Arc::new(
        gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).expect("replay provider"),
    );
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let workspace_path = workspace.path().to_path_buf();
    let snapshots_path = snapshots_dir.path().to_path_buf();
    let mut fixture = fixture_built(
        store,
        Some(snapshots),
        Some(workspace.path().to_path_buf()),
        None,
        Some(provider),
        None,
        Vec::new(),
        None,
    );
    fixture.hold = vec![scenarios, workspace, snapshots_dir];

    // Flip to accept edits before the first send — the session is
    // born with the chip's posture.
    fixture.act(|window, cx| {
        window.click("mode-chip", cx);
    });
    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 30);

    // The write ran with no permission card in between: the file is
    // on disk with the scripted content, and the ledger holds its
    // prior state like any approved edit.
    let written = std::fs::read(workspace_path.join("scratch.txt")).expect("write ran unasked");
    assert_eq!(written, b"auto");
    assert_eq!(
        std::fs::read_dir(snapshots_path.join("objects"))
            .expect("ledger")
            .count(),
        1,
        "the auto-approved edit is checkpointed"
    );
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
    let mut parent = Some(
        store
            .append_message(session_id, None, gertie_core::Role::User, "hello")
            .expect("user row"),
    );
    for text in [
        "# Heading\n\nprose with **bold**, *em*, `code`, and a [link](https://x.y)\n",
        "- item one\n- [x] done\n- [ ] open\n  1. nested\n",
        "| a | b |\n|---|---|\n| 1 | 2 |\n",
        "> quoted **prose**\n\n---\n\n```rust\nfn main() {}\n```",
        "closed **span and *nested* inside** end — écrit en français — 🦀 emoji",
    ] {
        parent = Some(
            store
                .append_message(session_id, parent, gertie_core::Role::Assistant, text)
                .expect("assistant row"),
        );
    }
    store
        .append_tool_result(
            session_id,
            parent,
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
    let guard: gertie::chat::LiveElsewhere = Arc::new(|_, _, _| Ok(()));
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
                    gertie_core::ModeScope::new(),
                    false,
                    gertie_core::PermissionMode::Supervised,
                    panel,
                    true,
                    guard.clone(),
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
        None,
        None,
        Vec::new(),
        None,
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

/// The §2.5 pin: a worker's permission ask bubbles into the parent
/// transcript as a pending card — not dead-ending in the agents panel —
/// and the human's answer rides back to the worker's loop parked on it.
/// Scripted end to end: the replay provider drives the leader; its one
/// tool registers a worker ask on the shared broker exactly as a real
/// worker's loop does, then parks on the answer.
struct WorkerAskTool {
    broker: Arc<gertie_core::PermissionBroker>,
}

impl gertie_core::Tool for WorkerAskTool {
    fn name(&self) -> &'static str {
        "worker_ask"
    }

    fn description(&self) -> String {
        "test double: registers a worker permission ask and parks on the answer".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    fn run<'a>(
        &'a self,
        _arguments: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = gertie_core::ToolResult> + Send + 'a>>
    {
        Box::pin(async move {
            let (_id, answer) = self.broker.request(
                gertie_core::ToolCall {
                    id: "worker_1".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"rm -rf /tmp/worker-scratch"}"#.into(),
                },
                gertie_core::PromptSource::Worker,
            );
            match answer.await {
                Ok(gertie_core::PermissionDecision::Approve) => Ok("worker approved".into()),
                Ok(gertie_core::PermissionDecision::Deny { .. }) => Ok("worker denied".into()),
                Err(_) => Ok("worker ask dropped".into()),
            }
        })
    }
}

/// Settles a live turn: the pump's select loop ticks on a background
/// timer, and test-executor timers only fire when the clock moves —
/// so nudge time, drain, and draw, until the turn parks for good.
fn pump(fixture: &mut Fixture, passes: usize) {
    for _ in 0..passes {
        fixture
            .cx
            .background_executor
            .advance_clock(std::time::Duration::from_millis(5));
        settle(&mut fixture.cx, fixture.window.into());
    }
}

#[test]
fn a_worker_ask_bubbles_into_the_transcript_and_the_answer_reaches_the_worker() {
    // The scripted leader turn: one call to the test's worker-ask tool,
    // then a closing step with no calls — the turn ends there. Empty
    // texts keep the replay provider from pacing any deltas.
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("worker.toml"),
        "[[steps]]\ntext = ''\n\
         [[steps.tool_calls]]\nname = 'worker_ask'\narguments = '{}'\n\
         [[steps]]\ntext = ''\n",
    )
    .expect("scenario file");
    let endpoint = gertie_providers::Endpoint {
        name: "replay".into(),
        base_url: scenarios.path().to_string_lossy().into_owned(),
        model: "worker".into(),
        api_key: None,
        context_window: None,
        wire: gertie_providers::Wire::Replay,
    };
    let provider = Arc::new(
        gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).expect("replay provider"),
    );

    let broker = Arc::new(gertie_core::PermissionBroker::new());
    // Read-only on purpose: the leader's own gate auto-runs the tool,
    // so the worker's ask is the only pending card in the transcript.
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let mut fixture = fixture_built(
        store.clone(),
        None,
        None,
        None,
        Some(provider),
        Some(broker.clone()),
        vec![Arc::new(WorkerAskTool {
            broker: broker.clone(),
        })],
        None,
    );

    // Send: the scripted leader calls worker_ask (a Running card, its
    // auto-run), which registers the ask and parks. The pump surfaces
    // it as a pending card in THIS transcript — the parent's, not the
    // agents panel's.
    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 30);
    fixture.act(|window, cx| {
        assert!(
            window.try_find("approve-2").is_some(),
            "the worker ask bubbled into the parent transcript as a pending card"
        );
    });

    // The answer rides back: approve resolves the parked tool, the turn
    // completes, and what the worker's loop made of the answer is
    // persisted with the turn.
    fixture.act(|window, cx| {
        window.click("approve-2", cx);
    });
    pump(&mut fixture, 30);
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
    let messages = store
        .load_recent_messages(session_id, 50)
        .expect("persisted rows");
    assert!(
        messages
            .iter()
            .any(|message| message.content == "worker approved"),
        "the human's answer reached the parked worker loop: {:?}",
        messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
    );
}

/// The §2.4 pin: ✕ deletes without a "sure?" step — the row leaves
/// every list at once and the undo toast is the speed bump. The store
/// row survives the five-second window; when it closes, the purge task
/// removes the row for real.
#[test]
fn deleting_hides_the_session_then_the_undo_window_purges_it() {
    let store = two_session_store();
    let mut fixture = fixture_with_store(store.clone());

    fixture.act(|window, cx| {
        window.click("delete-1", cx);
    });
    fixture.act(|window, cx| {
        assert!(
            window.try_find("session-1").is_none(),
            "the row leaves the list the moment ✕ is clicked"
        );
        assert!(
            window.try_find("undo-delete").is_some(),
            "the undo toast is up for the window's duration"
        );
    });
    assert_eq!(
        store.list_sessions().unwrap().len(),
        2,
        "the store row survives while the window is open"
    );

    // The window closes: the purge task deletes the row and the toast
    // goes with it.
    fixture
        .cx
        .background_executor
        .advance_clock(std::time::Duration::from_secs(6));
    settle(&mut fixture.cx, fixture.window.into());
    assert_eq!(store.list_sessions().unwrap().len(), 1);
    fixture.act(|window, cx| {
        assert!(
            window.try_find("undo-delete").is_none(),
            "a spent window has nothing left to undo"
        );
    });
}

/// Undo (the toast's button, or Ctrl+Z outside the composer) pops the
/// pending delete: the row returns untouched, and the purge task that
/// fires later finds nothing to do.
#[test]
fn undo_brings_a_deleted_session_back_before_the_window_closes() {
    let store = two_session_store();
    let mut fixture = fixture_with_store(store.clone());

    fixture.act(|window, cx| {
        window.click("delete-1", cx);
    });
    fixture.act(|window, cx| {
        window.click("undo-delete", cx);
    });
    fixture.act(|window, cx| {
        assert!(
            window.try_find("session-1").is_some(),
            "the undone row returns to its list"
        );
        assert!(
            window.try_find("undo-delete").is_none(),
            "the toast is spent"
        );
    });

    // The window's task still fires — and finds nothing pending.
    fixture
        .cx
        .background_executor
        .advance_clock(std::time::Duration::from_secs(6));
    settle(&mut fixture.cx, fixture.window.into());
    assert_eq!(
        store.list_sessions().unwrap().len(),
        2,
        "undo beat the purge: both sessions survive"
    );
}

/// Ctrl+Z outside the composer fires the undo-delete action — the
/// binding is global (no context), so it works from wherever focus
/// sits that doesn't consume the keystroke.
#[test]
fn ctrl_z_outside_the_composer_undoes_the_delete() {
    let store = two_session_store();
    let mut fixture = fixture_with_store(store);
    fixture.act(|window, cx| {
        window.click("delete-1", cx);
    });
    fixture.act(|window, cx| {
        window.press("ctrl-z", cx);
    });
    fixture.act(|window, cx| {
        assert!(
            window.try_find("session-1").is_some(),
            "ctrl-z undid the delete via the global binding"
        );
    });
}

/// The §2.3 pin, end to end through the replay provider: a turn that
/// ends on the provider's rate limit leaves the session Limited —
/// persisted, resumed from the bar above the composer, cleared the
/// moment the resumed turn runs.
#[test]
fn a_rate_limited_turn_offers_resume_and_resuming_clears_it() {
    // Step one refuses (the limit); step two is what the resumed turn
    // gets. Empty texts keep the replay pacing out of the picture.
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("worker.toml"),
        "[[steps]]\nlimited = true\n\n[[steps]]\ntext = ''\n",
    )
    .expect("scenario file");
    let endpoint = gertie_providers::Endpoint {
        name: "replay".into(),
        base_url: scenarios.path().to_string_lossy().into_owned(),
        model: "worker".into(),
        api_key: None,
        context_window: None,
        wire: gertie_providers::Wire::Replay,
    };
    let provider = Arc::new(
        gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).expect("replay provider"),
    );
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let mut fixture = fixture_built(
        store.clone(),
        None,
        None,
        None,
        Some(provider),
        None,
        Vec::new(),
        None,
    );

    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 30);
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
    assert!(
        fixture.cx.update(|cx| fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .open_session_limited()),
        "the turn ended limited"
    );
    assert!(
        store
            .list_sessions()
            .unwrap()
            .iter()
            .find(|summary| summary.id == session_id)
            .expect("summary")
            .limited,
        "limited persists in the store — it survives a restart"
    );
    fixture.act(|window, cx| {
        assert!(
            window.try_find("resume-turn").is_some(),
            "the resume bar is up on a limited session"
        );
    });

    // Resume: the scripted step two answers; the flag clears and the
    // bar goes with it.
    fixture.act(|window, cx| {
        window.click("resume-turn", cx);
    });
    pump(&mut fixture, 30);
    assert!(
        !store
            .list_sessions()
            .unwrap()
            .iter()
            .find(|summary| summary.id == session_id)
            .expect("summary")
            .limited,
        "a resumed turn clears the limited state"
    );
    fixture.act(|window, cx| {
        assert!(
            window.try_find("resume-turn").is_none(),
            "the bar drops once the session is no longer limited"
        );
    });
}

/// The snooze pin: "Snooze 30 min" schedules the resume itself — half
/// an hour later the limited turn resumes with no further click, and
/// the limited state clears. Nothing else in this test touches the
/// resume path, so a cleared flag IS the fired snooze.
#[test]
fn snoozing_a_limited_session_resumes_it_when_the_timer_fires() {
    // Step one refuses (the limit); step two is what the snoozed
    // resume gets. Empty text keeps replay pacing deterministic.
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("worker.toml"),
        "[[steps]]\nlimited = true\n\n[[steps]]\ntext = ''\n",
    )
    .expect("scenario file");
    let endpoint = gertie_providers::Endpoint {
        name: "replay".into(),
        base_url: scenarios.path().to_string_lossy().into_owned(),
        model: "worker".into(),
        api_key: None,
        context_window: None,
        wire: gertie_providers::Wire::Replay,
    };
    let provider = Arc::new(
        gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).expect("replay provider"),
    );
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let mut fixture = fixture_built(
        store.clone(),
        None,
        None,
        None,
        Some(provider),
        None,
        Vec::new(),
        None,
    );

    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 30);
    assert!(
        fixture.cx.update(|cx| fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .open_session_limited()),
        "the turn ended limited before the snooze"
    );

    fixture.act(|window, cx| {
        window.click("snooze-turn", cx);
    });
    fixture.act(|window, cx| {
        assert!(
            window.try_find("cancel-snooze").is_some(),
            "snoozing is visible and cancellable"
        );
    });

    // Half an hour later, on its own: the snooze fires the resume, the
    // limit clears, the bar goes with it.
    fixture
        .cx
        .background_executor
        .advance_clock(std::time::Duration::from_secs(31 * 60));
    pump(&mut fixture, 30);
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
        .expect("session");
    assert!(
        !store
            .list_sessions()
            .unwrap()
            .iter()
            .find(|summary| summary.id == session_id)
            .expect("summary")
            .limited,
        "the snoozed resume ran by itself and cleared the limit"
    );
    fixture.act(|window, cx| {
        assert!(window.try_find("resume-turn").is_none());
        assert!(window.try_find("cancel-snooze").is_none());
    });
}

/// Canceling the snooze dissolves the promise: the timer fires into a
/// cleared field and the session stays limited and quiet — no turn
/// runs by itself.
#[test]
fn canceling_the_snooze_leaves_the_session_limited_and_quiet() {
    let scenarios = tempfile::tempdir().expect("scenario dir");
    // Step two exists but must never be reached: no auto-resume.
    std::fs::write(
        scenarios.path().join("worker.toml"),
        "[[steps]]\nlimited = true\n\n[[steps]]\ntext = ''\n",
    )
    .expect("scenario file");
    let endpoint = gertie_providers::Endpoint {
        name: "replay".into(),
        base_url: scenarios.path().to_string_lossy().into_owned(),
        model: "worker".into(),
        api_key: None,
        context_window: None,
        wire: gertie_providers::Wire::Replay,
    };
    let provider = Arc::new(
        gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).expect("replay provider"),
    );
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let mut fixture = fixture_built(
        store.clone(),
        None,
        None,
        None,
        Some(provider),
        None,
        Vec::new(),
        None,
    );

    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 30);
    fixture.act(|window, cx| {
        window.click("snooze-turn", cx);
    });
    fixture.act(|window, cx| {
        window.click("cancel-snooze", cx);
    });
    fixture.act(|window, cx| {
        assert!(
            window.try_find("cancel-snooze").is_none(),
            "the snoozed marker went with the cancel"
        );
        assert!(
            window.try_find("resume-turn").is_some(),
            "the session is still limited — the manual way back stays"
        );
    });

    fixture
        .cx
        .background_executor
        .advance_clock(std::time::Duration::from_secs(31 * 60));
    pump(&mut fixture, 30);
    assert!(
        fixture.cx.update(|cx| fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .open_session_limited()),
        "a canceled snooze never resumes by itself"
    );
}

/// A paste bigger than a message should be lands as a file chip: at
/// send the body is filed into the workspace and the message cites
/// the path — the context never carries it. A smaller paste stays
/// text (returns false, the textarea does its thing).
#[test]
fn a_big_paste_files_itself_into_the_workspace_and_the_message_cites_it() {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let snapshots_dir = tempfile::tempdir().expect("snapshots dir");
    let snapshots =
        Arc::new(gertie_core::SnapshotStore::open(snapshots_dir.path()).expect("snapshot store"));
    let workspace = tempfile::tempdir().expect("workspace dir");
    let root = workspace.path().to_path_buf();
    let mut fixture = fixture_built(
        store,
        Some(snapshots),
        Some(root.clone()),
        None,
        None,
        None,
        Vec::new(),
        None,
    );
    fixture.hold = vec![snapshots_dir, workspace];

    let small = "tiny paste".to_string();
    let big = "pasted line of text\n".repeat(2_000); // ~38 KB

    let view = fixture.view.clone();
    fixture.act(|_window, cx| {
        view.upgrade().expect("view alive").update(cx, |view, cx| {
            assert!(
                !view.attach_big_paste_for_test(&small, cx),
                "under the threshold the paste stays text"
            );
            assert!(
                view.attach_big_paste_for_test(&big, cx),
                "at or above 32 KiB the paste becomes a chip"
            );
        });
    });

    // Sending with no provider still persists the user row — enough
    // to assert the citation and the filed file.
    fixture.act(|window, cx| {
        window.input("read what I pasted", cx);
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
        .into_iter()
        .find(|row| {
            row.role == gertie_core::Role::User && row.content.contains("read what I pasted")
        })
        .expect("sent row");

    assert!(
        user.content.contains("saved to .gertie/pastes/paste-"),
        "the message cites the filed path, got: {:?}",
        user.content
    );
    assert!(
        !user.content.contains("pasted line of text"),
        "the body never rides in the message"
    );

    // Exactly one paste file, holding the whole body.
    let pastes = root.join(".gertie/pastes");
    let mut filed = std::fs::read_dir(&pastes)
        .expect("pastes dir created")
        .collect::<Result<Vec<_>, _>>()
        .expect("readable");
    assert_eq!(filed.len(), 1, "one paste filed");
    let file = filed.remove(0).path();
    assert_eq!(
        std::fs::read_to_string(&file)
            .expect("paste body")
            .trim_end_matches('\n'),
        big.trim_end_matches('\n'),
        "the filed body is the paste, whole"
    );
}

/// "Quote" on an assistant turn drops a blockquote into the composer —
/// the exact words the user is talking about, with typing room under.
#[test]
fn quoting_an_assistant_turn_lands_a_blockquote_in_the_composer() {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let session = store.create_session("quote test").expect("session");
    store
        .append_message(
            session,
            None,
            gertie_core::Role::User,
            "tell me about widgets",
        )
        .expect("user row");
    store
        .append_message(
            session,
            None,
            gertie_core::Role::Assistant,
            "Widgets are small.\nSecond line.",
        )
        .expect("assistant row");
    let mut fixture = fixture_with_store(store);

    // The resumed session renders the assistant turn at index 1.
    fixture.act(|window, cx| {
        window.click("quote-1", cx);
    });
    let value = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .composer_value_for_test(cx)
    });
    assert!(
        value.starts_with("> Widgets are small.\n> Second line.\n"),
        "the quote leads the composer as a blockquote, got: {value:?}"
    );
    assert!(
        value.ends_with("\n\n"),
        "an empty draft leaves typing room under the quote"
    );
}

/// A long response quotes bounded — enough to pin the referent, never
/// the whole answer forced back into the prompt.
#[test]
fn a_long_assistant_turn_quotes_truncated() {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let session = store.create_session("long answer").expect("session");
    store
        .append_message(
            session,
            None,
            gertie_core::Role::Assistant,
            &"word ".repeat(2_000),
        )
        .expect("assistant row");
    let mut fixture = fixture_with_store(store);

    fixture.act(|window, cx| {
        window.click("quote-0", cx);
    });
    let value = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .composer_value_for_test(cx)
    });
    assert!(
        value.contains("… [truncated]"),
        "a long response is capped, got {} chars",
        value.len()
    );
    assert!(value.len() < 1_100, "the cap is small by design");
}

/// The queue is rows, not UI state: a follow-up typed mid-turn
/// (secondary Enter) persists immediately, renders as a dashed draft
/// with real actions, and the drain fires it as its own turn when the
/// running one ends.
#[test]
fn a_queued_followup_is_a_row_that_drafts_drains_and_survives() {
    // A turn that pends on a permission card: stable "running" state —
    // the queue can be exercised against a live turn.
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("held.toml"),
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"write_file\"\n\
         arguments = '{\"path\": \"q.txt\", \"content\": \"x\"}'\n\n\
         [[steps]]\ntext = ''\n",
    )
    .expect("scenario file");
    let workspace = tempfile::tempdir().expect("workspace dir");
    let endpoint = gertie_providers::Endpoint {
        name: "replay".into(),
        base_url: scenarios.path().to_string_lossy().into_owned(),
        model: "held".into(),
        api_key: None,
        context_window: None,
        wire: gertie_providers::Wire::Replay,
    };
    let provider = Arc::new(
        gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).expect("replay provider"),
    );
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let mut fixture = fixture_built(
        store,
        None,
        Some(workspace.path().to_path_buf()),
        None,
        Some(provider),
        None,
        Vec::new(),
        None,
    );
    fixture.hold = vec![scenarios, workspace];

    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 20);

    // Mid-turn: the secondary Enter queues.
    fixture.act(|window, cx| {
        window.input("follow up please", cx);
        window.press("secondary-enter", cx);
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
    let queued = fixture.store.queued_followups(session).expect("rows");
    assert_eq!(queued.len(), 1, "the follow-up persisted as a row");
    assert_eq!(queued[0].content, "follow up please");
    fixture.act(|window, cx| {
        assert!(
            window.try_find("queue-edit-1").is_some(),
            "the draft renders with its actions"
        );
    });

    // Approve the held write: the turn finishes and the drain fires
    // the queued row as its own turn — a real user row, no composer
    // involvement.
    fixture.act(|window, cx| {
        window.click("approve-1", cx);
    });
    pump(&mut fixture, 40);
    let drained = fixture
        .store
        .load_messages(session)
        .expect("rows")
        .into_iter()
        .filter(|row| row.role == gertie_core::Role::User && row.content == "follow up please")
        .count();
    assert_eq!(drained, 1, "the queued row ran as its own turn");
    assert!(
        fixture
            .store
            .queued_followups(session)
            .expect("rows")
            .is_empty(),
        "the queue drained"
    );
}

/// A queue outlives the view that made it: rows render as drafts on
/// reopen, "send now" runs one without a composer, "edit" pulls it
/// back, "✕" removes it for real.
#[test]
fn queued_drafts_render_from_rows_and_every_action_is_real() {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let session = store.create_session("parked").expect("session");
    store
        .append_message(session, None, gertie_core::Role::User, "earlier ask")
        .expect("row");
    store
        .enqueue_followup(session, "the parked words")
        .expect("row");
    let store2 = store.clone();
    let mut fixture = fixture_with_store(store);

    // The resumed session renders the queued row as a draft with
    // actions — the words survived the restart.
    fixture.act(|window, cx| {
        assert!(
            window.try_find("queue-edit-1").is_some(),
            "the queued row drafts on reopen"
        );
    });

    // "Send now" with no live turn: its own turn, straight from the
    // row — the composer's draft (if any) untouched.
    fixture.act(|window, cx| {
        window.click("queue-send-1", cx);
    });
    pump(&mut fixture, 5);
    let sent = fixture
        .store
        .load_messages(session)
        .expect("rows")
        .into_iter()
        .filter(|row| row.role == gertie_core::Role::User && row.content == "the parked words")
        .count();
    assert_eq!(sent, 1, "send now ran the row");
    assert!(
        fixture
            .store
            .queued_followups(session)
            .expect("rows")
            .is_empty(),
        "send now took the row out"
    );
    let composer = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .composer_value_for_test(cx)
    });
    assert!(
        composer.trim().is_empty(),
        "send now never touched the composer, got: {composer:?}"
    );

    // Edit pulls a fresh row back into the composer; ✕ removes one.
    // The rows land through the store (as a restart's would) — the
    // view sees them on re-open. Row ids come from the store: a
    // drained table reuses ids.
    store2
        .enqueue_followup(session, "pull me back")
        .expect("row");
    eprintln!(
        "DEBUG rows now: {:?}",
        store2.queued_followups(session).unwrap()
    );
    fixture.act(|window, cx| {
        window.click("session-1", cx);
    });
    eprintln!(
        "DEBUG rows after reopen: {:?}",
        store2.queued_followups(session).unwrap()
    );
    let edit_id = fixture.store.queued_followups(session).expect("rows")[0]
        .id
        .0;
    fixture.act(|window, cx| {
        window.click(
            gpui_kit::SharedString::from(format!("queue-edit-{edit_id}")),
            cx,
        );
    });
    let composer = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .composer_value_for_test(cx)
    });
    assert!(
        composer.starts_with("pull me back"),
        "edit loads the draft, got: {composer:?}"
    );
    assert!(
        fixture
            .store
            .queued_followups(session)
            .expect("rows")
            .is_empty(),
        "edit took the row out"
    );

    store2.enqueue_followup(session, "drop me").expect("row");
    fixture.act(|window, cx| {
        window.click("session-1", cx);
    });
    let drop_id = fixture.store.queued_followups(session).expect("rows")[0]
        .id
        .0;
    fixture.act(|window, cx| {
        window.click(
            gpui_kit::SharedString::from(format!("queue-drop-{drop_id}")),
            cx,
        );
    });
    assert!(
        fixture
            .store
            .queued_followups(session)
            .expect("rows")
            .is_empty(),
        "the drop button removed the row"
    );
}

/// Reads the composer through the view — the same bytes a user sees.
fn composer_text(fixture: &mut Fixture) -> String {
    fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .composer_value_for_test(cx)
    })
}

/// A session with two sent prompts and the answer between them.
fn seeded_two_turn_session() -> Arc<SessionStore> {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let session = store.create_session("recalled").expect("session");
    let parent = store
        .append_message(session, None, gertie_core::Role::User, "first ask")
        .expect("row");
    let parent = store
        .append_message(
            session,
            Some(parent),
            gertie_core::Role::Assistant,
            "first answer",
        )
        .expect("row");
    store
        .append_message(session, Some(parent), gertie_core::Role::User, "second ask")
        .expect("row");
    store
}

/// Prompt recall: ArrowUp in an empty composer walks the session's
/// sent prompts, newest first; ArrowDown walks back and, past the
/// newest, hands the composer its (empty) draft back.
#[test]
fn arrow_up_recalls_sent_prompts_and_arrow_down_returns() {
    let store = seeded_two_turn_session();
    let mut fixture = fixture_with_store(store);

    assert_eq!(composer_text(&mut fixture), "");
    fixture.act(|window, cx| window.press("up", cx));
    assert_eq!(composer_text(&mut fixture), "second ask");
    fixture.act(|window, cx| window.press("up", cx));
    assert_eq!(composer_text(&mut fixture), "first ask");
    // Clamped at the oldest sent prompt.
    fixture.act(|window, cx| window.press("up", cx));
    assert_eq!(composer_text(&mut fixture), "first ask");
    // Down walks back to the newest, then past it restores the draft.
    fixture.act(|window, cx| window.press("down", cx));
    assert_eq!(composer_text(&mut fixture), "second ask");
    fixture.act(|window, cx| window.press("down", cx));
    assert_eq!(composer_text(&mut fixture), "");
}

/// Typing ends the walk: the next ArrowUp is a cursor move again, not
/// a jump through history — the edit survives.
#[test]
fn typing_ends_the_recall_walk() {
    let store = seeded_two_turn_session();
    let mut fixture = fixture_with_store(store);

    fixture.act(|window, cx| window.press("up", cx));
    fixture.act(|window, cx| window.input("!", cx));
    fixture.act(|window, cx| window.press("up", cx));
    assert_eq!(composer_text(&mut fixture), "second ask!");
}

/// A half-typed draft never gets clobbered: with content in the
/// composer, the arrows stay cursor moves.
#[test]
fn arrows_never_clobber_a_half_typed_draft() {
    let store = seeded_two_turn_session();
    let mut fixture = fixture_with_store(store);

    fixture.act(|window, cx| window.input("draft in progress", cx));
    fixture.act(|window, cx| window.press("up", cx));
    assert_eq!(composer_text(&mut fixture), "draft in progress");
}

/// A fresh install has nothing sent: ArrowUp is a no-op, not a panic,
/// not a phantom prompt.
#[test]
fn arrow_up_with_no_history_leaves_the_composer_alone() {
    let mut fixture = fixture();

    fixture.act(|window, cx| window.press("up", cx));
    assert_eq!(composer_text(&mut fixture), "");
}

/// "Edit from here": the transcript rewinds to the kept prefix, the
/// question returns as a draft, and the NEXT send parents to the keep
/// point — the abandoned branch stays in the ledger as audit while
/// dropping off every read.
#[test]
fn edit_from_here_keeps_changes_and_parents_the_next_send_to_the_keep_point() {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let session = store.create_session("rewound").expect("session");
    let q1 = store
        .append_message(session, None, gertie_core::Role::User, "first ask")
        .expect("row");
    let a1 = store
        .append_message(
            session,
            Some(q1),
            gertie_core::Role::Assistant,
            "first answer",
        )
        .expect("row");
    store
        .append_message(session, Some(a1), gertie_core::Role::User, "second ask")
        .expect("row");
    let mut fixture = fixture_with_store(store);

    // Asking opens the confirm row — a choice, not a fait accompli.
    // No checkpoint ledger in this fixture, so there is no files
    // offer to make.
    fixture.act(|window, cx| window.click("rewind-2", cx));
    fixture.act(|window, cx| {
        assert!(
            window.try_find("rewind-keep").is_some(),
            "the confirm row appears"
        );
        assert!(
            window.try_find("rewind-files").is_none(),
            "no ledger, no files offer"
        );
        window.click("rewind-cancel", cx);
    });
    assert_eq!(turn_count_of(&mut fixture), 3, "cancel changes nothing");

    // Keep-changes: files untouched (there are none), transcript is
    // the kept prefix, the question is the composer's draft.
    fixture.act(|window, cx| window.click("rewind-2", cx));
    fixture.act(|window, cx| window.click("rewind-keep", cx));
    assert_eq!(
        turn_count_of(&mut fixture),
        2,
        "the transcript is the prefix"
    );
    assert_eq!(composer_text(&mut fixture), "second ask");

    // The rewind is real on the next send: it parents to the keep
    // point, so the canonical path reads the new branch and the
    // abandoned question stays in the table as audit.
    fixture.act(|window, cx| {
        window.input(" — edited", cx);
        window.press("enter", cx);
    });
    let rows = fixture.store.load_messages(session).expect("rows");
    let contents: Vec<&str> = rows.iter().map(|row| row.content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["first ask", "first answer", "second ask — edited"]
    );
    // (The abandoned row's survival as audit is pinned in the core
    // store tests — the rowids here are the store's to hand out.)
}

/// "Revert files too": the restore walks the ledger turn by turn —
/// the file goes back to its before content, then the transcript
/// truncates. Never files-without-history, never the reverse.
#[test]
fn revert_files_too_restores_the_checkpointed_content() {
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("edits.toml"),
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"write_file\"\n\
         arguments = '{\"path\": \"scratch.txt\", \"content\": \"auto\"}'\n\n\
         [[steps]]\ntext = ''\n",
    )
    .expect("scenario file");
    let workspace = tempfile::tempdir().expect("workspace dir");
    let snapshots_dir = tempfile::tempdir().expect("snapshots dir");
    let snapshots =
        Arc::new(gertie_core::SnapshotStore::open(snapshots_dir.path()).expect("snapshot store"));
    let endpoint = gertie_providers::Endpoint {
        name: "replay".into(),
        base_url: scenarios.path().to_string_lossy().into_owned(),
        model: "edits".into(),
        api_key: None,
        context_window: None,
        wire: gertie_providers::Wire::Replay,
    };
    let provider = Arc::new(
        gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).expect("replay provider"),
    );
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let workspace_path = workspace.path().to_path_buf();
    let mut fixture = fixture_built(
        store,
        Some(snapshots),
        Some(workspace_path.clone()),
        None,
        Some(provider),
        None,
        Vec::new(),
        None,
    );
    fixture.hold = vec![scenarios, workspace, snapshots_dir];

    // The scripted turn writes the file (the write needs one
    // approval in supervised mode).
    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 30);
    fixture.act(|window, cx| window.click("approve-1", cx));
    pump(&mut fixture, 40);
    assert_eq!(
        std::fs::read(workspace_path.join("scratch.txt")).expect("written"),
        b"auto"
    );

    // Rewind the first (only) turn, files included: the file's
    // before-content comes back (it didn't exist — an empty file),
    // the transcript empties, the question returns as a draft.
    fixture.act(|window, cx| window.click("rewind-0", cx));
    fixture.act(|window, cx| {
        assert!(
            window.try_find("rewind-files").is_some(),
            "the ledger offers the file restore"
        );
        window.click("rewind-files", cx);
    });
    assert_eq!(
        std::fs::read(workspace_path.join("scratch.txt")).expect("restored"),
        b"",
        "the file is back to its before content"
    );
    assert_eq!(turn_count_of(&mut fixture), 0, "the transcript is empty");
    assert_eq!(composer_text(&mut fixture), "go");
}

/// The safety guard is fail-closed: when something else streams in
/// the same checkout, the file restore refuses — and only the files
/// restore; keep-changes never needs the guard.
#[test]
fn the_guard_refuses_a_file_restore_while_another_tab_streams() {
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("edits.toml"),
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"write_file\"\n\
         arguments = '{\"path\": \"scratch.txt\", \"content\": \"auto\"}'\n\n\
         [[steps]]\ntext = ''\n",
    )
    .expect("scenario file");
    let workspace = tempfile::tempdir().expect("workspace dir");
    let snapshots_dir = tempfile::tempdir().expect("snapshots dir");
    let snapshots =
        Arc::new(gertie_core::SnapshotStore::open(snapshots_dir.path()).expect("snapshot store"));
    let endpoint = gertie_providers::Endpoint {
        name: "replay".into(),
        base_url: scenarios.path().to_string_lossy().into_owned(),
        model: "edits".into(),
        api_key: None,
        context_window: None,
        wire: gertie_providers::Wire::Replay,
    };
    let provider = Arc::new(
        gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).expect("replay provider"),
    );
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let workspace_path = workspace.path().to_path_buf();
    let busy: gertie::chat::LiveElsewhere =
        Arc::new(|_, _, _| Err("another chat is streaming in this checkout".to_string()));
    let mut fixture = fixture_built(
        store,
        Some(snapshots),
        Some(workspace_path.clone()),
        None,
        Some(provider),
        None,
        Vec::new(),
        Some(busy),
    );
    fixture.hold = vec![scenarios, workspace, snapshots_dir];

    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 30);
    fixture.act(|window, cx| window.click("approve-1", cx));
    pump(&mut fixture, 40);

    // Files restore: refused, nothing touched.
    fixture.act(|window, cx| window.click("rewind-0", cx));
    fixture.act(|window, cx| window.click("rewind-files", cx));
    assert_eq!(
        std::fs::read(workspace_path.join("scratch.txt")).expect("untouched"),
        b"auto",
        "the restore refused before touching anything"
    );
    // The refusal is visible — a notice turn joined the transcript —
    // but nothing else moved.
    assert_eq!(turn_count_of(&mut fixture), 3);

    // Keep-changes never consults the guard — it changes no files.
    fixture.act(|window, cx| window.click("rewind-keep", cx));
    assert_eq!(turn_count_of(&mut fixture), 0);
    assert_eq!(
        std::fs::read(workspace_path.join("scratch.txt")).expect("kept"),
        b"auto",
        "keep means keep"
    );
}

fn turn_count_of(fixture: &mut Fixture) -> usize {
    fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .turn_count_for_test()
    })
}
