//! The repo map's seam in the loop (#41): the harness hands the model a
//! compact, ranked map of the workspace's code at session start, so it
//! spends fewer rounds finding where things are.
//!
//! The map itself (walking, tree-sitter, the cache) lives in
//! gertie-tools; core only knows a [`RepoMapSource`] that renders one
//! for some [`MapHints`]. A [`SessionMap`] renders once and freezes: the
//! text rides every request of the session as framing, ahead of
//! history, so it must stay byte-stable or the provider's prompt cache
//! breaks every round (the memory index's rule). The `repo_map` tool
//! answers anything newer.

use crate::Role;
use crate::request::WireMessage;
use std::sync::{Arc, OnceLock};

/// What the session tells the ranking: files it already touched, files
/// live memory cites, and the user's own words (a path or file name
/// they wrote ranks first).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MapHints {
    pub touched: Vec<String>,
    pub cited: Vec<String>,
    pub said: String,
}

/// Renders a budgeted map body for some hints, or None (no source files,
/// nothing to say). Implementations bound their own cost.
pub trait RepoMapSource: Send + Sync {
    fn render(&self, hints: &MapHints, budget_tokens: usize) -> Option<String>;
}

/// A session's map: rendered on its first turn, then the same text for
/// every request after. Clones share the frozen text.
#[derive(Clone)]
pub struct SessionMap {
    source: Arc<dyn RepoMapSource>,
    budget_tokens: usize,
    frozen: Arc<OnceLock<Option<String>>>,
}

impl SessionMap {
    pub fn new(source: Arc<dyn RepoMapSource>, budget_tokens: usize) -> Self {
        Self {
            source,
            budget_tokens,
            frozen: Arc::new(OnceLock::new()),
        }
    }

    /// The frozen text, rendering it on the first call. The render runs
    /// on its own thread: a first walk of a large tree must not stall
    /// the executor the loop shares with the UI.
    pub async fn text(&self, hints: MapHints) -> Option<String> {
        if let Some(text) = self.frozen.get() {
            return text.clone();
        }
        let source = self.source.clone();
        let budget = self.budget_tokens;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = sender.send(source.render(&hints, budget));
        });
        let rendered = receiver.await.ok().flatten();
        self.frozen.get_or_init(|| rendered).clone()
    }

    /// Whether the text is already frozen (tests, and the GUI deciding
    /// whether a session needs a new map).
    pub fn is_frozen(&self) -> bool {
        self.frozen.get().is_some()
    }
}

const TOUCHING_TOOLS: [&str; 3] = ["read_file", "edit_file", "write_file"];

/// The hints a history carries: paths the file tools were called with,
/// and everything the user wrote. Memory's cited files come from the
/// caller, which holds the store.
pub fn hints_from(history: &[WireMessage], cited: Vec<String>) -> MapHints {
    let mut touched: Vec<String> = Vec::new();
    let mut said = String::new();
    for message in history {
        if message.role == Role::User
            && let Some(text) = &message.content
        {
            said.push_str(text);
            said.push('\n');
        }
        for call in &message.tool_calls {
            if !TOUCHING_TOOLS.contains(&call.name.as_str()) {
                continue;
            }
            let path = serde_json::from_str::<serde_json::Value>(&call.arguments)
                .ok()
                .and_then(|value| value["path"].as_str().map(str::to_string));
            if let Some(path) = path {
                let path = path.trim_start_matches("./").to_string();
                if !touched.contains(&path) {
                    touched.push(path);
                }
            }
        }
    }
    MapHints {
        touched,
        cited,
        said,
    }
}

/// The framing message around a map body.
pub fn framing(body: &str) -> String {
    format!(
        "[repo map -- frozen for this session. The workspace's source files, ranked for this \
         conversation, with their top-level symbols. Use it to go straight to the right file; \
         repo_map lists a directory, or a file's signatures with line numbers]\n{body}\n\
         [end repo map]"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::ToolCall;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting(AtomicUsize);

    impl RepoMapSource for Counting {
        fn render(&self, hints: &MapHints, budget: usize) -> Option<String> {
            let run = self.0.fetch_add(1, Ordering::SeqCst);
            Some(format!(
                "run {run}, budget {budget}, said {}",
                hints.said.trim()
            ))
        }
    }

    /// A tiny executor: the futures here only wait on a thread.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return value;
            }
            assert!(std::time::Instant::now() < deadline, "never settled");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Rendered once, then frozen: a later turn with other hints gets
    /// the same bytes, the cache rule.
    #[test]
    fn the_map_renders_once_and_freezes() {
        let source = Arc::new(Counting(AtomicUsize::new(0)));
        let map = SessionMap::new(source.clone(), 300);
        assert!(!map.is_frozen());
        let first = block_on(map.text(MapHints {
            said: "fix the pool".into(),
            ..MapHints::default()
        }));
        let second = block_on(map.clone().text(MapHints {
            said: "something else".into(),
            ..MapHints::default()
        }));
        assert_eq!(
            first.as_deref(),
            Some("run 0, budget 300, said fix the pool")
        );
        assert_eq!(first, second);
        assert_eq!(source.0.load(Ordering::SeqCst), 1);
        assert!(map.is_frozen());
    }

    #[test]
    fn hints_carry_touched_paths_and_the_users_words() {
        let call = ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: r#"{"path": "./src/pool.rs"}"#.into(),
        };
        let grep = ToolCall {
            id: "2".into(),
            name: "grep".into(),
            arguments: r#"{"pattern": "x", "path": "src"}"#.into(),
        };
        let history = vec![
            WireMessage::user("where is the retry delay?"),
            WireMessage::assistant_with_tool_calls(None, vec![call.clone(), grep.clone()]),
            WireMessage::tool_result(&call, "..."),
            WireMessage::tool_result(&grep, "..."),
        ];
        let hints = hints_from(&history, vec!["docs/a.md".into()]);
        assert_eq!(hints.touched, vec!["src/pool.rs".to_string()]);
        assert_eq!(hints.cited, vec!["docs/a.md".to_string()]);
        assert!(hints.said.contains("retry delay"));
    }
}
