//! Headless UI regression tests for the chat surface: typing and Enter
//! must reach the session store, with no provider configured at all,
//! the exact path a fresh install walks first.

use gertie::chat::ChatView;
use gertie_core::SessionStore;
use gpui_kit::component::Root;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext, Bounds, HeadlessAppContext, NoopTextSystem, SharedString,
    WeakEntity, WindowBounds, WindowHandle, WindowOptions, component::v_flex, div, prelude::*, px,
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
    fixture_built(store, None, None, None, None, None, Vec::new(), None, None)
}

/// A fixture whose view carries a real checkpoint ledger over a real
/// (empty) workspace, the turn-diff path end to end.
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
        None,
    );
    fixture.hold = vec![snapshots_dir, workspace];
    fixture
}

#[allow(clippy::too_many_arguments)]
fn fixture_full(
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
    // The worktree branch naming (None = a shared-workspace tab).
    naming: Option<gertie::worktree::BranchNaming>,
    // The rewind safety guard (None = single-tab tests, nothing ever
    // streams elsewhere).
    guard: Option<gertie::chat::LiveElsewhere>,
    // The memory store (None = no memory in the view).
    memory: Option<Arc<gertie_core::MemoryStore>>,
) -> Fixture {
    let mut cx = HeadlessAppContext::with_asset_source(
        Arc::new(NoopTextSystem),
        Arc::new(gpui_kit::assets::Assets),
    );
    cx.update(|cx| {
        gpui_kit::init(cx);
        // The defaults the app would read from keymap.toml, the
        // fixture binds them directly, no filesystem.
        gertie::chat::bind_chat_keys(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
    });
    let mut view_entity = None;
    let guard = guard.unwrap_or_else(|| Arc::new(|_, _, _| Ok(())));
    let root = workspace
        .clone()
        .unwrap_or_else(|| Path::new(".").to_path_buf());
    // A real session records its workspace at first send; the seeds
    // below (and above, the store arrives pre-seeded) bypass send, so
    // the fixture records for them: every seeded conversation ran in
    // the view's workspace, which is what launch-resume matches on. A
    // root a test already set on purpose is left alone.
    for summary in store.list_sessions().expect("session list") {
        if summary.workspace_root.is_none() {
            store.set_session_workspace(summary.id, &root);
        }
    }
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
                    gertie::chat::ChatViewConfig {
                        store: store.clone(),
                        provider,
                        snapshots: snapshots.clone(),
                        tools: merge_tools(
                            extra_tools.clone(),
                            workspace.as_deref().unwrap_or(Path::new(".")),
                        ),
                        endpoint_label: "no endpoint configured".into(),
                        system_prompt: None,
                        instructions: None,
                        memory_index: None,
                        memory_enabled: false,
                        memory_store: memory.clone(),
                        memory_extract_provider: None,
                        max_tool_rounds: None,
                        grace_rounds: None,
                        templates: Vec::new(),
                        workspace_root: root.clone(),
                        settle_command,
                        verify: None,
                        tool_guidance: gertie_core::ToolGuidance::Standard,
                        compact_at: None,
                        repo_map_budget: None,
                        broker: broker
                            .unwrap_or_else(|| Arc::new(gertie_core::PermissionBroker::new())),
                        session_scope: gertie_core::SessionScope::new(),
                        mode_scope: gertie_core::ModeScope::new(),
                        sandbox_bash: false,
                        default_mode: gertie_core::PermissionMode::Supervised,
                        settings_panel: panel,
                        resume: true,
                        branch_naming: naming,
                        live_elsewhere: guard.clone(),
                        switch: Arc::new(move |root: &Path| {
                            Ok(gertie::bootstrap::Switched {
                                tools: merge_tools(extra_tools.clone(), root),
                                system_prompt: None,
                                instructions: None,
                                memory_index: None,
                                memory: None,
                                memory_extract_provider: None,
                                workspace_root: root.to_path_buf(),
                                templates: Vec::new(),
                                sandbox_bash: false,
                                verify: None,
                                warnings: Vec::new(),
                            })
                        }),
                        session_router: None,
                    },
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

/// The common fixture: no naming (a shared-workspace tab), no guard.
#[allow(clippy::too_many_arguments)]
fn fixture_built(
    store: Arc<SessionStore>,
    snapshots: Option<Arc<gertie_core::SnapshotStore>>,
    workspace: Option<std::path::PathBuf>,
    settle_command: Option<String>,
    provider: Option<Arc<gertie_providers::AnyProvider>>,
    broker: Option<Arc<gertie_core::PermissionBroker>>,
    extra_tools: Vec<Arc<dyn gertie_core::Tool>>,
    memory: Option<Arc<gertie_core::MemoryStore>>,
    guard: Option<gertie::chat::LiveElsewhere>,
) -> Fixture {
    fixture_full(
        store,
        snapshots,
        workspace,
        settle_command,
        provider,
        broker,
        extra_tools,
        None,
        guard,
        memory,
    )
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
/// Rooted at the fixture's workspace, the way a real session's is,
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

    // A fresh install: no session rows yet, they are created lazily.
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

    // A session is born with the chip's posture, supervised until the
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
        store_mode(&mut fixture),
        gertie_core::PermissionMode::Supervised
    );

    // The chip's click opens the menu (the ring walk died: a click
    // could never reach full access, which stranded sessions born in
    // `auto`). The menu's rows apply directly; the full-access row
    // opens the confirm card instead of applying.
    fixture.act(|window, cx| {
        window.click("mode-chip", cx);
    });
    fixture.act(|window, cx| {
        assert!(window.try_find("mode-menu").is_some(), "the menu opened");
        window.click("mode-menu-accept-edits", cx);
    });
    assert_eq!(
        store_mode(&mut fixture),
        gertie_core::PermissionMode::AcceptEdits,
        "the picked row persists"
    );
    // Reopen, pick full access: the confirm card opens, the mode is
    // unchanged until the card's own approve.
    fixture.act(|window, cx| window.click("mode-chip", cx));
    fixture.act(|window, cx| window.click("mode-menu-full-access", cx));
    let gated = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .pending_card_index_for_test("mode_confirm")
    });
    assert!(gated.is_some(), "the confirm card gates the dangerous row");
    assert_eq!(
        store_mode(&mut fixture),
        gertie_core::PermissionMode::AcceptEdits,
        "picking full access does not apply from the menu"
    );
}

#[test]
fn the_empty_state_says_ready_and_first_run_gets_the_tour() {
    // The fixture builds views with no endpoint, so the default state
    // leads with the blocking fact and offers the fix.
    let mut default_fixture = fixture();
    let line = default_fixture.cx.update(|cx| {
        default_fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .empty_state_headline_for_test()
    });
    assert_eq!(
        line, "Ready to run, once an endpoint exists.",
        "the missing endpoint leads the state"
    );
    default_fixture.act(|window, _| {
        assert!(
            window.try_find("empty-state-configure").is_some(),
            "the fix is offered"
        );
    });

    // With an endpoint the state relaxes: "Ready." plus the tour, and
    // no configure button.
    let mut configured = fixture();
    configured.cx.update(|cx| {
        let view = configured.view.upgrade().expect("view alive");
        let view = view.downgrade();
        let _ = view.update(cx, |chat, cx| {
            chat.set_endpoint_label_for_test("ppq · glm-5.3-flash", cx);
        });
    });
    let line = configured.cx.update(|cx| {
        configured
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .empty_state_headline_for_test()
    });
    assert_eq!(line, "Ready.");
    configured.act(|window, _| {
        assert!(
            window.try_find("empty-state-configure").is_none(),
            "no configure button when an endpoint exists"
        );
    });

    // No endpoint: the blocking fact leads and the fix is offered.
    let mut empty = fixture();
    empty.cx.update(|cx| {
        let view = empty.view.upgrade().expect("view alive");
        let view = view.downgrade();
        let _ = view.update(cx, |chat, cx| {
            chat.set_endpoint_label_for_test("no endpoint configured", cx);
        });
    });
    let line = empty.cx.update(|cx| {
        empty
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .empty_state_headline_for_test()
    });
    assert_eq!(
        line, "Ready to run, once an endpoint exists.",
        "the missing endpoint leads the state"
    );
}

/// A memory store over tempdir roots, the panel and footer tests'
/// fixture. The store's session db is in-memory, its file roots are
/// the held tempdir.
fn fixture_with_memory() -> (Fixture, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("memory dir");
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("session store"));
    let memory = Arc::new(gertie_core::MemoryStore::open(
        dir.path().join("project"),
        dir.path().join("global"),
        store.clone(),
    ));
    let fixture = fixture_built(
        store,
        None,
        None,
        None,
        None,
        None,
        Vec::new(),
        Some(memory),
        None,
    );
    (fixture, dir)
}

#[test]
fn the_gauge_names_the_compaction_before_it_happens() {
    let mut fixture = fixture();
    // Under the threshold: the gauge says only the size.
    let line = fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        view.update(cx, |chat, cx| {
            chat.set_context_for_test(Some(100_000), 10_000, cx);
        });
        view.read(cx).gauge_line_for_test()
    });
    assert!(
        !line.contains("next compaction"),
        "quiet until near: {line}"
    );
    // At 75% of the window the sentence appears.
    let line = fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        view.update(cx, |chat, cx| {
            chat.set_context_for_test(Some(100_000), 80_000, cx);
        });
        view.read(cx).gauge_line_for_test()
    });
    assert!(
        line.contains("next compaction keeps the last 8 messages"),
        "the mechanism, not a guess: {line}"
    );
}

/// /compact on an idle session: the gauge drops to the compacted size
/// (it showed the last request's 400k until the next request), and no
/// run clock starts, because nothing runs.
#[test]
fn compact_shrinks_the_gauge_and_starts_no_run() {
    let mut fixture = fixture();
    fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        view.update(cx, |chat, cx| {
            chat.set_context_for_test(Some(200_000), 190_000, cx);
            chat.fill_history_for_test(200, 4_000);
            chat.compact_for_test(cx);
        });
    });
    fixture.cx.run_until_parked();
    let (line, running, turns) = fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        let chat = view.read(cx);
        (
            chat.gauge_line_for_test(),
            chat.run_clock_running_for_test(),
            chat.turn_count_for_test(),
        )
    });
    assert!(
        !line.contains("190k"),
        "the gauge still shows the old size: {line}"
    );
    assert!(!line.contains("next compaction"), "{line}");
    assert!(!running, "an idle /compact started the run clock");
    assert!(turns >= 1, "the compaction note is in the transcript");
}

#[test]
fn find_walks_the_transcript_and_escape_hands_focus_back() {
    let mut fixture = fixture();
    // A turn to find: the user's message persists without a provider.
    fixture.act(|window, cx| {
        window.input("the flake hides in teardown", cx);
        window.press("enter", cx);
    });
    // Ctrl+F opens the bar; typing narrows; the count shows.
    fixture.act(|window, cx| {
        window.press("ctrl-f", cx);
    });
    let opened = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .find_open_for_test()
    });
    assert!(opened, "ctrl-f opened the find bar");
    // Typing a query that matches the session's turns puts the bar on
    // the first match; the highlight reads the same row.
    fixture.act(|window, cx| {
        window.input("flake", cx);
    });
    let row = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .current_match_row_for_test()
    });
    assert!(row.is_some(), "a match exists for the query");

    // Escape closes it.
    fixture.act(|window, cx| {
        window.press("escape", cx);
    });
    let opened = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .find_open_for_test()
    });
    assert!(!opened, "escape closed the find bar");
}

#[test]
fn mention_matches_lists_workspace_files_and_erases_the_token_on_accept() {
    // The workspace: two files, one in a scratch dir the walk skips.
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(workspace.path().join("src")).expect("src");
    std::fs::write(workspace.path().join("src").join("main.rs"), "fn main() {}").expect("src");
    std::fs::write(workspace.path().join("README.md"), "# g").expect("readme");
    std::fs::create_dir_all(workspace.path().join("target")).expect("target");
    std::fs::write(workspace.path().join("target").join("junk.rs"), "x").expect("junk");

    // The bare `@` offers nothing (a query means something already).
    let empty = gertie::chat::mention_matches_for_test("@", workspace.path());
    assert!(empty.is_empty(), "a bare @ stays quiet");

    // The query narrows: `@main` finds src/main.rs, never target/.
    let hits = gertie::chat::mention_matches_for_test("@main", workspace.path());
    assert_eq!(hits.len(), 1, "only src/main.rs matches: {:?}", hits);
    assert_eq!(hits[0].path, "src/main.rs");
    assert_eq!(hits[0].token, "@main");

    // A query with a space is a sentence again, not a mention.
    assert!(gertie::chat::mention_matches_for_test("what is @ up to", workspace.path()).is_empty());

    // End to end: typing a token in the composer, Enter accepts the
    // top row, the token leaves the composer and the file attaches.
    let mut fixture = fixture_full(
        Arc::new(SessionStore::open(Path::new(":memory:")).expect("store")),
        None,
        Some(workspace.path().to_path_buf()),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
        None,
    );
    fixture.act(|window, cx| {
        window.input("@main", cx);
        window.press("enter", cx);
    });
    fixture.act(|window, cx| {
        window.press("enter", cx);
    });
    let composer = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .composer_value_for_test(cx)
    });
    assert_eq!(composer.trim(), "", "the token left the composer");
    // The attachment is parked: it rides on the next send.
    assert_eq!(
        fixture.cx.update(|cx| {
            fixture
                .view
                .upgrade()
                .expect("view alive")
                .read(cx)
                .attachments_count_for_test()
        }),
        1,
        "the mentioned file is attached"
    );
}

#[test]
fn the_arrows_walk_the_mention_popup_and_enter_takes_the_walked_row() {
    // The workspace: two files the query matches, ordered by path.
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("a.txt"), "first").expect("a");
    std::fs::write(workspace.path().join("b.txt"), "second").expect("b");
    let mut fixture = fixture_full(
        Arc::new(SessionStore::open(Path::new(":memory:")).expect("store")),
        None,
        Some(workspace.path().to_path_buf()),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
        None,
    );

    // Type the shared query; ArrowDown once walks to the second row;
    // Enter accepts THAT row, not the top one.
    fixture.act(|window, cx| {
        window.input("@.txt", cx);
    });
    // Walk (the arrows' behavior through recall_step is exercised by
    // the popup-open check; the dispatch itself is the PR 4 e2e's
    // path). Selection 1 = the second row (b.txt).
    fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        let view = view.downgrade();
        let _ = view.update(cx, |chat, cx| {
            chat.set_popup_selection_for_test(Some(1), cx);
        });
    });
    fixture.act(|window, cx| {
        window.press("enter", cx);
    });
    // The walked row attached (b.txt), the top row (a.txt) didn't:
    // one attachment, and the token left the composer.
    let count = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .attachments_count_for_test()
    });
    assert_eq!(count, 1, "one walked row attached, not a send");
    let composer = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .composer_value_for_test(cx)
    });
    assert_eq!(composer.trim(), "", "the token left the composer");
    // The dispatch went to the popup, not to a send: no user turn.
    assert_eq!(
        fixture.cx.update(|cx| {
            fixture
                .view
                .upgrade()
                .expect("view alive")
                .read(cx)
                .turn_count_for_test()
        }),
        0,
        "no turn was sent"
    );
}

#[test]
fn a_new_query_resets_the_popup_walk() {
    // Walk to row 2 (state-driven, the dispatch is PR 4's), then edit
    // the query: the selection belonged to the last list, so the next
    // Enter takes the new list's top row, not the walked one.
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("a.txt"), "first").expect("a");
    std::fs::write(workspace.path().join("ab.txt"), "second").expect("b");
    let mut fixture = fixture_full(
        Arc::new(SessionStore::open(Path::new(":memory:")).expect("store")),
        None,
        Some(workspace.path().to_path_buf()),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
        None,
    );
    fixture.act(|window, cx| {
        window.input("@a", cx);
    });
    fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        let view = view.downgrade();
        let _ = view.update(cx, |chat, cx| {
            chat.set_popup_selection_for_test(Some(1), cx);
        });
    });
    fixture.act(|window, cx| {
        window.input("b", cx);
    });
    let reset = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .popup_selection_for_test()
    });
    assert_eq!(reset, None, "a new query reset the walk");
    fixture.act(|window, cx| {
        window.press("enter", cx);
    });
    // The new query "@ab" matches only ab.txt; Enter attached it.
    let count = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .attachments_count_for_test()
    });
    assert_eq!(count, 1, "the new query's top row attached");
}

#[test]
fn the_memory_footer_word_shows_the_count_and_opens_the_panel() {
    let (mut fixture, dir) = fixture_with_memory();

    // An empty store: the word hides, memoryless sessions pay zero
    // attention (the zero-cost design, visible).
    fixture.act(|window, cx| {
        window.input("hello", cx);
        window.press("enter", cx);
    });
    let mut found = false;
    fixture.act(|window, _| {
        found = window.try_find("memory-chip").is_some();
    });
    assert!(!found, "empty store hides the word");

    // One remembered entry: the word appears with the count, and a
    // click opens the memory panel.
    std::fs::create_dir_all(dir.path().join("project")).expect("tier root");
    std::fs::write(
        dir.path().join("project").join("gotchas.md"),
        "# Gotchas\n\n- [p1] helpful=0 harmful=0 the flake hides in teardown [source: 9; added: 2026-10-31]\n",
    )
    .expect("topic file");
    fixture.act(|window, cx| {
        window.input("second turn", cx);
        window.press("enter", cx);
    });
    assert_eq!(
        fixture.cx.update(|cx| {
            fixture
                .view
                .upgrade()
                .expect("view alive")
                .read(cx)
                .memory_count_for_test()
        }),
        1,
        "the scan sees the entry"
    );
    let mut found = false;
    fixture.act(|window, _| {
        found = window.try_find("memory-chip").is_some();
    });
    assert!(found, "one entry shows the word");
    fixture.act(|window, cx| {
        window.click("memory-chip", cx);
    });
    assert_eq!(
        fixture.cx.update(|cx| fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .panel_for_test()),
        "memory",
        "the click opened the memory panel"
    );
}

#[test]
fn a_down_vote_alone_lands() {
    let (mut fixture, dir) = fixture_with_memory();
    let file = dir.path().join("project").join("gotchas.md");
    std::fs::create_dir_all(dir.path().join("project")).expect("tier root");
    std::fs::write(
        &file,
        "# Gotchas\n\n- [p1] helpful=0 harmful=0 the flake hides in teardown [source: 9; added: 2026-10-31]\n",
    )
    .expect("topic file");
    fixture.act(|window, cx| {
        window.input("hello", cx);
        window.press("enter", cx);
    });
    fixture.act(|window, cx| {
        window.click("memory-chip", cx);
    });
    fixture.act(|window, cx| {
        window.click("memory-vote-down-p1", cx);
    });
    let text = std::fs::read_to_string(&file).expect("topic file");
    assert!(
        text.contains("helpful=0 harmful=1"),
        "the down vote landed: {text}"
    );
}

/// A superseded entry's row says what replaced it, and a linked
/// entry's row names its links: the structure the files carry is
/// visible where the entries are.
#[test]
fn the_panel_shows_the_replacement_chain_and_links() {
    let (mut fixture, dir) = fixture_with_memory();
    std::fs::create_dir_all(dir.path().join("project")).expect("tier root");
    std::fs::write(
        dir.path().join("project").join("decisions.md"),
        "- [p1] helpful=0 harmful=0 tabs [superseded by: p2; valid until: 2026-11-01]\n\
         - [p2] helpful=0 harmful=0 spaces [superseded by: p3; valid until: 2026-11-20]\n\
         - [p3] helpful=0 harmful=0 spaces, four wide [links: builds_on p2]\n",
    )
    .expect("topic file");
    fixture.act(|window, cx| {
        window.input("hello", cx);
        window.press("enter", cx);
    });
    fixture.act(|window, cx| {
        window.click("memory-chip", cx);
    });
    let mut shown = (false, false, false);
    fixture.act(|window, _| {
        shown = (
            window.try_find("memory-structure-p1").is_some(),
            window.try_find("memory-structure-p3").is_some(),
            window.try_find("memory-vote-up-p3").is_some(),
        );
    });
    assert!(shown.2, "the panel is open with its rows");
    assert!(shown.0, "a superseded entry shows its chain");
    assert!(shown.1, "a linked entry shows its links");
}

/// An entry whose cited file changed since it was written says so in
/// the panel; one whose file still matches says nothing.
#[test]
fn the_panel_flags_an_entry_whose_evidence_changed() {
    let (mut fixture, dir) = fixture_with_memory();
    std::fs::create_dir_all(dir.path().join("project")).expect("tier root");
    std::fs::write(dir.path().join("pool.rs"), "const SIZE: u32 = 16;\n").expect("cited file");
    let current = gertie_core::memory::blob_hash(b"const SIZE: u32 = 16;\n");
    std::fs::write(
        dir.path().join("project").join("decisions.md"),
        format!(
            "- [p1] helpful=0 harmful=0 eight connections [evidence: pool.rs@000000000000]\n\
             - [p2] helpful=0 harmful=0 sixteen connections [evidence: pool.rs@{current}]\n"
        ),
    )
    .expect("topic file");
    fixture.act(|window, cx| {
        window.input("hello", cx);
        window.press("enter", cx);
    });
    fixture.act(|window, cx| {
        window.click("memory-chip", cx);
    });
    let mut shown = (false, false, false);
    fixture.act(|window, _| {
        shown = (
            window.try_find("memory-vote-up-p1").is_some(),
            window.try_find("memory-drift-p1").is_some(),
            window.try_find("memory-drift-p2").is_some(),
        );
    });
    assert!(shown.0, "the panel is open with its rows");
    assert!(shown.1, "the stale entry is flagged");
    assert!(!shown.2, "the current entry is not");
}

/// A flagged entry carries keep and archive in the panel: keep clears
/// the flag and leaves the belief, archive retires the entry.
#[test]
fn the_panel_settles_a_recheck_flag() {
    let (mut fixture, dir) = fixture_with_memory();
    std::fs::create_dir_all(dir.path().join("project")).expect("tier root");
    let file = dir.path().join("project").join("decisions.md");
    std::fs::write(
        &file,
        "- [p1] helpful=0 harmful=0 old [superseded by: p4; valid until: 2026-11-01]\n\
         - [p2] helpful=0 harmful=0 kept belief [recheck: p1]\n\
         - [p3] helpful=0 harmful=0 doomed belief [recheck: p1]\n\
         - [p4] helpful=0 harmful=0 new\n",
    )
    .expect("topic file");
    fixture.act(|window, cx| {
        window.input("hello", cx);
        window.press("enter", cx);
    });
    fixture.act(|window, cx| {
        window.click("memory-chip", cx);
    });
    let mut shown = (false, false);
    fixture.act(|window, _| {
        shown = (
            window.try_find("memory-keep-p2").is_some(),
            window.try_find("memory-keep-p4").is_some(),
        );
    });
    assert!(shown.0, "a flagged entry offers keep");
    assert!(!shown.1, "an unflagged one does not");
    fixture.act(|window, cx| {
        window.click("memory-keep-p2", cx);
    });
    fixture.act(|window, cx| {
        window.click("memory-archive-p3", cx);
    });
    let text = std::fs::read_to_string(&file).expect("topic file");
    assert!(
        text.contains("- [p2] helpful=0 harmful=0 kept belief\n"),
        "{text}"
    );
    assert!(
        text.contains("doomed belief [recheck: p1] [archived: "),
        "{text}"
    );
}

#[test]
fn voting_through_the_panel_writes_the_counters() {
    let (mut fixture, dir) = fixture_with_memory();
    let file = dir.path().join("project").join("gotchas.md");
    std::fs::create_dir_all(dir.path().join("project")).expect("tier root");
    std::fs::write(
        &file,
        "# Gotchas\n\n- [p1] helpful=0 harmful=0 the flake hides in teardown [source: 9; added: 2026-10-31]\n",
    )
    .expect("topic file");
    fixture.act(|window, cx| {
        window.input("hello", cx);
        window.press("enter", cx);
    });
    fixture.act(|window, cx| {
        window.click("memory-chip", cx);
    });

    let mut vote_btn = false;
    fixture.act(|window, _| {
        vote_btn = window.try_find("memory-vote-up-p1").is_some();
    });
    assert!(vote_btn, "the vote button is registered in the tree");
    let panel = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .panel_for_test()
    });
    assert_eq!(panel, "memory", "the panel is open before voting");
    assert_eq!(
        fixture.cx.update(|cx| fixture
            .view
            .upgrade()
            .unwrap()
            .read(cx)
            .memory_rows_count_for_test()),
        1,
        "the panel's rows are loaded"
    );

    // One helpful vote through the panel's real button: the row's
    // label updates and the file line carries the new counter.
    fixture.act(|window, cx| {
        window.click("memory-vote-up-p1", cx);
    });
    let notices = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .notice_texts_for_test()
    });
    let text = std::fs::read_to_string(&file).expect("topic file");
    assert!(
        text.contains("helpful=1 harmful=0"),
        "the vote landed in the file: {text}; notices: {notices:?}"
    );
    // The panel refreshed: close and reopen it (a fresh scan), then
    // the second vote lands the same way the first did.
    fixture.act(|window, cx| {
        window.click("memory-chip", cx);
    });
    fixture.act(|window, cx| {
        window.click("memory-chip", cx);
    });
    fixture.act(|window, cx| {
        window.click("memory-vote-down-p1", cx);
    });
    let notices2 = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .notice_texts_for_test()
    });
    let text = std::fs::read_to_string(&file).expect("topic file");
    assert!(
        text.contains("helpful=1 harmful=1"),
        "the second vote landed too: {text}; notices: {notices2:?}"
    );
}

#[test]
fn full_access_costs_a_palette_command_and_a_confirm() {
    // The chip alone can never reach full access; the palette command
    // opens a confirm card, and only its approve applies the mode.
    let mut fixture = fixture();
    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    let mode = |fixture: &mut Fixture| {
        fixture.cx.update(|cx| {
            fixture
                .view
                .upgrade()
                .expect("view alive")
                .read(cx)
                .mode_for_test()
        })
    };

    // Hammer the chip through the ring: full access never appears.
    for _ in 0..6 {
        fixture.act(|window, cx| {
            window.click("mode-chip", cx);
        });
        assert_ne!(
            mode(&mut fixture),
            gertie_core::PermissionMode::FullAccess,
            "the chip never lands on full access"
        );
    }

    // The palette command opens the confirm card: the pending ask is
    // real broker machinery, and the card's buttons are real UI.
    let open_confirm = |fixture: &mut Fixture| {
        fixture.cx.update(|cx| {
            let view = fixture.view.upgrade().expect("view alive");
            let view = view.downgrade();
            let _ = view.update(cx, |chat, cx| chat.full_access_confirm(cx));
        });
    };
    let card_index = |fixture: &mut Fixture| {
        fixture
            .cx
            .update(|cx| {
                fixture
                    .view
                    .upgrade()
                    .expect("view alive")
                    .read(cx)
                    .pending_card_index_for_test("mode_confirm")
            })
            .expect("pending confirm card");
    };
    open_confirm(&mut fixture);
    card_index(&mut fixture);
    let index = fixture
        .cx
        .update(|cx| {
            fixture
                .view
                .upgrade()
                .expect("view alive")
                .read(cx)
                .pending_card_index_for_test("mode_confirm")
        })
        .expect("pending confirm card");

    // Deny first: the mode stays where it was.
    fixture.act(|window, cx| {
        window.click(SharedString::from(format!("deny-{index}")), cx);
    });
    assert_ne!(
        mode(&mut fixture),
        gertie_core::PermissionMode::FullAccess,
        "deny leaves the mode alone"
    );

    // Approve the second confirm: the mode applies from next turn.
    open_confirm(&mut fixture);
    let index = fixture
        .cx
        .update(|cx| {
            fixture
                .view
                .upgrade()
                .expect("view alive")
                .read(cx)
                .pending_card_index_for_test("mode_confirm")
        })
        .expect("second confirm card");
    fixture.act(|window, cx| {
        window.click(SharedString::from(format!("approve-{index}")), cx);
    });
    assert_eq!(
        mode(&mut fixture),
        gertie_core::PermissionMode::FullAccess,
        "approve applies the mode"
    );

    // From full access the menu's supervised row walks straight home:
    // one click opens the menu, one click picks the row.
    fixture.act(|window, cx| {
        window.click("mode-chip", cx);
        window.click("mode-menu-supervised", cx);
    });
    assert_eq!(
        mode(&mut fixture),
        gertie_core::PermissionMode::Supervised,
        "full access de-escalates in one menu pick"
    );
}

#[test]
fn accept_edits_runs_the_checkpointed_edit_without_asking() {
    // A scripted turn that writes a file: the write_file call carries
    // a path, so the checkpoint hook covers it, exactly the class
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
        vision: true,
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
        None,
    );
    fixture.hold = vec![scenarios, workspace, snapshots_dir];

    // Flip to accept edits before the first send, the session is
    // born with the chip's posture.
    fixture.act(|window, cx| {
        window.click("mode-chip", cx);
        window.click("mode-menu-accept-edits", cx);
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
/// draw without panicking, the markdown renderer is hand-rolled and
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
        "closed **span and *nested* inside** end, écrit en français, 🦀 emoji",
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
    // A real session records its workspace at first send, the seed
    // bypasses send, so record it here: launch-resume matches on it.
    store.set_session_workspace(session_id, Path::new("."));

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
                    gertie::chat::ChatViewConfig {
                        store: store.clone(),
                        provider: None,
                        snapshots: None,
                        tools: gertie_tools::builtin_tools(Path::new(".")),
                        endpoint_label: "no endpoint configured".into(),
                        system_prompt: None,
                        instructions: None,
                        memory_index: None,
                        memory_enabled: false,
                        memory_store: None,
                        memory_extract_provider: None,
                        max_tool_rounds: None,
                        grace_rounds: None,
                        templates: Vec::new(),
                        workspace_root: Path::new(".").to_path_buf(),
                        settle_command: None,
                        verify: None,
                        tool_guidance: gertie_core::ToolGuidance::Standard,
                        compact_at: None,
                        repo_map_budget: None,
                        broker: Arc::new(gertie_core::PermissionBroker::new()),
                        session_scope: gertie_core::SessionScope::new(),
                        mode_scope: gertie_core::ModeScope::new(),
                        sandbox_bash: false,
                        default_mode: gertie_core::PermissionMode::Supervised,
                        settings_panel: panel,
                        resume: true,
                        branch_naming: None,
                        live_elsewhere: guard.clone(),
                        switch: Arc::new(|root: &Path| {
                            Ok(gertie::bootstrap::Switched {
                                tools: gertie_tools::builtin_tools(root),
                                system_prompt: None,
                                instructions: None,
                                memory_index: None,
                                memory: None,
                                memory_extract_provider: None,
                                workspace_root: root.to_path_buf(),
                                templates: Vec::new(),
                                sandbox_bash: false,
                                verify: None,
                                warnings: Vec::new(),
                            })
                        }),
                        session_router: None,
                    },
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

/// A reloaded write card diffs against its checkpoint: the loop froze
/// the file's before-content at approval time, keyed by the call's
/// own id, and the reloaded face asks the ledger by it. The disk
/// fallback would read the file as it sits now and tell a wrong
/// story; the ledger cannot.
#[test]
fn a_reloaded_write_card_diffs_against_its_checkpoint() {
    // The session's record: a write to a file that held one line.
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let session_id = store.create_session("wrote a file").expect("session");
    let parent = store
        .append_message(session_id, None, gertie_core::Role::User, "write it")
        .expect("user row");
    store
        .append_tool_result(
            session_id,
            Some(parent),
            "call-9",
            "write_file",
            r#"{"path": "out.txt", "content": "two\n"}"#,
            "overwrote out.txt (4 bytes)",
        )
        .expect("tool row");

    // The ledger holds the file's before-content, frozen at approval.
    let snapshots_dir = tempfile::tempdir().expect("snapshots dir");
    let snapshots =
        Arc::new(gertie_core::SnapshotStore::open(snapshots_dir.path()).expect("snapshot store"));
    let workspace = tempfile::tempdir().expect("workspace dir");
    snapshots
        .record(
            Some(session_id),
            &workspace.path().join("out.txt"),
            b"one\n",
            Some(workspace.path()),
            Some("call-9"),
        )
        .expect("checkpoint");
    // The workspace file moved on (what the write left): the disk
    // fallback would read it and tell "unchanged", the ledger cannot.
    std::fs::write(workspace.path().join("out.txt"), "two\n").expect("workspace file");

    let mut fixture = fixture_built(
        store,
        Some(snapshots),
        Some(workspace.path().to_path_buf()),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    fixture.hold = vec![snapshots_dir, workspace];

    // The resumed card's face: one add, one remove, the real change,
    // not the two additions of a no-source card nor the zero of a
    // disk read.
    let counts = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .card_diff_counts_for_test(1)
    });
    assert_eq!(
        counts,
        Some((1, 1)),
        "the diff is of the checkpoint, not disk"
    );
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
        // The one match sits at turn 6, early enough that revealing
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
    // 6, the whole point of the jump.
    assert!(
        scroll < 40,
        "the hit's turn should be scrolled into view, got {scroll}"
    );
}

/// Launch resume is workspace-scoped: the store is one global memory,
/// but a conversation rooted in another folder must not open in this
/// folder's first tab, that is how one chat's file paths ended up in
/// a different directory's fresh window.
#[test]
fn launch_resume_matches_the_workspace() {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let session = store.create_session("somewhere else").expect("session");
    store
        .append_message(session, None, gertie_core::Role::User, "hello")
        .expect("row");
    // The session ran in another folder; the view opens on ".".
    store.set_session_workspace(session, Path::new("/tmp/gertie-elsewhere"));

    let mut fixture = fixture_with_store(store);
    let resumed = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .session_id()
    });
    assert!(
        resumed.is_none(),
        "another folder's chat must not resume here"
    );

    // Clicking it routes the pristine launch tab to the session's
    // folder (rule 3): the view re-roots, the session loads, and the
    // notice says where the tab now runs. (Test fixtures carry no
    // router, so this exercises the local auto-follow path.)
    fixture.act(|window, cx| {
        window.click("session-1", cx);
    });
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
            .any(|notice| notice.contains("Resumed in /tmp/gertie-elsewhere")),
        "the resumed session re-roots the tab and says so, got: {notices:?}"
    );
    let root_after = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .workspace_root()
            .to_path_buf()
    });
    assert!(
        root_after.starts_with(Path::new("/tmp/gertie-elsewhere")),
        "the tab follows the session's workspace, got {root_after:?}"
    );
}

/// A conversation from this folder's root still resumes: the scoping
/// is a match, not a ban.
#[test]
fn launch_resume_still_takes_this_workspaces_chat() {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let session = store.create_session("this folder's chat").expect("session");
    store
        .append_message(session, None, gertie_core::Role::User, "hello")
        .expect("row");
    store.set_session_workspace(session, Path::new("."));

    let mut fixture = fixture_with_store(store);
    let resumed = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .session_id()
    });
    assert_eq!(
        resumed.map(|resumed| resumed.0),
        Some(1),
        "this folder's own chat resumes"
    );
}

/// A footer workspace switch re-roots the session at the next send:
/// the conversation follows the tab, so launch resume and the settle
/// hook land in the folder it now runs in, not the one it was born
/// in.
#[test]
fn a_send_after_a_workspace_switch_re_roots_the_session() {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let session = store.create_session("the traveling chat").expect("session");
    store
        .append_message(session, None, gertie_core::Role::User, "hello")
        .expect("row");
    store.set_session_workspace(session, Path::new("."));

    let mut fixture = fixture_with_store(store.clone());
    // The footer's switch, the apply path, portal-free.
    let view = fixture.view.clone();
    fixture.act(|_window, cx| {
        view.upgrade().expect("view alive").update(cx, |view, cx| {
            view.switch_workspace_for_test(Path::new("/tmp/gertie-moved"), cx);
        });
    });
    fixture.act(|window, cx| {
        window.input("after the move", cx);
        window.press("enter", cx);
    });
    assert_eq!(
        store.session_workspace(session).as_deref(),
        Some("/tmp/gertie-moved"),
        "the next send re-roots the session to the switched folder"
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

    // No provider configured: the turn still mints, the ledger
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

    // The ledger answers for that message, the seam a tool card's
    // "turn diff" resolves through, live or after a reload.
    let snapshots = fixture.snapshots.as_ref().expect("wired ledger");
    let turn = snapshots
        .turn_for_message(session, user.id)
        .expect("query")
        .expect("turn minted by send");
    assert_eq!(turn.user_message_id, user.id);

    // The panel opens on it, empty, since nothing mutated yet,
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
/// throughout, paging is a view concern, the record is not.
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

    // Exhausted: the pagination row retired, no stale control, and
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

/// Two chats in one folder, only the first filed under the project:
/// where a chat runs never files it.
fn project_store() -> (Arc<SessionStore>, i64) {
    let store = two_session_store();
    store.set_session_workspace(gertie_core::SessionId(1), Path::new("/w/alpha"));
    store.set_session_workspace(gertie_core::SessionId(2), Path::new("/w/alpha"));
    let project = store.add_project("alpha").expect("store").expect("named");
    store
        .set_session_project(gertie_core::SessionId(1), Some(project.id))
        .expect("file");
    (store, project.id)
}

fn shown(fixture: &mut Fixture, ids: &[&str]) -> Vec<bool> {
    let mut out = Vec::new();
    fixture.act(|window, _| {
        out = ids
            .iter()
            .map(|id| window.try_find(id.to_string()).is_some())
            .collect();
    });
    out
}

/// Projects group the sidebar: a chat filed under a project sits in
/// its group, the rest under Other, and both groups fold, the
/// project's fold persisting in the store.
#[test]
fn chats_group_under_their_project_and_the_rest_under_other() {
    let (store, id) = project_store();
    let mut fixture = fixture_with_store(store.clone());
    let header = format!("project-{id}");
    assert_eq!(
        shown(
            &mut fixture,
            &[&header, "project-other", "session-1", "session-2"]
        ),
        vec![true, true, true, true]
    );
    fixture.act(|window, cx| window.click(header.clone(), cx));
    assert_eq!(
        shown(&mut fixture, &["session-1", "session-2"]),
        vec![false, true],
        "folding the project hides its chat only"
    );
    assert!(
        store.list_projects().unwrap()[0].collapsed,
        "the fold persists"
    );
    fixture.act(|window, cx| window.click("project-other", cx));
    assert_eq!(shown(&mut fixture, &["session-2"]), vec![false]);
    assert!(store.store_flag("other_collapsed"));
}

/// Removing a project only takes it off the list: its chat stays, back
/// in the flat list (no projects left), and keeps its folder.
#[test]
fn removing_a_project_keeps_its_chats_and_their_folder() {
    let (store, id) = project_store();
    let mut fixture = fixture_with_store(store.clone());
    fixture.act(|window, cx| window.click(format!("project-remove-{id}"), cx));
    assert!(store.list_projects().unwrap().is_empty());
    assert_eq!(
        shown(
            &mut fixture,
            &[&format!("project-{id}"), "project-other", "session-1"]
        ),
        vec![false, false, true],
        "no projects: the list is flat again"
    );
    assert_eq!(
        store
            .session_workspace(gertie_core::SessionId(1))
            .as_deref(),
        Some("/w/alpha")
    );
    assert!(
        store
            .list_sessions()
            .unwrap()
            .iter()
            .all(|summary| summary.project_id.is_none())
    );
}

/// A chat moves in through its row's move list and out the same way.
#[test]
fn a_chat_moves_into_a_project_and_back_out() {
    let (store, id) = project_store();
    let mut fixture = fixture_with_store(store.clone());
    let filed = |store: &SessionStore| {
        store
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|summary| summary.id.0 == 2)
            .unwrap()
            .project_id
    };
    fixture.act(|window, cx| window.click("move-2", cx));
    assert_eq!(
        shown(&mut fixture, &[&format!("move-2-to-{id}"), "move-2-out"]),
        vec![true, false],
        "an unfiled chat is offered the project, not a way out"
    );
    fixture.act(|window, cx| window.click(format!("move-2-to-{id}"), cx));
    assert_eq!(filed(&store), Some(id));
    assert_eq!(
        shown(&mut fixture, &["project-other"]),
        vec![false],
        "Other emptied"
    );
    fixture.act(|window, cx| window.click("move-2", cx));
    fixture.act(|window, cx| window.click("move-2-out", cx));
    assert_eq!(filed(&store), None);
}

/// #31: a filed chat leaves its project in one click from its row,
/// kept whole; an unfiled chat has no such button.
#[test]
fn a_filed_chat_leaves_its_project_in_one_click() {
    let (store, id) = project_store();
    let mut fixture = fixture_with_store(store.clone());
    assert_eq!(
        shown(&mut fixture, &["unfile-1", "unfile-2"]),
        vec![true, false],
        "only the filed chat offers the way out"
    );
    fixture.act(|window, cx| window.click("unfile-1", cx));
    let sessions = store.list_sessions().unwrap();
    assert!(sessions.iter().all(|summary| summary.project_id.is_none()));
    assert_eq!(sessions.len(), 2, "unfiling deletes nothing");
    assert_eq!(store.list_projects().unwrap().len(), 1, "the project stays");
    assert_eq!(
        shown(
            &mut fixture,
            &["session-1", "unfile-1", &format!("project-{id}")]
        ),
        vec![true, false, true]
    );
    let notices = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .unwrap()
            .read(cx)
            .notice_texts_for_test()
    });
    assert!(
        notices
            .iter()
            .any(|text| text.contains("Removed from alpha")),
        "{notices:?}"
    );
}

/// #41: each session keeps its own frozen map. Switching to another
/// chat and back hands the first chat the same map, not a re-render
/// (which could differ and miss the prompt cache).
#[test]
fn a_chat_keeps_its_repo_map_across_a_switch() {
    let (store, _) = project_store();
    let mut fixture = fixture_with_store(store);
    let map_now = |fixture: &mut Fixture| {
        fixture.cx.update(|cx| {
            let view = fixture.view.upgrade().expect("view alive");
            view.update(cx, |chat, _| {
                chat.set_repo_map_budget_for_test(Some(500));
                chat.repo_map_for_test().expect("the map is on")
            })
        })
    };
    fixture.act(|window, cx| window.click("session-1", cx));
    let first = map_now(&mut fixture);
    fixture.act(|window, cx| window.click("session-2", cx));
    let second = map_now(&mut fixture);
    fixture.act(|window, cx| window.click("session-1", cx));
    let back = map_now(&mut fixture);
    assert!(
        back.same_as(&first),
        "switching back reuses the first chat's map"
    );
    assert!(!second.same_as(&first), "another chat has its own");
}

/// A project's + opens a blank chat in the current folder, filed
/// under the project when the first message creates it.
#[test]
fn a_projects_plus_starts_a_chat_filed_there() {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let project = store
        .add_project("Research")
        .expect("store")
        .expect("named");
    let store_handle = store.clone();
    let mut fixture = fixture_with_store(store);
    let root_before = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .workspace_root_for_test()
    });
    fixture.act(|window, cx| window.click(format!("project-new-{}", project.id), cx));
    let (root, session) = fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive").read(cx);
        (view.workspace_root_for_test(), view.session_id())
    });
    assert_eq!(
        root, root_before,
        "a project is not a folder: the chat stays put"
    );
    assert_eq!(session, None, "a blank chat, created by its first message");
    fixture.act(|window, cx| {
        window.input("hello", cx);
        window.press("enter", cx);
    });
    let sessions = store_handle.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].project_id, Some(project.id));
}

/// The add button asks for a name, not a folder: Enter makes the
/// project, Enter on an empty field makes nothing.
#[test]
fn the_add_button_names_a_new_project() {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let mut fixture = fixture_with_store(store.clone());
    fixture.act(|window, cx| window.click("add-project", cx));
    fixture.act(|window, cx| window.press("enter", cx));
    assert!(
        store.list_projects().unwrap().is_empty(),
        "blank is the cancel"
    );
    fixture.act(|window, cx| window.click("add-project", cx));
    fixture.act(|window, cx| {
        window.input("Side quests", cx);
        window.press("enter", cx);
    });
    let projects = store.list_projects().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].name, "Side quests");
    assert_eq!(
        shown(&mut fixture, &[&format!("project-{}", projects[0].id)]),
        vec![true]
    );
}

/// The palette finds a project by name; confirming it opens the group.
#[test]
fn the_palette_finds_a_project_and_opens_its_group() {
    let (store, id) = project_store();
    store.set_project_collapsed(id, true).expect("fold");
    let mut fixture = fixture_with_store(store.clone());
    assert_eq!(shown(&mut fixture, &["session-1"]), vec![false]);
    fixture.act(|window, cx| window.press("ctrl-k", cx));
    fixture.act(|window, cx| window.input("alpha", cx));
    fixture.act(|window, cx| window.press("enter", cx));
    assert_eq!(shown(&mut fixture, &["session-1"]), vec![true]);
    assert!(!store.list_projects().unwrap()[0].collapsed);
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

    // The fold is collapsed: the settled row paints nothing, its
    // restore click must miss until the fold expands.
    fixture.act(|window, cx| {
        assert!(window.try_find("session-1").is_none());
        window.click("settled-fold", cx);
    });

    // Open the settled session from the expanded fold, opening it
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
/// session's recorded workspace, the reclaim hook end to end: send
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
        None,
    );

    // A send records the workspace on the session, the settle hook's
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
/// transcript as a pending card, the answer rides back to the worker's
/// loop parked on it.
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
        Box::pin(async move { Ok("worker approved".into()) })
    }

    fn run_with_call<'a>(
        &'a self,
        call: &'a gertie_core::ToolCall,
        _progress: &'a gertie_core::WorkerProgress,
        turn_token: Option<gertie_core::TurnToken>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = gertie_core::ToolResult> + Send + 'a>>
    {
        let call = call.clone();
        Box::pin(async move {
            // The ask stamps with the turn token the loop handed this
            // tool: that stamp is the whole fix under test (issue #15).
            let (_id, answer) = self.broker.request(
                gertie_core::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                },
                gertie_core::PromptSource::Worker,
                turn_token,
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
/// timer, and test-executor timers only fire when the clock moves,
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

/// Pumps until a condition holds (or the pass budget runs out, at
/// which point the test's own assert fails with a real message). The
/// fixed-count pump was the audit's flake: a cold cache makes the
/// real-thread work slower than a tuned pass count, and a test that
/// needs exactly N passes fails on the first run after a build.
/// Condition-pumping makes the count a deadline, not a requirement.
fn pump_until(
    fixture: &mut Fixture,
    passes: usize,
    ready: impl Fn(&mut Fixture) -> bool,
    what: &'static str,
) {
    for _ in 0..passes {
        if ready(fixture) {
            return;
        }
        fixture
            .cx
            .background_executor
            .advance_clock(std::time::Duration::from_millis(5));
        settle(&mut fixture.cx, fixture.window.into());
    }
    panic!("pump_until: {what} never settled within {passes} passes");
}

#[test]
fn a_worker_ask_bubbles_into_the_transcript_and_the_answer_reaches_the_worker() {
    // The scripted leader turn: one call to the test's worker-ask tool,
    // then a closing step with no calls, the turn ends there. Empty
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
        vision: true,
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
        None,
    );

    // Send: the scripted leader calls worker_ask (a Running card, its
    // auto-run), which registers the ask and parks. The pump surfaces
    // it as a pending card in THIS transcript, the parent's.
    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 30);
    fixture.act(|window, _cx| {
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

/// The #15 repro, at the seam the bug lived in: two chats subscribe
/// the way two views do (each filtered by its own turn's token), a
/// worker ask arrives stamped with turn A's token, and only A's
/// subscription receives it. The other chat's fallback subscription
/// (which exists for token-absent asks, the resume path) sees
/// nothing either: a stamped ask is addressed, never broadcast.
#[test]
fn a_worker_ask_reaches_only_the_turn_that_spawned_it() {
    let broker = Arc::new(gertie_core::PermissionBroker::new());
    let token_a = broker.next_turn_token();
    let token_b = broker.next_turn_token();
    let mut chat_a = broker.subscribe(gertie_core::AskFilter::Turn(token_a));
    let mut chat_b = broker.subscribe(gertie_core::AskFilter::Turn(token_b));
    let mut fallback_b = broker.subscribe(gertie_core::AskFilter::EveryWorkerAsk);

    let (_id, answer) = broker.request(
        gertie_core::ToolCall {
            id: "w1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        },
        gertie_core::PromptSource::Worker,
        Some(token_a),
    );

    let ask = chat_a.blocking_recv().expect("the owning chat got its ask");
    assert_eq!(ask.call.id, "w1");
    assert!(
        chat_b.try_recv().is_err(),
        "the other chat's subscription never sees a foreign turn's ask"
    );
    assert!(
        fallback_b.try_recv().is_err(),
        "a stamped ask is addressed, not broadcast to fallbacks"
    );
    drop(answer);
}

/// The token-absent fallback still delivers: an unstamped worker ask
/// reaches the EveryWorkerAsk subscriber, so a pending ask saved
/// before this fix still gets answered after it.
#[test]
fn an_unstamped_worker_ask_still_reaches_the_fallback() {
    let broker = Arc::new(gertie_core::PermissionBroker::new());
    let mut rx = broker.subscribe(gertie_core::AskFilter::EveryWorkerAsk);
    let (_id, _answer) = broker.request(
        gertie_core::ToolCall {
            id: "w2".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        },
        gertie_core::PromptSource::Worker,
        None,
    );
    let ask = rx
        .blocking_recv()
        .expect("the fallback sees unstamped asks");
    assert_eq!(ask.call.id, "w2");
}

/// Quiet is the agent's silence, not the human's: while an approval
/// card waits on an answer, the status line keeps "quiet" to itself,
/// the ask's own clock tells that story. The gauge still speaks when
/// nothing waits on anyone.
#[test]
fn the_quiet_gauge_holds_its_tongue_while_an_approval_waits() {
    // The held-turn rig from the worker-ask test: a pending card with
    // the turn alive underneath it.
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
        vision: true,
    };
    let provider = Arc::new(
        gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).expect("replay provider"),
    );
    let broker = Arc::new(gertie_core::PermissionBroker::new());
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let mut fixture = fixture_built(
        store,
        None,
        None,
        None,
        Some(provider),
        Some(broker.clone()),
        vec![Arc::new(WorkerAskTool {
            broker: broker.clone(),
        })],
        None,
        None,
    );
    fixture.hold = vec![scenarios];

    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 30);

    // The card pends; the clocks age past the quiet threshold. The
    // line must not call that quiet, the human is the quiet party.
    let view = fixture.view.clone();
    fixture.act(|_window, cx| {
        view.upgrade().expect("view alive").update(cx, |view, _| {
            view.age_status_clocks_for_test(std::time::Duration::from_secs(30));
        });
    });
    let line = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .activity_line_for_test()
    });
    assert!(
        !line.contains("quiet"),
        "a waiting human is not agent quiet: {line}"
    );

    // The answer is an event: the waiting label and its clock retire
    // with the card, no instant, un-reset quiet on the other side.
    fixture.act(|window, cx| {
        window.click("approve-2", cx);
    });
    let line = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .activity_line_for_test()
    });
    assert!(
        !line.contains("Waiting for your approval"),
        "the ask's clock retires on approval: {line}"
    );
    assert!(
        !line.contains("quiet"),
        "the clocks restart when the human acts: {line}"
    );

    // With no card waiting, aged clocks speak: the gauge itself works.
    pump(&mut fixture, 30);
    fixture.act(|_window, cx| {
        view.upgrade().expect("view alive").update(cx, |view, _| {
            view.age_status_clocks_for_test(std::time::Duration::from_secs(30));
        });
    });
    let line = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .activity_line_for_test()
    });
    assert!(
        line.contains("quiet"),
        "agent-side silence is still reported: {line}"
    );
}

/// The card is the now, and the row is the now when the card is not
/// on screen. While a plain tool runs and the user is pinned to the
/// bottom, the row would only echo the card's own label, twin
/// spinners saying one sentence twice: it yields, and the run total
/// rides the card's clock line. Scrolled up, the card is off-screen
/// and the row speaks again, full label and both clocks. When the
/// run ends, the row retires with the turn.
#[test]
fn the_card_is_the_now_and_the_row_yields_to_it() {
    // A transcript tall enough to overflow the 900x640 window: the
    // scroll-up below needs a real "not at the bottom" to hold
    // against, the kit re-engages following whenever the viewport
    // still sits on the tail.
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let seeded = store.create_session("the park").expect("session");
    let filler = "a measured paragraph of transcript filler. ".repeat(12);
    let mut parent = Some(
        store
            .append_message(seeded, None, gertie_core::Role::User, "seed the run")
            .expect("user row"),
    );
    for _ in 0..7 {
        parent = Some(
            store
                .append_message(seeded, parent, gertie_core::Role::Assistant, &filler)
                .expect("assistant row"),
        );
        parent = Some(
            store
                .append_message(seeded, parent, gertie_core::Role::User, "go on")
                .expect("user row"),
        );
    }

    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("park.toml"),
        "[[steps]]\ntext = ''\n\
         [[steps.tool_calls]]\nname = 'park'\narguments = '{}'\n\
         [[steps]]\ntext = ''\n",
    )
    .expect("scenario file");
    let endpoint = gertie_providers::Endpoint {
        name: "replay".into(),
        base_url: scenarios.path().to_string_lossy().into_owned(),
        model: "park".into(),
        api_key: None,
        context_window: None,
        wire: gertie_providers::Wire::Replay,
        vision: true,
    };
    let provider = Arc::new(
        gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).expect("replay provider"),
    );
    let (release, park) = tokio::sync::oneshot::channel::<()>();
    let mut fixture = fixture_built(
        store,
        None,
        None,
        None,
        Some(provider),
        None,
        vec![Arc::new(ParkTool {
            park: std::sync::Mutex::new(Some(park)),
        })],
        None,
        None,
    );
    fixture.hold = vec![scenarios];

    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 30);

    let view = fixture.view.clone();
    let read = |fixture: &mut Fixture| {
        let view = view.clone();
        fixture.cx.update(move |cx| {
            let view = view.upgrade().expect("view alive").read(cx);
            (
                view.show_activity_row_for_test(),
                view.running_words_for_test(),
                view.activity_line_for_test(),
            )
        })
    };

    // Parked mid-run, pinned to the bottom: the row yields to the
    // card, the card's clock line carries the run total.
    let (row, words, line) = read(&mut fixture);
    assert!(!row, "the row yields to the running card at the bottom");
    let words = words.expect("a running card");
    assert!(
        words.contains("running · "),
        "the card's own clock: {words}"
    );
    assert!(
        words.contains(" · run "),
        "the run total rides the card: {words}"
    );
    assert!(
        line.contains(" · run "),
        "the row's line still carries the total for when it speaks: {line}"
    );

    // Scrolled up, the card is off-screen: the row is the now again,
    // and the total moves to it. The seeded transcript overflows the
    // window, so the scroll to row 0 is a real "not at the bottom"
    // and the kit's pause sticks through the paint.
    fixture.act(|_window, cx| {
        let view = view.upgrade().expect("view alive");
        view.update(cx, |view, _| view.scroll_to_item_for_test(0));
    });
    pump(&mut fixture, 10);
    let (row, _words, line) = read(&mut fixture);
    assert!(row, "scrolled up, the row is the now");
    assert!(
        line.contains(" · run "),
        "the run total rides the row when the row speaks: {line}"
    );

    // Released, the run ends, the row retires with the turn.
    drop(release);
    pump(&mut fixture, 30);
    let (row, _, _) = read(&mut fixture);
    assert!(!row, "the row retires when the run is over");
}

/// A read-only tool that parks until the test releases it: the rig
/// for "a tool is executing right now" with no pending ask.
struct ParkTool {
    park: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

/// The label builder's truncation contract: one line, one cap, one
/// ellipsis. A command that arrives wearing its own trailing dots
/// must not grow a second set next to ours (the screenshot bug:
/// `garbage......` in the row, `garbage...` on the card).
#[test]
fn a_label_truncates_to_a_single_ellipsis() {
    let long = format!("{{\"command\": \"echo {}\"}}", "x".repeat(120));
    let running = gertie::tools_ui::action_label("bash", &long, true);
    assert!(
        running.ends_with('…') && !running.contains("……"),
        "one ellipsis at the cap: {running}"
    );
    // Trailing dots are stripped before the tense's one ellipsis goes
    // on, so a command that ends in dots never reads as a stutter.
    let dotted = "{\"command\": \"# malformed-SSE garbage...\"}";
    let labeled = gertie::tools_ui::action_label("bash", dotted, true);
    assert!(
        labeled.ends_with("garbage`…"),
        "dots stripped, exactly one ellipsis, outside the chip: {labeled}"
    );
}

/// A shell with a fixed-height sibling above the chat view, the
/// chat view as the column's flex item. A stand-in keeps the test on
/// the real structural seam, a fixed-height sibling above a view that
/// must shrink, without pulling a whole Bootstrap in.
struct TabbedShell(gpui_kit::Entity<ChatView>);

impl gpui_kit::Render for TabbedShell {
    fn render(
        &mut self,
        _: &mut gpui_kit::Window,
        _: &mut gpui_kit::Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        v_flex()
            .size_full()
            .child(div().h(px(40.)).flex_shrink_0().child("tabs"))
            .child(self.0.clone())
    }
}

/// The window's chrome must stay inside the window: the footer's
/// ledger line under the composer, and the sidebar's Settings button
/// at the sidebar's foot. The sidebar lists every session in the
/// store, and when a run of them piled up (an agent's own probe
/// sessions: the user's window held ~35), the view's minimum height
/// grew past the window and the bottom chrome clipped away. The
/// footer read as "completely gone", with nothing removed at all.
#[test]
fn the_footer_and_the_sidebar_foot_stay_inside_the_window() {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    for i in 0..40 {
        let id = store
            .create_session(&format!("probe session {i}"))
            .expect("session");
        store
            .append_message(id, None, gertie_core::Role::User, "hello")
            .expect("user row");
        store
            .append_message(id, None, gertie_core::Role::Assistant, "hi")
            .expect("assistant row");
        store.set_session_workspace(id, Path::new("."));
    }

    let mut cx = HeadlessAppContext::with_asset_source(
        Arc::new(NoopTextSystem),
        Arc::new(gpui_kit::assets::Assets),
    );
    cx.update(|cx| {
        gpui_kit::init(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
    });
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
                    gertie::chat::ChatViewConfig {
                        store: store.clone(),
                        provider: None,
                        snapshots: None,
                        tools: gertie_tools::builtin_tools(Path::new(".")),
                        endpoint_label: "no endpoint configured".into(),
                        system_prompt: None,
                        instructions: None,
                        memory_index: None,
                        memory_enabled: false,
                        memory_store: None,
                        memory_extract_provider: None,
                        max_tool_rounds: None,
                        grace_rounds: None,
                        templates: Vec::new(),
                        workspace_root: Path::new(".").to_path_buf(),
                        settle_command: None,
                        verify: None,
                        tool_guidance: gertie_core::ToolGuidance::Standard,
                        compact_at: None,
                        repo_map_budget: None,
                        broker: Arc::new(gertie_core::PermissionBroker::new()),
                        session_scope: gertie_core::SessionScope::new(),
                        mode_scope: gertie_core::ModeScope::new(),
                        sandbox_bash: false,
                        default_mode: gertie_core::PermissionMode::Supervised,
                        settings_panel: panel,
                        resume: true,
                        branch_naming: None,
                        live_elsewhere: guard.clone(),
                        switch: Arc::new(|root: &Path| {
                            Ok(gertie::bootstrap::Switched {
                                tools: gertie_tools::builtin_tools(root),
                                system_prompt: None,
                                instructions: None,
                                memory_index: None,
                                memory: None,
                                memory_extract_provider: None,
                                workspace_root: root.to_path_buf(),
                                templates: Vec::new(),
                                sandbox_bash: false,
                                verify: None,
                                warnings: Vec::new(),
                            })
                        }),
                        session_router: None,
                    },
                    window,
                    cx,
                )
            });
            let shell = cx.new(|_| TabbedShell(view));
            cx.new(|cx| Root::new(shell, window, cx))
        })
        .expect("chat window");
    let handle: AnyWindowHandle = window.into();
    settle(&mut cx, handle);

    let read_bounds = |cx: &mut HeadlessAppContext, id: &'static str| {
        cx.update_window(handle, |_, window, _| {
            window.try_find(id).map(|s| s.bounds())
        })
        .expect("window")
    };
    let chip = read_bounds(&mut cx, "mode-chip");
    let settings = read_bounds(&mut cx, "toggle-settings");
    let height = px(640.);
    let chip = chip.unwrap_or_else(|| panic!("the footer's mode chip never painted"));
    assert!(
        chip.bottom() <= height,
        "the footer stays inside the window: bottom {} of {height}",
        chip.bottom()
    );
    let settings = settings.unwrap_or_else(|| panic!("the sidebar's Settings never painted"));
    assert!(
        settings.bottom() <= height,
        "the sidebar's Settings stays inside the window: bottom {} of {height}",
        settings.bottom()
    );
}

impl gertie_core::Tool for ParkTool {
    fn name(&self) -> &'static str {
        "park"
    }

    fn description(&self) -> String {
        "test double: parks until the test releases it".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    fn run<'a>(
        &'a self,
        _arguments: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = gertie_core::ToolResult> + Send + 'a>>
    {
        let park = self.park.lock().expect("park").take();
        Box::pin(async move {
            if let Some(park) = park {
                let _ = park.await;
            }
            Ok("released".into())
        })
    }
}

/// The §2.4 pin: ✕ deletes without a "sure?" step, the row leaves
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
    fixture.act(|window, _cx| {
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
    fixture.act(|window, _cx| {
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
    fixture.act(|window, _cx| {
        assert!(
            window.try_find("session-1").is_some(),
            "the undone row returns to its list"
        );
        assert!(
            window.try_find("undo-delete").is_none(),
            "the toast is spent"
        );
    });

    // The window's task still fires, and finds nothing pending.
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

/// Ctrl+Z outside the composer fires the undo-delete action, the
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
    fixture.act(|window, _cx| {
        assert!(
            window.try_find("session-1").is_some(),
            "ctrl-z undid the delete via the global binding"
        );
    });
}

/// The §2.3 pin, end to end through the replay provider: a turn that
/// ends on the provider's rate limit leaves the session Limited,
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
        vision: true,
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
        "limited persists in the store, it survives a restart"
    );
    fixture.act(|window, _cx| {
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
    fixture.act(|window, _cx| {
        assert!(
            window.try_find("resume-turn").is_none(),
            "the bar drops once the session is no longer limited"
        );
    });
}

/// The snooze pin: "Snooze 30 min" schedules the resume itself, half
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
        vision: true,
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
    fixture.act(|window, _cx| {
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
    fixture.act(|window, _cx| {
        assert!(window.try_find("resume-turn").is_none());
        assert!(window.try_find("cancel-snooze").is_none());
    });
}

/// Canceling the snooze dissolves the promise: the timer fires into a
/// cleared field and the session stays limited and quiet, no turn
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
        vision: true,
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
    fixture.act(|window, _cx| {
        assert!(
            window.try_find("cancel-snooze").is_none(),
            "the snoozed marker went with the cancel"
        );
        assert!(
            window.try_find("resume-turn").is_some(),
            "the session is still limited, the manual way back stays"
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
/// the path, the context never carries it. A smaller paste stays
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

    // Sending with no provider still persists the user row, enough
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

/// "Quote" on an assistant turn drops a blockquote into the composer,
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
        window.hover("turn-1", cx);
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

/// A long response quotes bounded, enough to pin the referent, never
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
        window.hover("turn-0", cx);
    });
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
/// (Enter) persists immediately, renders as a dashed draft
/// with real actions, and the drain fires it as its own turn when the
/// running one ends.
#[test]
fn a_queued_followup_is_a_row_that_drafts_drains_and_survives() {
    // A turn that pends on a permission card: stable "running" state,
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
        vision: true,
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
        None,
    );
    fixture.hold = vec![scenarios, workspace];

    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 20);

    // Mid-turn: Enter queues.
    fixture.act(|window, cx| {
        window.input("follow up please", cx);
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
    let queued = fixture.store.queued_followups(session).expect("rows");
    assert_eq!(queued.len(), 1, "the follow-up persisted as a row");
    assert_eq!(queued[0].content, "follow up please");
    fixture.act(|window, _cx| {
        assert!(
            window.try_find("queue-edit-1").is_some(),
            "the draft renders with its actions"
        );
    });

    // Approve the held write: the turn finishes and the drain fires
    // the queued row as its own turn, a real user row, no composer
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
    // actions, the words survived the restart.
    fixture.act(|window, _cx| {
        assert!(
            window.try_find("queue-edit-1").is_some(),
            "the queued row drafts on reopen"
        );
    });

    // "Send now" with no live turn: its own turn, straight from the
    // row, the composer's draft (if any) untouched.
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
    // The rows land through the store (as a restart's would), the
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

/// Reads the composer through the view, the same bytes a user sees.
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
/// a jump through history, the edit survives.
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
/// point, the abandoned branch stays in the ledger as audit while
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

    // Asking opens the confirm row, a choice, not a fait accompli.
    // No checkpoint ledger in this fixture, so there is no files
    // offer to make. Rewind-2 is the latest turn, so its affordance
    // row is visible without hover (the hover-reveal idiom).
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
        window.input(", edited", cx);
        window.press("enter", cx);
    });
    let rows = fixture.store.load_messages(session).expect("rows");
    let contents: Vec<&str> = rows.iter().map(|row| row.content.as_str()).collect();
    assert_eq!(
        contents,
        vec!["first ask", "first answer", "second ask, edited"]
    );
    // (The abandoned row's survival as audit is pinned in the core
    // store tests, the rowids here are the store's to hand out.)
}

/// "Revert files too": the restore walks the ledger turn by turn,
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
        vision: true,
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
    // before-content comes back (it didn't exist, an empty file),
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
/// the same checkout, the file restore refuses, and only the files
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
        vision: true,
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
        None,
        Some(busy),
    );
    fixture.hold = vec![scenarios, workspace, snapshots_dir];

    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    // The approval lands, then the turn runs to its done state. Both
    // waits pump-until (the audit's flake: a cold cache made the
    // fixed counts fail on first run).
    // The pending write card: pump until an approve-* button exists
    // in the tree (any index). Indexing by a literal is the old
    // flake's other half.
    let card_up = std::cell::Cell::new(false);
    pump_until(
        &mut fixture,
        40,
        |fixture| {
            fixture.act(|window, _| {
                card_up.set((0..8).any(|i| {
                    window
                        .try_find(gpui_kit::ElementId::from(format!("approve-{i}")))
                        .is_some()
                }));
            });
            card_up.get()
        },
        "the write card pends",
    );
    fixture.act(|window, cx| window.click("approve-1", cx));
    pump_until(
        &mut fixture,
        60,
        |fixture| {
            !fixture.cx.update(|cx| {
                fixture
                    .view
                    .upgrade()
                    .map(|view| view.read(cx).is_streaming())
                    .unwrap_or(true)
            })
        },
        "the turn done after approval",
    );

    // Files restore: refused, nothing touched.
    fixture.act(|window, cx| window.click("rewind-0", cx));
    fixture.act(|window, cx| window.click("rewind-files", cx));
    assert_eq!(
        std::fs::read(workspace_path.join("scratch.txt")).expect("untouched"),
        b"auto",
        "the restore refused before touching anything"
    );
    // The refusal is visible, a notice turn joined the transcript,
    // but nothing else moved.
    assert_eq!(turn_count_of(&mut fixture), 3);

    // Keep-changes never consults the guard, it changes no files.
    fixture.act(|window, cx| window.click("rewind-keep", cx));
    assert_eq!(turn_count_of(&mut fixture), 0);
    assert_eq!(
        std::fs::read(workspace_path.join("scratch.txt")).expect("kept"),
        b"auto",
        "keep means keep"
    );
}

/// The workspace switch, end to end: turn 1 writes in the first
/// folder, the chat moves to the second, turn 2 writes there, and
/// rewinding turn 1 restores into the first, because the ledger
/// recorded where its turn ran. The tools follow the switch (the
/// second write landed in the new root), the history does not lie
/// about the old one.
#[test]
fn a_workspace_switch_moves_the_tools_and_the_ledger_restores_where_it_ran() {
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("edits.toml"),
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"write_file\"\n\
         arguments = '{\"path\": \"scratch.txt\", \"content\": \"one\"}'\n\n\
         [[steps]]\ntext = ''\n\n\
         [[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"write_file\"\n\
         arguments = '{\"path\": \"scratch.txt\", \"content\": \"two\"}'\n\n\
         [[steps]]\ntext = ''\n",
    )
    .expect("scenario file");
    let first = tempfile::tempdir().expect("first workspace");
    let second = tempfile::tempdir().expect("second workspace");
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
        vision: true,
    };
    let provider = Arc::new(
        gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).expect("replay provider"),
    );
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let first_path = first.path().to_path_buf();
    let second_path = second.path().to_path_buf();
    let mut fixture = fixture_built(
        store,
        Some(snapshots),
        Some(first_path.clone()),
        None,
        Some(provider),
        None,
        Vec::new(),
        None,
        None,
    );
    fixture.hold = vec![scenarios, first, second, snapshots_dir];

    // Accept edits up front: the writes run unasked, no approvals in
    // the way of the two turns. Chip opens the menu, row applies.
    fixture.act(|window, cx| window.click("mode-chip", cx));
    fixture.act(|window, cx| window.click("mode-menu-accept-edits", cx));
    let picked = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .mode_for_test()
    });
    assert_eq!(
        picked,
        gertie_core::PermissionMode::AcceptEdits,
        "the row click moved the mode (workspace-switch fixture)"
    );

    // Turn 1 writes into the first folder.
    fixture.act(|window, cx| {
        window.input("one", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 40);
    assert_eq!(
        std::fs::read(first_path.join("scratch.txt")).expect("turn 1 wrote"),
        b"one"
    );

    // The switch, the footer word's apply path, portal-free.
    let view = fixture.view.clone();
    fixture.act(|_window, cx| {
        view.upgrade().expect("view alive").update(cx, |view, cx| {
            view.switch_workspace_for_test(second_path.as_path(), cx);
        });
    });

    // Turn 2 writes into the second folder; the first is untouched.
    fixture.act(|window, cx| {
        window.input("two", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 40);
    assert_eq!(
        std::fs::read(second_path.join("scratch.txt")).expect("turn 2 wrote"),
        b"two",
        "the tools followed the switch"
    );
    assert_eq!(
        std::fs::read(first_path.join("scratch.txt")).expect("turn 1's file"),
        b"one",
        "the switch wrote nothing backward in time"
    );

    // Rewind turn 1, files included: rewind truncates both turns, and
    // each file restores where its own turn ran, turn 1's into the
    // first folder (the ledger's recorded workspace, though the caller
    // now stands in the second), turn 2's into the second.
    fixture.act(|window, cx| window.click("rewind-0", cx));
    fixture.act(|window, cx| window.click("rewind-files", cx));
    assert_eq!(
        std::fs::read(first_path.join("scratch.txt")).expect("restored"),
        b"",
        "the restore wrote where the turn ran"
    );
    assert_eq!(
        std::fs::read(second_path.join("scratch.txt")).expect("restored"),
        b"",
        "the second turn's undo followed its own recorded root"
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

/// Consecutive finished tool cards collapse into one summary row; the
/// reader expands it to the full cards and collapses it back.
#[test]
fn finished_tool_runs_collapse_into_one_summary() {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let session = store.create_session("run of tools").expect("session");
    let parent = store
        .append_message(session, None, gertie_core::Role::User, "run the checks")
        .expect("row");
    let parent = store
        .append_tool_result(
            session,
            Some(parent),
            "c1",
            "read_file",
            "{}",
            "read one\nline two",
        )
        .expect("row");
    let parent = store
        .append_tool_result(
            session,
            Some(parent),
            "c2",
            "read_file",
            "{}",
            "read two\nline two",
        )
        .expect("row");
    store
        .append_tool_result(
            session,
            Some(parent),
            "c3",
            "bash",
            "{}",
            "error: nope\nline two",
        )
        .expect("row");
    let mut fixture = fixture_with_store(store);

    // Collapsed: one summary, no member cards in sight.
    fixture.act(|window, _cx| {
        assert!(window.try_find("tool-run-1").is_some(), "the summary shows");
        assert!(
            window.try_find("tool-toggle-2").is_none(),
            "member cards hide inside the run"
        );
    });
    // Expanded: every card is back, failures included.
    fixture.act(|window, cx| window.click("tool-run-1", cx));
    fixture.act(|window, _cx| {
        assert!(window.try_find("tool-toggle-2").is_some(), "expanded");
        assert!(window.try_find("tool-toggle-3").is_some(), "expanded");
    });
    // Collapsed again.
    fixture.act(|window, cx| window.click("tool-run-1", cx));
    fixture.act(|window, _cx| {
        assert!(window.try_find("tool-toggle-2").is_none(), "hidden again");
    });
}

/// The status line names what a pending mutation waits on, the timer
/// next to "waiting on you" is honest in the opposite direction from a
/// timer next to "running". A silent scenario (no narration) falls
/// back to naming the finished tool once the call settles.
#[test]
fn a_pending_mutation_names_what_it_waits_on() {
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("edits.toml"),
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"read_file\"\n\
         arguments = '{\"path\": \"scratch.txt\"}'\n\n\
         [[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"write_file\"\n\
         arguments = '{\"path\": \"out.txt\", \"content\": \"x\"}'\n\n\
         [[steps]]\ntext = ''\n",
    )
    .expect("scenario file");
    let workspace = tempfile::tempdir().expect("workspace dir");
    let endpoint = gertie_providers::Endpoint {
        name: "replay".into(),
        base_url: scenarios.path().to_string_lossy().into_owned(),
        model: "edits".into(),
        api_key: None,
        context_window: None,
        wire: gertie_providers::Wire::Replay,
        vision: true,
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
        None,
    );
    fixture.hold = vec![scenarios, workspace];

    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 30);
    let pending_line = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .activity_line_for_test()
    });
    assert!(
        pending_line.starts_with("Waiting on you: Writing `out.txt`"),
        "the pending card names the ask: {pending_line}"
    );

    // Approved and settled: the status is the done label alone, the
    // model chewing on the result is the thinking row's business.
    fixture.act(|window, cx| window.click("approve-2", cx));
    pump(&mut fixture, 40);
    let settled_line = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .activity_line_for_test()
    });
    assert!(
        settled_line.starts_with("Wrote `out.txt`"),
        "the done label alone: {settled_line}"
    );
}

/// Narration never becomes the status: the model may say "Let me
/// check the config first." before a call, but once the result lands
/// the line is the done label and its step clock. The old behavior
/// quoted the model's prose (truncated at 64 chars); a monologue in
/// a status line is the model's words wearing the harness's voice.
#[test]
fn narration_stays_out_of_the_status_line() {
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("narrated.toml"),
        "[[steps]]\ntext = 'Let me check the config first.'\n\n\
         [[steps.tool_calls]]\nname = \"read_file\"\n\
         arguments = '{\"path\": \"scratch.txt\"}'\n\n\
         [[steps]]\ntext = ''\n",
    )
    .expect("scenario file");
    let workspace = tempfile::tempdir().expect("workspace dir");
    std::fs::write(workspace.path().join("scratch.txt"), "x").expect("scratch file");
    let endpoint = gertie_providers::Endpoint {
        name: "replay".into(),
        base_url: scenarios.path().to_string_lossy().into_owned(),
        model: "narrated".into(),
        api_key: None,
        context_window: None,
        wire: gertie_providers::Wire::Replay,
        vision: true,
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
        None,
    );
    fixture.hold = vec![scenarios, workspace];

    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 60);
    let line = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .activity_line_for_test()
    });
    assert!(
        line.starts_with("Read `scratch.txt` · "),
        "the done label with its step clock: {line}"
    );
    assert!(
        !line.contains("Let me check"),
        "the narration never rides the line: {line}"
    );
}

/// An ask card's status is the question itself: the card is the ask,
/// and the ask_user tool's label carries the model's question.
#[test]
fn the_ask_card_names_the_question_in_the_status() {
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("asks.toml"),
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"ask_user\"\n\
         arguments = '{\"question\": \"Which database?\", \"options\": \
                      [\"postgres\", \"sqlite\"], \"allow_custom\": false}'\n\n\
         [[steps]]\ntext = ''\n",
    )
    .expect("scenario file");
    let endpoint = gertie_providers::Endpoint {
        name: "replay".into(),
        base_url: scenarios.path().to_string_lossy().into_owned(),
        model: "asks".into(),
        api_key: None,
        context_window: None,
        wire: gertie_providers::Wire::Replay,
        vision: true,
    };
    let provider = Arc::new(
        gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).expect("replay provider"),
    );
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let mut fixture = fixture_built(
        store,
        None,
        None,
        None,
        Some(provider),
        None,
        vec![Arc::new(gertie_tools::AskUser)],
        None,
        None,
    );
    fixture.hold = vec![scenarios];

    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 30);
    let ask_line = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .activity_line_for_test()
    });
    assert!(
        ask_line.starts_with("Asking you \u{201c}Which database?"),
        "the status is the question: {ask_line}"
    );
    fixture.act(|window, _cx| {
        // The ask's own card stands in view, its option buttons with it.
        assert!(window.try_find("ask-1-0").is_some(), "options in view");
    });

    // The human clicks an option: the ask settles and the turn ends.
    fixture.act(|window, cx| window.click("ask-1-0", cx));
    pump(&mut fixture, 40);
    let answered_line = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .activity_line_for_test()
    });
    assert!(
        answered_line.starts_with("Asked you \u{201c}Which database?"),
        "the settled ask names itself: {answered_line}"
    );
}

/// Stop ends a running turn from the composer: with nothing typed the
/// send button is Stop; a click ends the turn, the waiting card closes
/// as stopped (persisted like any result), and the button is Send
/// again.
#[test]
fn stop_ends_a_running_turn_and_closes_its_open_card() {
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("asks.toml"),
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"ask_user\"\n\
         arguments = '{\"question\": \"Which database?\", \"options\": \
                      [\"postgres\", \"sqlite\"], \"allow_custom\": false}'\n\n\
         [[steps]]\ntext = ''\n",
    )
    .expect("scenario file");
    let endpoint = gertie_providers::Endpoint {
        name: "replay".into(),
        base_url: scenarios.path().to_string_lossy().into_owned(),
        model: "asks".into(),
        api_key: None,
        context_window: None,
        wire: gertie_providers::Wire::Replay,
        vision: true,
    };
    let provider = Arc::new(
        gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).expect("replay provider"),
    );
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let mut fixture = fixture_built(
        store,
        None,
        None,
        None,
        Some(provider),
        None,
        vec![Arc::new(gertie_tools::AskUser)],
        None,
        None,
    );
    fixture.hold = vec![scenarios];

    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 30);
    let streaming = |fixture: &mut Fixture| {
        fixture.cx.update(|cx| {
            fixture
                .view
                .upgrade()
                .expect("view alive")
                .read(cx)
                .is_streaming()
        })
    };
    assert!(streaming(&mut fixture), "the turn waits on the ask");
    fixture.act(|window, _cx| {
        assert!(
            window.try_find("ask-1-0").is_some(),
            "the ask's options show"
        );
        assert!(
            window.try_find("stop-turn").is_some(),
            "an empty composer offers Stop"
        );
    });

    fixture.act(|window, cx| window.click("stop-turn", cx));
    pump(&mut fixture, 30);
    assert!(!streaming(&mut fixture), "Stop ended the turn");
    fixture.act(|window, _cx| {
        assert!(window.try_find("send-message").is_some(), "Send is back");
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
        .expect("session");
    let rows = fixture.store.load_messages(session).expect("rows");
    assert!(
        rows.iter()
            .any(|row| row.content.contains("stopped by the user")),
        "the stopped card's result is stored: {:?}",
        rows.iter()
            .map(|row| row.content.clone())
            .collect::<Vec<_>>()
    );
}

/// A pending card breaks a run: finished reads before it stay
/// stand-alone (an approval hiding inside a collapsed summary would be
/// a trap); approving it lets the run close over both.
#[test]
fn a_pending_card_breaks_the_run_until_it_settles() {
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("edits.toml"),
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"read_file\"\n\
         arguments = '{\"path\": \"scratch.txt\"}'\n\n\
         [[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"write_file\"\n\
         arguments = '{\"path\": \"out.txt\", \"content\": \"x\"}'\n\n\
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
        vision: true,
    };
    let provider = Arc::new(
        gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).expect("replay provider"),
    );
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let mut fixture = fixture_built(
        store,
        Some(snapshots),
        Some(workspace.path().to_path_buf()),
        None,
        Some(provider),
        None,
        Vec::new(),
        None,
        None,
    );
    fixture.hold = vec![scenarios, workspace, snapshots_dir];

    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 30);
    fixture.act(|window, _cx| {
        assert!(
            window.try_find("tool-run-1").is_none(),
            "the pending write breaks the run, no summary over a live ask"
        );
        assert!(
            window.try_find("approve-2").is_some(),
            "the pending card stands alone, its approval in view"
        );
    });

    // Approve: the write settles, the run of finished cards closes
    // over both.
    fixture.act(|window, cx| window.click("approve-2", cx));
    pump(&mut fixture, 40);
    fixture.act(|window, _cx| {
        assert!(
            window.try_find("tool-run-1").is_some(),
            "both finished, the run collapses"
        );
    });
}

/// `>` switches the palette to commands: the term matches names and
/// descriptions, Enter runs the action, and the panel opens.
#[test]
fn the_palette_gt_filter_runs_a_command() {
    let mut fixture = fixture();

    fixture.act(|window, cx| window.press("ctrl-k", cx));
    fixture.act(|window, cx| window.input(">", cx));
    assert_eq!(
        fixture
            .cx
            .update(|cx| fixture
                .view
                .upgrade()
                .expect("view alive")
                .read(cx)
                .palette_command_names_for_test())
            .len(),
        6,
        "a bare > lists every command"
    );

    // Fresh palette: typing appends, so the next query starts from a
    // cleared field (escape closes, ctrl-k reopens it empty).
    fixture.act(|window, cx| window.press("escape", cx));
    fixture.act(|window, cx| window.press("ctrl-k", cx));
    fixture.act(|window, cx| window.input(">settings", cx));
    assert_eq!(
        fixture.cx.update(|cx| fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .palette_command_names_for_test()),
        vec!["settings"]
    );
    fixture.act(|window, cx| window.press("enter", cx));
    let state = fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive").read(cx);
        (
            view.palette_open_for_test(),
            view.palette_command_names_for_test(),
            view.panel_for_test(),
        )
    });
    assert!(
        !state.0,
        "palette should close on confirm (commands={:?}, panel={})",
        state.1, state.2
    );
    // The action ran: the right column flipped to the settings panel.
    assert_eq!(state.2, "settings", "the settings panel opened");
}

/// F6 cycles the surface's areas forward (wrapping), Shift+F6 back;
/// the right panel joins the cycle only while it's open.
#[test]
fn f6_cycles_focus_areas_and_shift_f6_walks_back() {
    let mut fixture = fixture();

    // The composer holds focus on open.
    assert_eq!(focused_area(&mut fixture), "composer");

    // Forward: sidebar → transcript → composer → wrap.
    fixture.act(|window, cx| window.press("f6", cx));
    assert_eq!(focused_area(&mut fixture), "sidebar");
    fixture.act(|window, cx| window.press("f6", cx));
    assert_eq!(focused_area(&mut fixture), "transcript");
    fixture.act(|window, cx| window.press("f6", cx));
    assert_eq!(focused_area(&mut fixture), "composer");
    fixture.act(|window, cx| window.press("f6", cx));
    assert_eq!(focused_area(&mut fixture), "sidebar", "the cycle wraps");

    // Backward: from the sidebar, Shift+F6 lands on the last area
    // (composer), then walks back to the transcript.
    fixture.act(|window, cx| window.press("shift-f6", cx));
    assert_eq!(focused_area(&mut fixture), "composer");
    fixture.act(|window, cx| window.press("shift-f6", cx));
    assert_eq!(focused_area(&mut fixture), "transcript");

    // The right panel joins the cycle while it's open. Open it via
    // the palette: the page takes focus on open (its Escape rule
    // binds), F6 walks on from there, and the wrap crosses the
    // panel too.
    fixture.act(|window, cx| window.press("ctrl-k", cx));
    fixture.act(|window, cx| window.input(">settings", cx));
    fixture.act(|window, cx| window.press("enter", cx));
    assert_eq!(focused_area(&mut fixture), "panel");
    fixture.act(|window, cx| window.press("f6", cx));
    assert_eq!(
        focused_area(&mut fixture),
        "sidebar",
        "the cycle wraps off the panel"
    );
    fixture.act(|window, cx| window.press("shift-f6", cx));
    assert_eq!(
        focused_area(&mut fixture),
        "panel",
        "shift-f6 walks back onto the panel"
    );
}

fn focused_area(fixture: &mut Fixture) -> String {
    fixture
        .cx
        .update_window(fixture.window.into(), |_, window, cx| {
            fixture
                .view
                .upgrade()
                .expect("view alive")
                .read(cx)
                .focused_area_for_test(window, cx)
                .to_string()
        })
        .expect("fixture window")
}

/// The MRU boost: the thread you last picked for a query ranks first
/// when you type that query again. The selection moves through the
/// palette state's own API, what the hover and arrow paths converge
/// on; the input context claims the arrow keys while the query has
/// text, so the keystroke path isn't testable.
#[test]
fn a_picked_session_ranks_first_when_the_query_returns() {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    // Two flaky rows with different match densities, so the FTS rank,
    // and therefore the unpicked order, is deterministic: alpha's
    // denser row outranks beta's.
    let alpha = store.create_session("alpha flaky").expect("alpha");
    store
        .append_message(alpha, None, gertie_core::Role::User, "flaky flaky clue one")
        .expect("row");
    let beta = store.create_session("beta flaky").expect("beta");
    store
        .append_message(beta, None, gertie_core::Role::User, "flaky clue two")
        .expect("row");
    let mut fixture = fixture_with_store(store);

    fixture.act(|window, cx| window.press("ctrl-k", cx));
    fixture.act(|window, cx| window.input("flaky", cx));
    let before = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .palette_hit_sessions_for_test()
    });
    assert_eq!(before.len(), 2, "both sessions hold a flaky row");
    assert_eq!(before[0], alpha, "the denser row ranks first, unpicked");

    // Select beta's row and confirm it, the pick ledger records it.
    let palette = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .palette_entity_for_test()
    });
    fixture.act(move |window, cx| {
        palette.update(cx, |palette, cx| {
            palette.set_selected_index(
                Some(gpui_kit::component::IndexPath::new(1).section(0)),
                window,
                cx,
            );
        });
        window.press("enter", cx);
    });
    let landed = fixture
        .cx
        .update(|cx| {
            fixture
                .view
                .upgrade()
                .expect("view alive")
                .read(cx)
                .session_id()
        })
        .expect("landed");
    assert_eq!(landed, beta, "the confirm switched to the picked hit");

    fixture.act(|window, cx| window.press("ctrl-k", cx));
    fixture.act(|window, cx| window.input("flaky", cx));
    let after = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .palette_hit_sessions_for_test()
    });
    let ledger = fixture.store.palette_boosts("flaky");
    assert_eq!(ledger, vec![beta], "the pick landed in the ledger");
    assert_eq!(after.len(), before.len(), "nothing was lost");
    assert_eq!(
        after[0], beta,
        "the picked session ranks first on the repeated query"
    );
}

#[test]
fn a_worktree_tabs_first_message_names_its_branch() {
    // A real repo, a real worktree under it, the chat bound to the
    // worktree with naming on: the send that creates the session also
    // names the branch.
    let repo = tempfile::tempdir().expect("repo dir");
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .output()
            .expect("git runs")
    };
    run(&["init"]);
    std::fs::write(repo.path().join("f.txt"), "x").unwrap();
    run(&["add", "-A"]);
    run(&[
        "-c",
        "user.email=g@t",
        "-c",
        "user.name=g",
        "commit",
        "-m",
        "init",
    ]);

    let root = gertie::worktree::create(repo.path(), "tab-0").expect("worktree created");
    let naming = gertie::worktree::BranchNaming {
        root: root.clone(),
        prefix: "gertie/".into(),
    };
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let mut fixture = fixture_full(
        store,
        None,
        Some(root.clone()),
        None,
        None,
        None,
        Vec::new(),
        Some(naming),
        None,
        None,
    );
    fixture.hold.push(repo);

    fixture.act(|window, cx| {
        window.input("Fix the flaky widget test", cx);
        window.press("enter", cx);
    });

    // The session exists (the send ran end to end) and the branch was
    // renamed from tab-0 to the first message's slug under the prefix.
    let _ = fixture
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
    let branch = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .expect("git runs");
    let branch = String::from_utf8_lossy(&branch.stdout);
    assert!(
        branch.contains("gertie/fix-the-flaky-widget-test"),
        "the branch is the first message's slug under the prefix, got: {branch}"
    );
}

/// The routing rules against a real WorkspaceView: two sessions in
/// two workspaces, a two-tab shell, and the decisions the single-tab
/// fixture cannot reach. The shell is real (WorkspaceView::new over a
/// real Bootstrap), so the routing runs the production code, not a
/// re-implementation.
#[test]
fn routing_sends_a_session_to_the_tab_that_owns_its_workspace() {
    // A temp config: Bootstrap::load writes no skeleton for an
    // explicit path, and an empty file parses to the defaults.
    let config_dir = tempfile::tempdir().expect("config dir");
    let config_path = config_dir.path().join("config.toml");
    std::fs::write(&config_path, "").expect("empty config");
    let boot = Arc::new(gertie::bootstrap::Bootstrap::load(
        Some(&config_path),
        false,
    ));

    // The roots must EXIST: workspace_for_root canonicalizes, and a
    // nonexistent folder is a switch refusal by design.
    std::fs::create_dir_all("/tmp/routing-gertie").expect("root dir");
    std::fs::create_dir_all("/tmp/routing-kuma").expect("root dir");

    // Two sessions, two roots, both outside the boot's own root so
    // every click is a cross-root decision.
    let s_gertie = boot.store.create_session("gertie work").expect("session");
    boot.store
        .append_message(s_gertie, None, gertie_core::Role::User, "hello")
        .expect("row");
    boot.store
        .set_session_workspace(s_gertie, Path::new("/tmp/routing-gertie"));
    let s_kuma = boot.store.create_session("kuma work").expect("session");
    boot.store
        .append_message(s_kuma, None, gertie_core::Role::User, "hello")
        .expect("row");
    boot.store
        .set_session_workspace(s_kuma, Path::new("/tmp/routing-kuma"));

    let mut cx = HeadlessAppContext::with_asset_source(
        Arc::new(NoopTextSystem),
        Arc::new(gpui_kit::assets::Assets),
    );
    cx.update(|cx| {
        gpui_kit::init(cx);
        gertie::chat::bind_chat_keys(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
    });
    let shell: std::sync::Mutex<Option<gpui_kit::WeakEntity<gertie::tabs::WorkspaceView>>> =
        std::sync::Mutex::new(None);
    let window = cx
        .open_window(size(px(1200.), px(800.)), |window, cx| {
            let view = cx.new(|cx| gertie::tabs::WorkspaceView::new(boot.clone(), window, cx));
            *shell.lock().unwrap() = Some(view.downgrade());
            cx.new(|cx| Root::new(view, window, cx))
        })
        .expect("window");
    // Give the shell a moment: the first tab's sidebar carries both
    // seeded sessions.
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
    })
    .expect("draw");

    // Helper: read the shell's (tab count, active chat root).
    let probe = |cx: &mut HeadlessAppContext,
                 shell: &gpui_kit::WeakEntity<gertie::tabs::WorkspaceView>| {
        cx.update(|cx| {
            let shell = shell.upgrade().expect("shell alive");
            shell.read(cx).tab_probe(cx)
        })
    };

    // ---- Rule 2: a tab that holds the root loads the click.
    // State: tab 1 (the launch tab) holds /tmp/routing-gertie (boot
    // root); tab 2 exists at /tmp/routing-kuma. Clicking the kuma
    // session while tab 1 is active must load it in tab 2, not move
    // tab 1.
    {
        let shell = shell.lock().unwrap().clone().expect("shell");
        // Tab 2 at the kuma root: make_view is private, so the test
        // reaches it through the shell's own add path... which roots
        // at boot.workspace_root. So instead: switch tab 1 to kuma
        // first (rule 3, pristine adopt), add a second tab, and have
        // tab 2 adopt gertie — then click the gertie session from
        // tab 2's sidebar: rule 2 must land it in tab 1.
        cx.update_window(window.into(), |_, window, cx| {
            let shell_entity = shell.upgrade().expect("shell");
            // The direct call stands in for the listener: the shell
            // decides, then the clicked view applies its instruction.
            shell_entity.update(cx, |shell, cx| {
                // Tab 1 is pristine: clicking the kuma session adopts
                // the kuma root into tab 1 (rule 3).
                let decision = shell.route_session(
                    s_kuma,
                    Some("/tmp/routing-kuma".to_string()),
                    gertie::chat::ClickOrigin {
                        root: boot.workspace_root.clone(),
                        session: None,
                        streaming: false,
                    },
                    window,
                    cx,
                );
                assert!(
                    matches!(decision, gertie::chat::SessionRouting::Adopt(_)),
                    "rule 3 hands the clicked view the adoption"
                );
                let (_, view, _) = shell.tab_probe(cx);
                view.update(cx, |view, cx| {
                    view.apply_routing(s_kuma, decision, window, cx)
                });
            });
        })
        .expect("route kuma");
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw");
        let (tabs, view, root) = probe(&mut cx, &shell);
        assert_eq!(tabs, 1, "rule 3: the idle view adopts, no new view");
        assert_eq!(
            root,
            Path::new("/tmp/routing-kuma").to_path_buf(),
            "the tab follows the session's workspace"
        );
        let _ = view;
    }

    // ---- Rule 4, then rule 0, in one update: a frame (which the
    // update's end may draw) drops idle off-screen views, and view 1
    // only claims to run.
    {
        let shell = shell.lock().unwrap().clone().expect("shell");
        cx.update_window(window.into(), |_, window, cx| {
            let shell_entity = shell.upgrade().expect("shell");
            shell_entity.update(cx, |shell, cx| {
                // View 1 holds the kuma session and (as far as the
                // shell is told) runs a turn: the gertie click opens
                // a new view rooted at gertie's workspace.
                let decision = shell.route_session(
                    s_gertie,
                    Some("/tmp/routing-gertie".to_string()),
                    gertie::chat::ClickOrigin {
                        root: Path::new("/tmp/routing-kuma").to_path_buf(),
                        session: Some(s_kuma),
                        streaming: true,
                    },
                    window,
                    cx,
                );
                assert!(
                    matches!(decision, gertie::chat::SessionRouting::Elsewhere),
                    "rule 4 loads the session in the new view"
                );
                let (tabs, _, root) = shell.tab_probe(cx);
                assert_eq!(tabs, 2, "rule 4: the running view stays, a new view opens");
                assert_eq!(
                    root,
                    Path::new("/tmp/routing-gertie").to_path_buf(),
                    "the new view runs at the session's workspace"
                );

                // Rule 0: the kuma click from the gertie view shows
                // view 1, which has it open; no third view.
                let decision = shell.route_session(
                    s_kuma,
                    Some("/tmp/routing-kuma".to_string()),
                    gertie::chat::ClickOrigin {
                        root: Path::new("/tmp/routing-gertie").to_path_buf(),
                        session: Some(s_gertie),
                        streaming: false,
                    },
                    window,
                    cx,
                );
                assert!(
                    matches!(decision, gertie::chat::SessionRouting::Elsewhere),
                    "rule 0 shows the view that has it open"
                );
                let (tabs, _, root) = shell.tab_probe(cx);
                assert_eq!(tabs, 2, "rule 0: the holder shows, no new view");
                assert_eq!(
                    root,
                    Path::new("/tmp/routing-kuma").to_path_buf(),
                    "the holder is active after the click"
                );
            });
        })
        .expect("route");
        // A frame drops the idle, off-screen gertie view: the
        // conversation stays in the store and the sidebar.
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("draw");
        let (tabs, _view, _root) = probe(&mut cx, &shell);
        assert_eq!(tabs, 1, "idle off-screen views are dropped");
    }
}

/// The routing tests above call `route_session` straight from the test,
/// the shell leased alone. A real click is not that: the click leases
/// the clicked ChatView first, and only then does the router reach the
/// shell, which routes straight back into that same view (rules 1 and
/// 3 both load into the active tab). A re-entrant lease is a gpui
/// panic, and a panic under a click listener takes the app down. This
/// test clicks for real: a same-root session (rule 1, the everyday
/// sidebar click) must load into the active tab and the window must
/// survive it.
#[test]
fn a_real_sidebar_click_loads_a_same_root_session() {
    let config_dir = tempfile::tempdir().expect("config dir");
    let config_path = config_dir.path().join("config.toml");
    std::fs::write(&config_path, "").expect("empty config");
    let boot = Arc::new(gertie::bootstrap::Bootstrap::load(
        Some(&config_path),
        false,
    ));

    // One session, rooted at the boot's own workspace: the click is
    // rule 1, load here. This is the click every single-project user
    // makes on every session they own.
    let session = boot.store.create_session("same root").expect("session");
    boot.store
        .append_message(session, None, gertie_core::Role::User, "hello")
        .expect("row");
    boot.store
        .set_session_workspace(session, &boot.workspace_root);

    let mut cx = HeadlessAppContext::with_asset_source(
        Arc::new(NoopTextSystem),
        Arc::new(gpui_kit::assets::Assets),
    );
    cx.update(|cx| {
        gpui_kit::init(cx);
        gertie::chat::bind_chat_keys(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
    });
    let shell: std::sync::Mutex<Option<gpui_kit::WeakEntity<gertie::tabs::WorkspaceView>>> =
        std::sync::Mutex::new(None);
    let window = cx
        .open_window(size(px(1200.), px(800.)), |window, cx| {
            let view = cx.new(|cx| gertie::tabs::WorkspaceView::new(boot.clone(), window, cx));
            *shell.lock().unwrap() = Some(view.downgrade());
            cx.new(|cx| Root::new(view, window, cx))
        })
        .expect("window");
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
    })
    .expect("draw");

    // The click itself, the production path: dispatch leases the
    // clicked view, the listener hands the shell the route, the shell
    // routes back. Before the fix this panics under the click.
    let shell = shell.lock().unwrap().clone().expect("shell");
    cx.update_window(window.into(), |_, window, cx| {
        let button = SharedString::from(format!("session-{}", session.0));
        window.click(button, cx);
    })
    .expect("the click must not panic");
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
    })
    .expect("draw after the click");

    // Green-capable signal: the session actually loaded here, one tab.
    let shell = shell.upgrade().expect("shell alive");
    let (tabs, view, _root) = cx.update(|cx| shell.read(cx).tab_probe(cx));
    let loaded = cx.update(|cx| view.read(cx).session_id());
    assert_eq!(loaded, Some(session), "the clicked session loads");
    assert_eq!(tabs, 1, "rule 1: the same-root click never opens a tab");
}

/// A replay provider over one scenario file, the scripted-turn helper
/// the status column tests share.
fn replay_provider(scenario: &str) -> (Arc<gertie_providers::AnyProvider>, tempfile::TempDir) {
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(scenarios.path().join("status.toml"), scenario).expect("scenario file");
    let endpoint = gertie_providers::Endpoint {
        name: "replay".into(),
        base_url: scenarios.path().to_string_lossy().into_owned(),
        model: "status".into(),
        api_key: None,
        context_window: None,
        wire: gertie_providers::Wire::Replay,
        vision: true,
    };
    let provider = Arc::new(
        gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).expect("replay provider"),
    );
    (provider, scenarios)
}

/// The status column (issue #33): a scripted turn reads a file, states
/// its goal and next step, and consults memory. The column shows the
/// goal, the touched file, the next step, and the consulted entry with
/// its moved evidence; the riding line names the entry and, the
/// evidence having moved, carries the flag. Folding the column leaves
/// the rail and the header chip, and the fold is stored per
/// conversation.
#[test]
fn the_status_column_shows_the_session_state_and_folds_to_a_rail() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::write(dir.path().join("teardown.rs"), "fn teardown() {}\n").expect("cited file");
    std::fs::create_dir_all(dir.path().join("project")).expect("tier root");
    std::fs::write(
        dir.path().join("project").join("gotchas.md"),
        "- [p12] helpful=0 harmful=0 cite the flake before blaming the test \
         [evidence: teardown.rs@000000000000]\n",
    )
    .expect("topic file");
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("session store"));
    let memory = Arc::new(gertie_core::MemoryStore::open(
        dir.path().join("project"),
        dir.path().join("global"),
        store.clone(),
    ));
    memory.sync().expect("sidecar sync");
    let (provider, scenarios) = replay_provider(
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"status\"\n\
         arguments = '{\"goal\": \"Fix the teardown race\", \"next\": \"re-run the suite\"}'\n\n\
         [[steps.tool_calls]]\nname = \"search_memory\"\n\
         arguments = '{\"query\": \"flake\"}'\n\n\
         [[steps.tool_calls]]\nname = \"read_file\"\n\
         arguments = '{\"path\": \"teardown.rs\"}'\n\n\
         [[steps]]\ntext = 'done'\n",
    );
    let mut fixture = fixture_built(
        store.clone(),
        None,
        Some(dir.path().to_path_buf()),
        None,
        Some(provider),
        None,
        vec![
            Arc::new(gertie_tools::Status),
            Arc::new(gertie_tools::memory::SearchMemory::new(memory.clone())),
        ],
        Some(memory.clone()),
        None,
    );
    fixture.hold = vec![scenarios, dir];

    fixture.act(|window, _| {
        assert!(
            window.try_find("status-column").is_none(),
            "a fresh chat has nothing to show"
        );
    });
    fixture.act(|window, cx| {
        window.input("The widget_test keeps flaking", cx);
        window.press("enter", cx);
    });
    pump_until(
        &mut fixture,
        200,
        |fixture| {
            fixture.cx.update(|cx| {
                !fixture
                    .view
                    .upgrade()
                    .expect("view alive")
                    .read(cx)
                    .is_streaming()
            })
        },
        "the scripted turn",
    );
    let (goal, next, touched, ground) = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .status_for_test()
    });
    assert_eq!(goal.as_deref(), Some("Fix the teardown race"));
    assert_eq!(next.as_deref(), Some("re-run the suite"));
    assert_eq!(touched, vec![("teardown.rs".to_string(), "read")]);
    assert_eq!(ground, vec!["p12".to_string()]);
    let riding = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .riding_for_test()
    });
    let (id, flag) = riding.expect("the riding line names the consulted entry");
    assert_eq!(id, "p12");
    // The reread of the cited file IS the re-check: the fold re-recorded
    // the hash at that moment, so the flag cleared itself (no click, no
    // tool call) and the riding line carries no correction.
    assert!(flag.is_none(), "the reread re-verified the entry: {flag:?}");
    let entry = memory
        .scan()
        .into_iter()
        .find(|entry| entry.id == "p12")
        .expect("the entry");
    assert!(
        memory.changed_evidence(&entry).is_empty(),
        "the stored hash is the file's current state"
    );
    // The sizing spike, kept as a guard: a permanent right column in
    // the flex row must take its width and stay inside the window, the
    // transcript yielding, not overflowing (the memory panel's
    // zero-extent lesson came from this same row).
    fixture.act(|window, _| {
        let column = window
            .try_find("status-column")
            .map(|found| found.bounds())
            .expect("the column is laid out");
        eprintln!("status column bounds: {column:?}");
        assert!(
            column.size.width > px(299.) && column.size.width < px(301.),
            "the column keeps its width: {column:?}"
        );
        assert!(
            column.right() <= px(900.5),
            "the column stays inside the window: {column:?}"
        );
        assert!(
            column.size.height > px(100.),
            "the column has height: {column:?}"
        );
    });
    fixture.act(|window, _| {
        assert!(
            window.try_find("status-column").is_some(),
            "the column shows"
        );
        assert!(
            window.try_find("status-ground-p12").is_some(),
            "DEPENDS ON lists the entry"
        );
    });

    // Fold: the rail and the header chip replace the column, stored.
    fixture.act(|window, cx| window.click("status-collapse", cx));
    fixture.act(|window, _| {
        assert!(window.try_find("status-column").is_none(), "folded");
        assert!(window.try_find("status-rail").is_some(), "the rail stays");
        assert!(
            window.try_find("status-rail-state").is_some(),
            "the rail's state icon"
        );
        assert!(window.try_find("status-chip").is_some(), "the header chip");
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
        .expect("session");
    assert!(
        store.store_flag(&format!("status_collapsed:{}", session.0)),
        "the fold is stored for this conversation"
    );
    fixture.act(|window, cx| window.click("status-chip", cx));
    fixture.act(|window, _| {
        assert!(window.try_find("status-column").is_some(), "unfolded");
    });
    assert!(!store.store_flag(&format!("status_collapsed:{}", session.0)));
}

/// The GROUNDED IN viewport shows three whole rows at their real
/// heights: rows free-wrap, so the viewport's max is measured from the
/// rows themselves (three measured heights plus their gaps), not a
/// constant. Everything past the third row scrolls.
#[test]
fn the_grounded_in_viewport_shows_three_whole_rows() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(dir.path().join("project")).expect("tier root");
    // Six entries sharing the query word, each long enough to wrap
    // well past one line in the 300px column: with every row tall, the
    // assertions cannot depend on which entry ranks first in the FTS
    // result. No [evidence:] clauses: the cluster is then just the
    // entry, so three of them (3 x 149 + gaps = 453px) fit the space
    // this window's column has, and the size assertions exercise the
    // three-row rule rather than the crowded-column bound.
    let mut entries = String::new();
    for n in 12..=17 {
        entries.push_str(&format!(
            "- [p{n}] helpful=0 harmful=0 the flake test keeps failing when the \
             teardown timer races the widget between retries and the retry \
             budget is spent before the fixture settles\n"
        ));
    }
    std::fs::write(dir.path().join("project").join("gotchas.md"), entries).expect("topic file");
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("session store"));
    let memory = Arc::new(gertie_core::MemoryStore::open(
        dir.path().join("project"),
        dir.path().join("global"),
        store.clone(),
    ));
    memory.sync().expect("sidecar sync");
    let (provider, scenarios) = replay_provider(
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"search_memory\"\n\
         arguments = '{\"query\": \"flake\"}'\n\n\
         [[steps]]\ntext = 'done'\n",
    );
    let mut fixture = fixture_built(
        store.clone(),
        None,
        Some(dir.path().to_path_buf()),
        None,
        Some(provider),
        None,
        vec![Arc::new(gertie_tools::memory::SearchMemory::new(
            memory.clone(),
        ))],
        Some(memory),
        None,
    );
    fixture.hold = vec![scenarios, dir];
    fixture.act(|window, cx| {
        window.input("The widget_test keeps flaking", cx);
        window.press("enter", cx);
    });
    pump_until(
        &mut fixture,
        200,
        |fixture| {
            fixture.cx.update(|cx| {
                !fixture
                    .view
                    .upgrade()
                    .expect("view alive")
                    .read(cx)
                    .is_streaming()
            })
        },
        "the scripted turn",
    );
    // The rows in render order, so the third and fourth can be named
    // without depending on the search's rank order.
    let ground = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .status_for_test()
            .3
    });
    assert!(
        ground.len() >= 4,
        "the scenario seeds at least four rows to size against: {ground:?}"
    );
    // The settle after the turn already drew the corrective frame the
    // measurement defers: read the settled bounds.
    fixture.act(|window, _| {
        let viewport = window
            .try_find("grounded-scroll")
            .map(|found| found.bounds())
            .expect("the grounded viewport is laid out");
        let column = window
            .try_find("status-column")
            .map(|found| found.bounds())
            .expect("the column is laid out");
        eprintln!("viewport: {viewport:?} column: {column:?}");
        // The outer bound holds: the viewport stays inside the
        // column's visible bottom (the window edge), whatever the
        // three-row math wanted.
        assert!(
            viewport.bottom() <= column.bottom() + px(1.),
            "the viewport never exceeds the column: {viewport:?} vs {column:?}"
        );
        let row = |id: &str| {
            window
                .try_find(format!("status-ground-{id}"))
                .map(|found| found.bounds())
                .unwrap_or_else(|| panic!("{id} is laid out"))
        };
        let third = row(&ground[2]);
        let fourth = row(&ground[3]);
        eprintln!("viewport: {viewport:?} third: {third:?} fourth: {fourth:?}");
        assert!(
            third.bottom() <= viewport.bottom() + px(1.),
            "the third row shows whole: {third:?} vs {viewport:?}"
        );
        assert!(
            fourth.top() >= viewport.bottom() - px(1.),
            "the fourth row starts below the fold: {fourth:?} vs {viewport:?}"
        );
    });
}

/// An ask answered from another chat's status column lands on the
/// owning card through the card's own path: the tool result is the
/// option, the turn completes, and the card says where the answer came
/// from. A permission ask cannot be answered that way.
#[test]
fn a_question_answered_from_the_column_lands_on_its_own_card() {
    let (provider, scenarios) = replay_provider(
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"ask_user\"\n\
         arguments = '{\"question\": \"Which database?\", \"options\": \
                      [\"postgres\", \"sqlite\"], \"allow_custom\": false}'\n\n\
         [[steps]]\ntext = ''\n",
    );
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("memory store"));
    let mut fixture = fixture_built(
        store.clone(),
        None,
        None,
        None,
        Some(provider),
        None,
        vec![Arc::new(gertie_tools::AskUser)],
        None,
        None,
    );
    fixture.hold = vec![scenarios];
    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 30);
    let asks = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .pending_asks()
    });
    assert_eq!(
        asks.len(),
        1,
        "the pending question is offered to the project"
    );
    assert_eq!(asks[0].question, "Which database?");
    assert_eq!(asks[0].options, vec!["postgres", "sqlite"]);
    let ask = asks[0].ask;

    let applied = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .update(cx, |chat, cx| {
                chat.answer_from_column(ask, "sqlite".into(), cx)
            })
    });
    assert!(applied, "the owning card took the answer");
    pump(&mut fixture, 40);
    let again = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .update(cx, |chat, cx| {
                chat.answer_from_column(ask, "postgres".into(), cx)
            })
    });
    assert!(!again, "an answered ask takes no second answer");
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
        .expect("session");
    let rows = store.load_messages(session).expect("rows");
    assert!(
        rows.iter()
            .any(|row| row.tool_name.as_deref() == Some("ask_user") && row.content == "sqlite"),
        "the record is the one a click on the card makes: {:?}",
        rows.iter()
            .map(|row| row.content.clone())
            .collect::<Vec<_>>()
    );
    let provenance = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .answered_from_column_for_test()
    });
    assert_eq!(provenance, 1, "the card names where its answer came from");
}

/// The live session's REAL recorded commands (from the session DB):
/// multi-line heredocs whose interior python lines look like paths.
/// Today's gates must kill every fragment.
#[test]
fn the_live_sessions_real_commands_stay_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::write(root.join("real.md"), "x").unwrap();
    let mut facts = gertie::chat::status::StatusFacts::default();
    // Verbatim from the store: a heredoc whose python body contains
    // text.replace( tokens, and a grep over a log containing FAILED.
    let heredoc = r#"cd /work && python3 << 'PYEOF'
path = "gertie/src/chat/status.rs"
text = open(path).read()
text = text.replace(old, new)
old2 = "FAILED\."
print("anchored")
PYEOF"#;
    let grep = r#"grep -E "FAILED\." /tmp/s16.log"#;
    for command in [heredoc, grep] {
        facts.fold_tool(
            "bash",
            &serde_json::json!({"command": command}).to_string(),
            "ok",
            &root,
        );
    }
    for (path, _) in &facts.touched {
        assert!(
            path.contains("real.md") || path.contains("/tmp/") || path.contains(".md"),
            "heredoc fragments survived: {path:?}"
        );
    }
}

/// The file-tool arm gates too: a model-named path that is not a
/// real file renders no row (write_file excepted: it creates).
#[test]
fn the_file_tool_arm_gates_model_named_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::write(root.join("real.md"), "x").unwrap();
    let mut facts = gertie::chat::status::StatusFacts::default();
    facts.fold_tool("read_file", r#"{"path":"FAILED\."}"#, "ok", &root);
    facts.fold_tool(
        "edit_file",
        r#"{"path":"text.replace(anchor,"}"#,
        "ok",
        &root,
    );
    facts.fold_tool("read_file", r#"{"path":"real.md"}"#, "ok", &root);
    let paths: Vec<&String> = facts.touched.iter().map(|(path, _)| path).collect();
    assert_eq!(paths.len(), 1, "only the real file lands: {paths:?}");
    assert!(paths[0].contains("real.md"));
}

/// The hand test's garbage rows (`FAILED\.`, `text.replace(anchor,`):
/// reproducing the fold against the exact command shapes that
/// produced them. The gate must kill every one.
#[test]
fn the_gate_kills_the_hand_tests_garbage() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::write(root.join("real.md"), "x").unwrap();
    let mut facts = gertie::chat::status::StatusFacts::default();
    // The real commands from the live session (the grep that ran with
    // a quoted pattern; the python heredoc line).
    for command in [
        r#"grep -E "FAILED\." /tmp/s16.log"#,
        r#"grep -E "text.replace(anchor," gertie/src/chat.rs"#,
        r#"grep "FAILED\.\|panicked" /tmp/rc3.log"#,
        r#"python3 -c "text.replace(anchor, new)""#,
    ] {
        facts.fold_tool(
            "bash",
            &serde_json::json!({"command": command}).to_string(),
            "ok",
            &root,
        );
    }
    for (path, _) in &facts.touched {
        assert!(
            path.contains("real.md") || path.contains("/tmp/"),
            "garbage survived the gate: {path:?}"
        );
    }
}

/// Recall on touch (#36): a model that never searches memory still
/// gets the entries citing the file it reads, in that read's result,
/// and the column counts them as consulted, so GROUNDED IN and the
/// riding card fire with any model.
#[test]
fn reading_a_cited_file_recalls_its_memory_without_a_search() {
    let dir = tempfile::tempdir().expect("workspace");
    let root = dir.path().canonicalize().expect("root");
    std::fs::write(root.join("teardown.rs"), "fn teardown() {}\n").expect("cited file");
    std::fs::create_dir_all(root.join(".gertie/memory")).expect("tier root");
    let hash = gertie_core::blob_hash(b"fn teardown() {}\n");
    std::fs::write(
        root.join(".gertie/memory/gotchas.md"),
        format!(
            "- [p12] helpful=0 harmful=0 cite the flake before blaming the test \
             [evidence: teardown.rs@{hash}] [source: added: 2026-11-01]\n"
        ),
    )
    .expect("topic file");
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("session store"));
    let memory = Arc::new(gertie_core::MemoryStore::open(
        root.join(".gertie/memory"),
        root.join("global"),
        store.clone(),
    ));
    memory.sync().expect("sidecar sync");
    let (provider, scenarios) = replay_provider(
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"read_file\"\n\
         arguments = '{\"path\": \"teardown.rs\"}'\n\n\
         [[steps]]\ntext = 'done'\n",
    );
    let mut fixture = fixture_built(
        store.clone(),
        None,
        Some(root.clone()),
        None,
        Some(provider),
        None,
        vec![],
        Some(memory),
        None,
    );
    fixture.hold = vec![scenarios, dir];
    fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .update(cx, |view, _| view.enable_memory_for_test());
    });
    fixture.act(|window, cx| {
        window.input("why does the widget test flake", cx);
        window.press("enter", cx);
    });
    let view = fixture.view.clone();
    pump_until(
        &mut fixture,
        200,
        |fixture| {
            fixture
                .cx
                .update(|cx| !view.upgrade().expect("view alive").read(cx).is_streaming())
        },
        "the scripted turn",
    );
    let (ground, riding) = fixture.cx.update(|cx| {
        let view = view.upgrade().expect("view alive").read(cx);
        (view.status_for_test().3, view.riding_for_test())
    });
    assert_eq!(ground, vec!["p12".to_string()], "recalled is consulted");
    assert_eq!(riding.map(|(id, _)| id).as_deref(), Some("p12"));
    let session = fixture
        .cx
        .update(|cx| view.upgrade().expect("view alive").read(cx).session_id())
        .expect("session");
    let rows = store.load_messages(session).expect("rows");
    let read = rows
        .iter()
        .find(|row| row.tool_name.as_deref() == Some("read_file"))
        .expect("the read row");
    assert!(
        read.content
            .contains("[memory: entries citing teardown.rs]"),
        "the recall rides the read's result: {}",
        read.content
    );
}

/// The riding card (#33 decision 1, #34): one line, verb + why, on
/// the transcript's latest row while the run lives. The why is the
/// memory entry the work consulted; its moved evidence puts the full
/// form (the flag line) in play; and being news no running card
/// carries, it keeps the card on screen past the card-is-the-now
/// yield. Goal and next stay the column's. The run's end takes the
/// card away, so it never stacks on older turns.
#[test]
fn the_riding_card_is_verb_and_why_and_leaves_with_the_run() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::write(dir.path().join("teardown.rs"), "fn teardown() {}\n").expect("cited file");
    std::fs::create_dir_all(dir.path().join("project")).expect("tier root");
    std::fs::write(
        dir.path().join("project").join("gotchas.md"),
        "- [p12] helpful=0 harmful=0 cite the flake before blaming the test \
         [evidence: teardown.rs@000000000000]\n",
    )
    .expect("topic file");
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("session store"));
    let memory = Arc::new(gertie_core::MemoryStore::open(
        dir.path().join("project"),
        dir.path().join("global"),
        store.clone(),
    ));
    memory.sync().expect("sidecar sync");
    let (provider, scenarios) = replay_provider(
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"status\"\n\
         arguments = '{\"goal\": \"Fix the teardown race\", \"next\": \"re-run the suite\"}'\n\n\
         [[steps.tool_calls]]\nname = \"search_memory\"\n\
         arguments = '{\"query\": \"flake\"}'\n\n\
         [[steps.tool_calls]]\nname = \"park\"\narguments = '{}'\n\n\
         [[steps]]\ntext = 'done'\n",
    );
    let (release, park) = tokio::sync::oneshot::channel::<()>();
    let mut fixture = fixture_built(
        store,
        None,
        Some(dir.path().to_path_buf()),
        None,
        Some(provider),
        None,
        vec![
            Arc::new(gertie_tools::Status),
            Arc::new(gertie_tools::memory::SearchMemory::new(memory.clone())),
            Arc::new(ParkTool {
                park: std::sync::Mutex::new(Some(park)),
            }),
        ],
        Some(memory),
        None,
    );
    fixture.hold = vec![scenarios, dir];

    fixture.act(|window, cx| {
        window.input("The widget_test keeps flaking", cx);
        window.press("enter", cx);
    });
    let view = fixture.view.clone();
    pump_until(
        &mut fixture,
        200,
        |fixture| {
            fixture.cx.update(|cx| {
                view.upgrade()
                    .expect("view alive")
                    .read(cx)
                    .riding_for_test()
                    .is_some()
            })
        },
        "the consultation folds",
    );
    pump(&mut fixture, 10);

    // Parked mid-run on a running card, pinned to the bottom: without
    // a grounding the row would yield; the why keeps the card.
    let (shown, riding) = fixture.cx.update(|cx| {
        let view = view.upgrade().expect("view alive").read(cx);
        (view.show_activity_row_for_test(), view.riding_for_test())
    });
    assert!(shown, "a grounded run keeps its riding card");
    let (id, flag) = riding.expect("the why names the consulted entry");
    assert_eq!(id, "p12");
    let flag = flag.expect("moved evidence puts the full form in play");
    assert!(
        flag.contains("not re-read this session"),
        "the work never read the moved file: {flag}"
    );
    fixture.act(|window, _| {
        assert!(window.try_find("riding-card").is_some(), "the card rides");
        assert!(
            window.try_find("riding-flag").is_some(),
            "the flag line, the full form"
        );
    });

    let _ = release.send(());
    pump_until(
        &mut fixture,
        200,
        |fixture| {
            fixture
                .cx
                .update(|cx| !view.upgrade().expect("view alive").read(cx).is_streaming())
        },
        "the run ends",
    );
    fixture.act(|window, _| {
        assert!(
            window.try_find("riding-card").is_none(),
            "the card leaves with the run: no stacking on older turns"
        );
    });
}

/// The composer belongs to its conversation: words typed (and chips
/// parked) in one chat stay there when New chat or another chat takes
/// the view, and come back when it returns. Before, New chat inherited
/// whatever was half-written.
#[test]
fn a_draft_stays_with_the_conversation_it_was_written_in() {
    let mut fixture = fixture_with_store(two_session_store());
    let composer = |fixture: &mut Fixture| {
        fixture.cx.update(|cx| {
            let view = fixture.view.upgrade().expect("view alive");
            let chat = view.read(cx);
            (
                chat.composer_value_for_test(cx),
                chat.attachments_count_for_test(),
            )
        })
    };

    fixture.act(|window, cx| window.click("session-1", cx));
    fixture.act(|window, cx| window.input("half written", cx));
    fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        view.update(cx, |chat, cx| chat.attach_image_for_test(7, b"png", cx));
    });

    // New chat starts clean: nothing from the first chat rides in.
    fixture.act(|window, cx| window.click("new-chat", cx));
    assert_eq!(composer(&mut fixture), (String::new(), 0));

    // Another chat is clean too.
    fixture.act(|window, cx| window.click("session-2", cx));
    assert_eq!(composer(&mut fixture), (String::new(), 0));
    fixture.act(|window, cx| window.input("for two", cx));

    // Back to the first: its words and its chip come back.
    fixture.act(|window, cx| window.click("session-1", cx));
    assert_eq!(composer(&mut fixture), ("half written".to_string(), 1));

    // And the second kept its own.
    fixture.act(|window, cx| window.click("session-2", cx));
    assert_eq!(composer(&mut fixture), ("for two".to_string(), 0));
}

/// A screenshot attached while a turn runs used to be dropped: Enter
/// steered the running turn with the words alone, and the chip sat in
/// the composer for some later message. Now a message carrying chips
/// queues, the images ride in the queue row, and the drain sends them.
#[test]
fn an_image_sent_mid_turn_queues_and_rides_with_its_message() {
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("worker.toml"),
        "[[steps]]\ntext = ''\n\
         [[steps.tool_calls]]\nname = 'worker_ask'\narguments = '{}'\n\
         [[steps]]\ntext = ''\n\
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
        vision: true,
    };
    let provider = Arc::new(
        gertie_providers::AnyProvider::with_endpoints(vec![endpoint]).expect("replay provider"),
    );
    let broker = Arc::new(gertie_core::PermissionBroker::new());
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
        None,
    );
    let attachments = tempfile::tempdir().expect("attachments dir");
    fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        view.update(cx, |chat, _| {
            chat.set_attachments_dir_for_test(attachments.path())
        });
    });

    // The turn parks on the worker's ask: it is running.
    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 30);
    assert!(fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .streaming_state_for_test()
    }));

    // A screenshot and words, Ctrl+Enter: words alone would steer.
    fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        view.update(cx, |chat, cx| chat.attach_image_for_test(42, b"pixels", cx));
    });
    fixture.act(|window, cx| {
        window.input("see this", cx);
        window.press("secondary-enter", cx);
    });
    let (queued, chips, notices) = fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        let chat = view.read(cx);
        (
            chat.queued_for_test(),
            chat.attachments_count_for_test(),
            chat.notice_texts_for_test(),
        )
    });
    assert_eq!(queued.len(), 1, "the message queued instead of steering");
    assert!(queued[0].starts_with("see this"));
    assert!(
        queued[0].contains(gertie_core::IMAGE_MARKER_PREFIX),
        "the image rides in the queue row: {queued:?}"
    );
    assert_eq!(chips, 0, "no chip left behind for a later message");
    assert!(
        notices
            .iter()
            .any(|text| text.contains("Attachments can't join a running turn"))
    );

    // The turn ends, the drain sends the queued message with its image.
    fixture.act(|window, cx| window.click("approve-2", cx));
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
    pump_until(
        &mut fixture,
        1000,
        |fixture| {
            fixture
                .store
                .load_messages(session_id)
                .unwrap_or_default()
                .iter()
                .any(|message| message.content.starts_with("see this"))
        },
        "the queued message to be sent",
    );
    let sent = store
        .load_messages(session_id)
        .expect("rows")
        .into_iter()
        .find(|message| message.content.starts_with("see this"))
        .expect("queued message sent");
    let (_, paths) = gertie_core::extract_image_markers(&sent.content);
    assert_eq!(
        paths.len(),
        1,
        "the sent row carries its image: {:?}",
        sent.content
    );
    assert_eq!(
        std::fs::read(&paths[0]).expect("image filed"),
        b"pixels",
        "the bytes on disk are the screenshot's"
    );
}

/// A live turn parked on a write card (`approve-1`), with steps left
/// over for the turns the queue starts afterwards.
fn held_turn_fixture() -> Fixture {
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("held.toml"),
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"write_file\"\n\
         arguments = '{\"path\": \"q.txt\", \"content\": \"x\"}'\n\n\
         [[steps]]\ntext = ''\n\n[[steps]]\ntext = ''\n\n[[steps]]\ntext = ''\n",
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
        vision: true,
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
        None,
    );
    fixture.hold = vec![scenarios, workspace];
    fixture.act(|window, cx| {
        window.input("go", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 20);
    fixture
}

fn the_session(fixture: &mut Fixture) -> gertie_core::SessionId {
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
        .expect("session born")
}

fn user_rows(fixture: &Fixture, session: gertie_core::SessionId) -> Vec<String> {
    fixture
        .store
        .load_messages(session)
        .unwrap_or_default()
        .into_iter()
        .filter(|row| row.role == gertie_core::Role::User)
        .map(|row| row.content)
        .collect()
}

/// Three quick afterthoughts are one request: the queue goes out as a
/// single message when the turn ends, in the order it was written.
#[test]
fn queued_followups_go_out_together_as_one_message() {
    let mut fixture = held_turn_fixture();
    fixture.act(|window, cx| {
        window.input("first thought", cx);
        window.press("enter", cx);
    });
    fixture.act(|window, cx| {
        window.input("second thought", cx);
        window.press("enter", cx);
    });
    let session = the_session(&mut fixture);
    assert_eq!(fixture.store.queued_followups(session).unwrap().len(), 2);

    fixture.act(|window, cx| window.click("approve-1", cx));
    pump_until(
        &mut fixture,
        1000,
        |fixture| {
            let session = the_session(fixture);
            user_rows(fixture, session)
                .iter()
                .any(|row| row == "first thought\n\nsecond thought")
        },
        "the combined follow-up",
    );
    let rows = user_rows(&fixture, session);
    assert!(
        !rows.iter().any(|row| row == "second thought"),
        "no turn of its own for the second: {rows:?}"
    );
    assert!(fixture.store.queued_followups(session).unwrap().is_empty());
}

/// Ctrl+Enter steers: the words go to the running turn, show as in
/// flight until the loop confirms them, and land as a user row.
#[test]
fn ctrl_enter_steers_and_the_steer_is_tracked_until_it_joins() {
    let mut fixture = held_turn_fixture();
    fixture.act(|window, cx| {
        window.input("steer this way", cx);
        window.press("secondary-enter", cx);
    });
    let session = the_session(&mut fixture);
    let in_flight = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .steers_in_flight_for_test()
    });
    assert_eq!(in_flight, ["steer this way"]);
    assert!(
        fixture.store.queued_followups(session).unwrap().is_empty(),
        "a steer is not a queued row"
    );

    fixture.act(|window, cx| window.click("approve-1", cx));
    pump_until(
        &mut fixture,
        1000,
        |fixture| {
            let session = the_session(fixture);
            user_rows(fixture, session)
                .iter()
                .any(|row| row == "steer this way")
        },
        "the steer to land",
    );
    pump_until(
        &mut fixture,
        1000,
        |fixture| {
            fixture.cx.update(|cx| {
                let view = fixture.view.upgrade().expect("view alive");
                let chat = view.read(cx);
                !chat.streaming_state_for_test() && chat.steers_in_flight_for_test().is_empty()
            })
        },
        "the turn to end with nothing in flight",
    );
}

/// A steer the turn ends without reading isn't lost: it leads the
/// next message.
#[test]
fn a_steer_the_turn_never_read_leads_the_next_message() {
    let mut fixture = held_turn_fixture();
    fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        view.update(cx, |chat, _| chat.plant_unread_steer_for_test("late words"));
    });
    let session = the_session(&mut fixture);
    fixture.act(|window, cx| window.click("approve-1", cx));
    pump_until(
        &mut fixture,
        1000,
        |fixture| {
            let session = the_session(fixture);
            user_rows(fixture, session)
                .iter()
                .any(|row| row == "late words")
        },
        "the unread steer to go out",
    );
    let _ = session;
}

/// A stop holds the queue: nothing fires, the composer area says what
/// is waiting, and the next message carries the queue ahead of itself.
#[test]
fn a_stop_holds_the_queue_and_the_next_message_carries_it() {
    let mut fixture = held_turn_fixture();
    fixture.act(|window, cx| {
        window.input("held thought", cx);
        window.press("enter", cx);
    });
    let session = the_session(&mut fixture);
    fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        view.update(cx, |chat, cx| chat.stop_turn(cx));
    });
    pump_until(
        &mut fixture,
        1000,
        |fixture| {
            !fixture.cx.update(|cx| {
                fixture
                    .view
                    .upgrade()
                    .expect("view alive")
                    .read(cx)
                    .streaming_state_for_test()
            })
        },
        "the stop to land",
    );
    pump(&mut fixture, 10);
    assert_eq!(
        fixture.store.queued_followups(session).unwrap().len(),
        1,
        "the stop held the queue"
    );
    assert!(
        !user_rows(&fixture, session)
            .iter()
            .any(|row| row.contains("held thought"))
    );
    fixture.act(|window, _cx| {
        assert!(
            window.try_find("queue-send-all").is_some(),
            "the held line shows"
        );
    });

    fixture.act(|window, cx| {
        window.input("new ask", cx);
        window.press("enter", cx);
    });
    assert!(
        user_rows(&fixture, session)
            .iter()
            .any(|row| row == "held thought\n\nnew ask"),
        "the queue rode ahead of the new message: {:?}",
        user_rows(&fixture, session)
    );
    assert!(fixture.store.queued_followups(session).unwrap().is_empty());
}

/// Per-root answers move with the root: after a switch, the check is
/// the new project's and recall reads the new root's memory. Both
/// used to stay with the folder the app started in.
#[test]
fn a_switch_moves_the_check_and_the_memory_with_the_root() {
    let mut fixture = fixture();
    let elsewhere = tempfile::tempdir().expect("other root");
    let memory = Arc::new(gertie_core::MemoryStore::open(
        elsewhere.path().join(".gertie/memory"),
        elsewhere.path().join("global"),
        fixture.store.clone(),
    ));
    let switched = gertie::bootstrap::Switched {
        tools: Arc::new(Default::default()),
        system_prompt: None,
        instructions: None,
        memory_index: None,
        memory: Some(memory.clone()),
        memory_extract_provider: None,
        workspace_root: elsewhere.path().to_path_buf(),
        templates: Vec::new(),
        sandbox_bash: false,
        verify: Some("make check".into()),
        warnings: Vec::new(),
    };
    let (verify, store) = fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        view.update(cx, |chat, cx| {
            chat.adopt_switch(switched, cx);
            (chat.verify_for_test(), chat.memory_store_for_test())
        })
    });
    assert_eq!(verify.as_deref(), Some("make check"));
    assert!(
        store.is_some_and(|store| Arc::ptr_eq(&store, &memory)),
        "recall reads the new root's memory"
    );
}

/// #50's first promise: `read_file` on a PNG puts the picture in front
/// of the model, and it survives a reload. The scripted turn opens an
/// image in a round that says nothing, then makes two calls in a round
/// that does: the reloaded history holds every round whole, the image
/// back on its result, and no empty bubble in the transcript.
#[test]
fn an_image_read_file_opens_survives_a_reload() {
    let scenarios = tempfile::tempdir().expect("scenario dir");
    std::fs::write(
        scenarios.path().join("look.toml"),
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"read_file\"\n\
         arguments = '{\"path\": \"logo.png\"}'\n\n\
         [[steps]]\ntext = 'checking the rest'\n\n\
         [[steps.tool_calls]]\nname = \"glob\"\n\
         arguments = '{\"pattern\": \"*.png\"}'\n\n\
         [[steps.tool_calls]]\nname = \"glob\"\n\
         arguments = '{\"pattern\": \"*.txt\"}'\n\n\
         [[steps]]\ntext = 'a red square'\n",
    )
    .expect("scenario file");
    let workspace = tempfile::tempdir().expect("workspace dir");
    // The smallest valid PNG: one red pixel.
    let png: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8,
        0xcf, 0xc0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xdd, 0x8d, 0xb0, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    std::fs::write(workspace.path().join("logo.png"), png).expect("image");
    let attachments = tempfile::tempdir().expect("attachments dir");
    let endpoint = gertie_providers::Endpoint {
        name: "replay".into(),
        base_url: scenarios.path().to_string_lossy().into_owned(),
        model: "look".into(),
        api_key: None,
        context_window: None,
        wire: gertie_providers::Wire::Replay,
        vision: true,
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
        None,
    );
    fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        view.update(cx, |chat, _| {
            chat.set_attachments_dir_for_test(attachments.path())
        });
    });
    fixture.act(|window, cx| {
        window.input("what is logo.png", cx);
        window.press("enter", cx);
    });
    pump(&mut fixture, 60);
    let live_turns = turn_count_of(&mut fixture);

    let session = fixture.store.list_sessions().unwrap()[0].id;
    let rows = fixture.store.load_messages(session).unwrap();
    let row = rows
        .iter()
        .find(|row| row.tool_name.as_deref() == Some("read_file"))
        .expect("the read ran");
    assert!(
        row.content.starts_with("logo.png: image, 1x1"),
        "{}",
        row.content
    );
    assert_eq!(row.images.len(), 1, "the row keeps the image's file");
    let saved = Path::new(&row.images[0]);
    assert!(saved.starts_with(attachments.path()), "{}", saved.display());
    assert_eq!(std::fs::read(saved).unwrap(), png);

    // The history a resume sends: every round whole, in order.
    let wire = fixture.store.load_wire_history(session).unwrap();
    let shape: Vec<(gertie_core::Role, Option<&str>, usize)> = wire
        .iter()
        .map(|message| {
            (
                message.role,
                message.content.as_deref(),
                message.tool_calls.len(),
            )
        })
        .collect();
    use gertie_core::Role::{Assistant, Tool, User};
    assert_eq!(shape[0], (User, Some("what is logo.png"), 0));
    assert_eq!(
        shape[1],
        (Assistant, None, 1),
        "the silent round keeps its call"
    );
    assert_eq!(shape[2].0, Tool);
    assert_eq!(shape[3], (Assistant, Some("checking the rest"), 2));
    assert_eq!(shape[4].0, Tool);
    assert_eq!(shape[5].0, Tool, "the second call's result survives too");
    assert_eq!(shape[6], (Assistant, Some("a red square"), 0));
    assert_eq!(wire.len(), 7);
    assert_eq!(
        wire[2].attachments.len(),
        1,
        "the image is back on its result"
    );
    assert!(wire[2].attachments[0].starts_with("data:image/png;base64,"));

    // Reopening shows what the live turn showed: no empty bubble for
    // the round that only called a tool.
    fixture.act(|window, cx| window.click("new-chat", cx));
    let view = fixture.view.upgrade().expect("view alive");
    fixture.act(|window, cx| {
        view.update(cx, |chat, cx| chat.switch_to_session(session, window, cx));
    });
    pump(&mut fixture, 10);
    assert_eq!(turn_count_of(&mut fixture), live_turns);
    fixture.hold = vec![scenarios, workspace, attachments];
}

/// The evidence flag's automation: a consulted entry whose cited file
/// the session re-reads gets its hash re-recorded at that moment (the
/// reread IS the re-check), so the `[evidence changed]` flag clears
/// without a click or a tool call. The turn: search_memory consults
/// the entry, read_file reads the (stale-hashed) cited file, done.
#[test]
fn a_reread_of_a_cited_file_reverifies_the_consulted_entry() {
    let dir = tempfile::tempdir().expect("workspace");
    std::fs::create_dir_all(dir.path().join("project")).expect("tier root");
    std::fs::write(dir.path().join("teardown.rs"), "fn teardown() {}\n").expect("cited file");
    // The entry cites teardown.rs with the hash of its ORIGINAL
    // content; then the file moves, so the entry is flagged.
    std::fs::write(
        dir.path().join("project").join("gotchas.md"),
        format!(
            "- [p1] helpful=0 harmful=0 the teardown timer races the widget on retries \
             [evidence: teardown.rs@{}] [source: added: 2026-11-27]\n",
            gertie_core::memory::blob_hash(b"fn teardown() {}\n")
        ),
    )
    .expect("topic file");
    std::fs::write(
        dir.path().join("teardown.rs"),
        "fn teardown() -> bool { true }\n",
    )
    .expect("the cited file moves");
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("session store"));
    let memory = Arc::new(gertie_core::MemoryStore::open(
        dir.path().join("project"),
        dir.path().join("global"),
        store.clone(),
    ));
    memory.sync().expect("sidecar sync");
    let flagged = memory
        .scan()
        .into_iter()
        .find(|entry| entry.id == "p1")
        .expect("the entry");
    assert_eq!(
        memory.changed_evidence(&flagged),
        vec!["teardown.rs"],
        "the setup: the cited file moved, the entry is flagged"
    );
    let (provider, scenarios) = replay_provider(
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"search_memory\"\n\
         arguments = '{\"query\": \"teardown\"}'\n\n\
         [[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"read_file\"\n\
         arguments = '{\"path\": \"teardown.rs\"}'\n\n\
         [[steps]]\ntext = 'done'\n",
    );
    let mut fixture = fixture_built(
        store.clone(),
        None,
        Some(dir.path().to_path_buf()),
        None,
        Some(provider),
        None,
        vec![
            Arc::new(gertie_tools::memory::SearchMemory::new(memory.clone())),
            Arc::new(gertie_tools::ReadFile::new(dir.path().to_path_buf())),
        ],
        Some(memory.clone()),
        None,
    );
    fixture.hold = vec![scenarios, dir];
    fixture.act(|window, cx| {
        window.input("why does widget_test flake?", cx);
        window.press("enter", cx);
    });
    pump_until(
        &mut fixture,
        200,
        |fixture| {
            fixture.cx.update(|cx| {
                !fixture
                    .view
                    .upgrade()
                    .expect("view alive")
                    .read(cx)
                    .is_streaming()
            })
        },
        "the scripted turn",
    );
    let entry = memory
        .scan()
        .into_iter()
        .find(|entry| entry.id == "p1")
        .expect("the entry");
    assert!(
        memory.changed_evidence(&entry).is_empty(),
        "the reread re-recorded the hash: the flag cleared"
    );
    assert_eq!(
        entry.evidence[0].hash,
        gertie_core::memory::blob_hash(b"fn teardown() -> bool { true }\n"),
        "the stored hash is the file's current state"
    );
}

/// The bash reread path: this session's actual reading shape is
/// `sed -n`, `grep`, `python3 -c` through bash, not the file tool,
/// and a cited file re-read that way is as verified as one read by
/// read_file. The fold extracts the command's file targets (the
/// TOUCHED state machine), matches them against the consulted
/// entry's evidence, and the hash re-records.
#[test]
fn a_bash_reread_of_a_cited_file_also_reverifies() {
    let (mut fixture, _keep) = fixture_with_memory();
    let dir = tempfile::tempdir().expect("cited dir");
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("src")).expect("src");
    std::fs::write(root.join("src/cited.rs"), "old\n").expect("cited");

    // The entry cites src/cited.rs and this chat consulted it.
    fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        view.update(cx, |chat, _cx| {
            let memory = chat.memory_store_for_test().expect("memory store");
            let report = memory
                .apply(
                    &[gertie_core::memory::Op::Add {
                        file: "gotchas.md".into(),
                        text: "the cited file holds the constant".into(),
                        source: None,
                        evidence: vec!["src/cited.rs".into()],
                    }],
                    10_000,
                )
                .expect("add");
            let added = report.applied.first().expect("one id").clone();
            // The fold consults what the turn's tools surface: the
            // scripted turn's search_memory result names the id, the
            // same natural path a real consultation takes.
            chat.fold_status_tool_for_test(
                "search_memory",
                r#"{"query":"constant"}"#,
                &format!("[{added}] gotchas.md, added -\n   the cited file holds the constant\n"),
            );
        });
    });

    // The bash read, folded the way the live loop folds it (no turn
    // needed: the fold is the unit under test).
    fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        view.update(cx, |chat, _| {
            chat.fold_status_tool_for_test(
                "bash",
                r#"{"command":"grep constant src/cited.rs"}"#,
                "the constant",
            );
        });
    });

    // The flag never got to exist: the bash reread re-recorded the
    // hash while the turn folded.
    let stale = fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive");
        let memory = view.read(cx).memory_store_for_test().expect("memory store");
        let entry = memory
            .scan()
            .into_iter()
            .find(|entry| entry.text.contains("the cited file"))
            .expect("the entry");
        memory.changed_evidence(&entry)
    });
    assert!(
        !stale.contains(&"src/cited.rs".to_string()),
        "the bash reread re-recorded the hash, got {stale:?}"
    );
}

/// The settings page: four sections with substance (not eight
/// one-switch tabs), the nav switching which shows, and the page
/// covering the middle column's composer and footer while it's open.
#[test]
fn the_settings_page_groups_four_sections_and_covers_the_middle_column() {
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("session store"));
    let mut fixture = fixture_with_store(store);
    // Open settings via the palette: ctrl-k, a pump (the palette's
    // input mounts on the draw), then type the query and confirm.
    fixture.act(|window, _cx| window.press("ctrl-k", _cx));
    pump(&mut fixture, 2);
    fixture.act(|window, cx| window.input(">settings", cx));
    fixture.act(|window, cx| window.press("enter", cx));
    let state = fixture.cx.update(|cx| {
        let view = fixture.view.upgrade().expect("view alive").read(cx);
        (view.palette_open_for_test(), view.panel_for_test())
    });
    assert_eq!(state.1, "settings", "the settings panel opened");
    fixture.act(|window, _| {
        let window = window;
        assert!(
            window.try_find("settings-panel").is_some()
                || window.try_find("settings-content").is_some(),
            "the settings page painted (settings-panel / settings-content missing)"
        );
        // The modal covers the sidebar: the button survives in the
        // tree, and the backdrop's click closes the dialog.
        assert!(
            window.try_find("toggle-settings").is_some(),
            "the sidebar's settings button survives under the modal"
        );
        // The dialog carries its own visible close affordance.
        assert!(
            window.try_find("settings-close").is_some(),
            "the dialog has its close ✕"
        );
        assert!(
            window.try_find("nav-chat").is_some() && window.try_find("nav-tools").is_some(),
            "the nav has the four grouped sections"
        );
        assert!(
            window.try_find("nav-appearance").is_none() && window.try_find("nav-config").is_none(),
            "the eight-tab nav is gone"
        );
        // The dialog is its body's height (680px), not the window's:
        // a modal sizes to its content, no full-screen void.
        let panel = window
            .try_find("settings-panel")
            .expect("panel painted")
            .bounds();
        assert!(
            (panel.size.height - px(680.)).abs() < px(2.),
            "the dialog sizes to its body: {panel:?}"
        );
        // The card column fills the pane right of the nav: its gaps
        // to the nav's edge and the dialog's edge are both the pane's
        // padding (24px), symmetric by construction.
        let nav = window.try_find("nav-chat").expect("nav painted").bounds();
        let column = window
            .try_find("settings-column")
            .expect("card column painted")
            .bounds();
        let left_gap = column.left() - nav.right();
        let right_gap = panel.right() - column.right();
        assert!(
            left_gap > px(0.),
            "content clears the nav: panel {panel:?} column {column:?}"
        );
        assert!(
            (left_gap - right_gap).abs() < px(40.),
            "the card column fills its pane evenly: left gap {left_gap:?}, right gap {right_gap:?}"
        )
    });
    // Switching sections swaps the content: Tools & agents owns the
    // sandbox switch; Chat owns the mode buttons.
    fixture.act(|window, cx| window.click("nav-tools", cx));
    fixture.act(|window, _| {
        assert!(
            window.try_find("sandbox-switch").is_some(),
            "Tools & agents shows the sandbox switch"
        );
        assert!(
            window.try_find("mode-supervised").is_none(),
            "the mode choices live in Chat, not Tools"
        );
    });
    fixture.act(|window, cx| window.click("nav-chat", cx));
    fixture.act(|window, _| {
        assert!(
            window.try_find("mode-supervised").is_some(),
            "Chat shows the permission-mode choices"
        );
        assert!(
            window.try_find("settle-7 days").is_some(),
            "Chat shows the auto-settle choices"
        );
    });
    // Close: Esc over the page hands the middle column back.
    fixture.act(|window, cx| window.press("escape", cx));
    let closed = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .panel_for_test()
    });
    assert_eq!(closed, "none", "escape closes the page");
    fixture.act(|window, cx| {
        assert!(
            window.try_find("settings-panel").is_none(),
            "the page is gone after esc"
        );
    });
    // The ✕ closes too: reopen, click it.
    fixture.act(|window, cx| window.click("toggle-settings", cx));
    let reopened = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .panel_for_test()
    });
    assert_eq!(reopened, "settings", "the sidebar's toggle reopens");
    fixture.act(|window, cx| window.click("settings-close", cx));
    let xed = fixture.cx.update(|cx| {
        fixture
            .view
            .upgrade()
            .expect("view alive")
            .read(cx)
            .panel_for_test()
    });
    assert_eq!(xed, "none", "the ✕ closes the page");
}

/// The settings modal's geometry, measured where it engages and with
/// a status column present: the wide shell runs a scripted turn that
/// sets a goal, so the column is on screen; the modal's backdrop
/// covers everything (page-sized settings left dead voids), the
/// dialog centers in the window, and the column returns on close.
#[test]
fn settings_modal_covers_and_centers_at_a_wide_window() {
    let dir = tempfile::tempdir().expect("workspace");
    let store = Arc::new(SessionStore::open(Path::new(":memory:")).expect("session store"));
    let (provider, scenarios) = replay_provider(
        "[[steps]]\ntext = ''\n\n\
         [[steps.tool_calls]]\nname = \"status\"\n\
         arguments = '{\"goal\": \"Fix the teardown race\", \"next\": \"re-run the suite\"}'\n\n\
         [[steps]]\ntext = 'done'\n",
    );
    let mut fixture = fixture_built(
        store,
        None,
        Some(dir.path().to_path_buf()),
        None,
        Some(provider),
        None,
        vec![Arc::new(gertie_tools::Status)],
        None,
        None,
    );
    // dir moves into fixture.hold; the wide view reads the workspace
    // path through the fixture's resumed session, not the tempdir.
    let workspace = dir.path().to_path_buf();
    fixture.hold = vec![scenarios, dir];
    // A turn that sets the column's goal, so it has content to show.
    fixture.act(|window, cx| {
        window.input("The widget_test keeps flaking", cx);
        window.press("enter", cx);
    });
    pump_until(
        &mut fixture,
        200,
        |fixture| {
            fixture.cx.update(|cx| {
                !fixture
                    .view
                    .upgrade()
                    .expect("view alive")
                    .read(cx)
                    .is_streaming()
            })
        },
        "the scripted turn",
    );
    // The wide shell: born 1600px, a resize does not relayout the
    // headless window, so the window must open wide for max-width
    // centering to engage.
    let wide: AnyWindowHandle = fixture.cx.update(|cx| {
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Default::default(),
                    size: size(px(1600.), px(900.)),
                })),
                ..Default::default()
            },
            |window, cx| {
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
                        gertie::chat::ChatViewConfig {
                            store: fixture.store.clone(),
                            provider: None,
                            snapshots: None,
                            tools: gertie_tools::builtin_tools(Path::new(".")),
                            endpoint_label: "no endpoint configured".into(),
                            system_prompt: None,
                            instructions: None,
                            memory_index: None,
                            memory_enabled: false,
                            memory_store: None,
                            memory_extract_provider: None,
                            max_tool_rounds: None,
                            grace_rounds: None,
                            templates: Vec::new(),
                            // The fixture turn's workspace: resume picks
                            // its newest unsettled session, and the
                            // rebuild folds the status tool call back
                            // into the column.
                            workspace_root: workspace.clone(),
                            settle_command: None,
                            verify: None,
                            tool_guidance: gertie_core::ToolGuidance::Standard,
                            compact_at: None,
                            repo_map_budget: None,
                            broker: Arc::new(gertie_core::PermissionBroker::new()),
                            session_scope: gertie_core::SessionScope::new(),
                            mode_scope: gertie_core::ModeScope::new(),
                            sandbox_bash: false,
                            default_mode: gertie_core::PermissionMode::Supervised,
                            settings_panel: panel,
                            resume: true,
                            branch_naming: None,
                            live_elsewhere: Arc::new(|_, _, _| Ok(())),
                            switch: Arc::new(|root: &Path| {
                                Ok(gertie::bootstrap::Switched {
                                    tools: gertie_tools::builtin_tools(root),
                                    system_prompt: None,
                                    instructions: None,
                                    memory_index: None,
                                    memory: None,
                                    memory_extract_provider: None,
                                    workspace_root: root.to_path_buf(),
                                    templates: Vec::new(),
                                    sandbox_bash: false,
                                    verify: None,
                                    warnings: Vec::new(),
                                })
                            }),
                            session_router: None,
                        },
                        window,
                        cx,
                    )
                });
                cx.new(|cx| Root::new(view, window, cx))
            },
        )
        .expect("wide window")
        .into()
    });
    let draw = |fixture: &mut Fixture,
                wide: AnyWindowHandle,
                f: &dyn Fn(&mut gpui_kit::Window, &mut gpui_kit::App)| {
        fixture
            .cx
            .update_window(wide, |_, window, cx| f(window, cx))
            .expect("wide window");
        settle(&mut fixture.cx, wide);
    };
    // The resume rebuilds the column from the session's status tool
    // call. The rebuild folds the stored rows on the background
    // executor: wait for the wide window's own tree to show it (the
    // fixture's view handle is the first window's chat, not this one).
    let column_up = std::cell::Cell::new(false);
    for _ in 0..100 {
        pump(&mut fixture, 2);
        draw(&mut fixture, wide, &|window, _| {
            column_up.set(window.try_find("status-column").is_some());
        });
        if column_up.get() {
            break;
        }
    }
    assert!(
        column_up.get(),
        "the resumed session shows the status column"
    );
    // Open settings via the palette.
    draw(&mut fixture, wide, &|window, cx| window.press("ctrl-k", cx));
    draw(&mut fixture, wide, &|window, cx| {
        window.input(">settings", cx)
    });
    draw(&mut fixture, wide, &|window, cx| window.press("enter", cx));
    draw(&mut fixture, wide, &|window, _| {
        // The modal covers the whole window, column included: the
        // backdrop spans everything, the dialog is centered in it,
        // and no sidebar or column element needs measuring.
        let backdrop = window
            .try_find("settings-backdrop")
            .expect("backdrop painted")
            .bounds();
        let panel = window
            .try_find("settings-panel")
            .expect("panel painted")
            .bounds();
        assert!(
            backdrop.size.width > px(1400.) && backdrop.size.height > px(800.),
            "the backdrop covers the window: {backdrop:?}"
        );
        // The dialog centers in the window: its gaps to the backdrop's
        // edges match.
        let left_gap = panel.left() - backdrop.left();
        let right_gap = backdrop.right() - panel.right();
        assert!(
            (left_gap - right_gap).abs() < px(40.),
            "the dialog centers: left gap {left_gap:?}, right gap {right_gap:?}"
        );
        // The dialog is its body's height (560px), not the window's.
        assert!(
            (panel.size.height - px(680.)).abs() < px(2.),
            "the dialog is its body's height: {panel:?}"
        );
    });
    // Esc closes, and the column comes back.
    draw(&mut fixture, wide, &|window, cx| window.press("escape", cx));
    draw(&mut fixture, wide, &|window, _| {
        assert!(
            window.try_find("status-column").is_some(),
            "the column returns when the modal closes"
        );
    });
    fixture
        .cx
        .update_window(wide.into(), |_, window, _| window.remove_window())
        .ok();
}
