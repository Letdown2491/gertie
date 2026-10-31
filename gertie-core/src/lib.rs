//! gertie-core: the headless agent harness.
//!
//! This crate is the whole harness minus the UI. It must never depend on a
//! GUI framework: it is embeddable from a CLI, a server, or an editor. The
//! UI-facing `gertie` crate is one client of many.

pub mod agent;
pub mod agent_loop;
pub mod memory;
pub mod memory_extract;
pub mod permissions;
pub mod provider;
pub mod request;
pub mod snapshot;
pub mod tool;

pub use agent::{AgentTool, AgentType, builtin_agent_types, sanitize_untrusted, worker_registry};
pub use agent_loop::{
    DEFAULT_GRACE_ROUNDS, DEFAULT_MAX_TOOL_ROUNDS, LoopEvent, PermissionDecision, SharedHistory,
    ToolRegistry, TurnSpec, run_turn,
};
pub use memory::{MemoryEntry, MemoryHit, MemoryStore, Op, Tier};
pub use permissions::{ModeScope, PendingAsk, PermissionBroker, PermissionMode, PromptSource};
pub use provider::Provider;
pub use request::{
    IMAGE_MARKER_PREFIX, Request, ToolCall, ToolSpec, WireMessage, data_url_parts,
    extract_image_markers, file_to_data_url, media_type_for_extension,
};
use rusqlite::Connection;
pub use snapshot::{
    AfterState, Checkpoint, CheckpointId, SnapshotStore, TurnBounds, TurnFileChange, TurnId,
};
use std::path::Path;
pub use tool::{Tool, ToolResult};

/// Identifies one message in a session's message tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageId(pub u64);

/// Identifies a queued follow-up row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueuedId(pub u64);

/// Identifies a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u64);

/// The front's current session, shared with machinery that runs outside
/// any session's knowledge (subagent dispatch) so worker runs can
/// record their lineage. One atomic cell: the front sets it whenever the
/// open session changes; dispatch reads it once. `None` means unknown:
/// worker runs still persist, just unlinked.
#[derive(Default)]
pub struct SessionScope(std::sync::Arc<std::sync::atomic::AtomicI64>);

impl SessionScope {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, session: Option<SessionId>) {
        self.0.store(
            session.map_or(0, |session| session.0 as i64),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub fn get(&self) -> Option<SessionId> {
        let raw = self.0.load(std::sync::atomic::Ordering::Relaxed);
        (raw != 0).then_some(SessionId(raw as u64))
    }
}

impl Clone for SessionScope {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

/// Characters a human can paste but never see: bidirectional controls
/// that reorder displayed text, zero-width spaces and joiners that
/// glue tokens together, and Unicode tag characters (an invisible
/// alphabet that smuggles text past the eye). Stripped from user text
/// at the system's entrance, so the database, the transcript, and the
/// prompt all hold the same words. Invisible characters that carry
/// meaning survive: emoji ZWJ sequences, soft hyphens, and variation
/// selectors are left alone.
pub fn strip_invisible(text: &str) -> String {
    fn invisible(c: char) -> bool {
        matches!(c,
            // Bidirectional controls: LRM, RLM, the Arabic letter mark,
            // the embedding/override block, the isolate block.
            '\u{061C}' | '\u{200E}' | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            // Zero-width space, zero-width non-joiner, word joiner,
            // and the byte-order mark read mid-text.
            | '\u{200B}' | '\u{200C}' | '\u{2060}' | '\u{FEFF}'
            // Tag characters: an invisible block of letters.
            | '\u{E0000}'..='\u{E007F}'
        )
    }
    text.chars().filter(|c| !invisible(*c)).collect()
}

/// Who authored a message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Role {
    System,
    #[default]
    User,
    Assistant,
    /// Output produced by a tool invocation.
    Tool,
}

impl Role {
    fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }

    fn from_str(raw: &str) -> Self {
        match raw {
            "system" => Role::System,
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => Role::Tool,
        }
    }
}

/// One node in a session's message tree. Sessions are never flat lists:
/// every message knows its parent, so forking, "retry from here", and
/// subagent transcripts-as-branches need no migration later.
///
/// A message's *canonical* rendering is the path from its node to the root.
#[derive(Clone, Debug)]
pub struct Message {
    pub id: MessageId,
    pub parent_id: Option<MessageId>,
    pub role: Role,
    pub content: String,
    /// Tool rows only: which call this answers.
    pub tool_call_id: Option<String>,
    /// Tool rows only: the tool's name.
    pub tool_name: Option<String>,
    /// Tool rows only: the arguments the model sent.
    pub tool_arguments: Option<String>,
}

/// A follow-up parked while a session's turn runs. Persisted rows, so
/// a queue survives a stop, a crash, or a restart: the transcript
/// renders them as editable drafts, and a drain fires them in order.
#[derive(Clone, Debug)]
pub struct QueuedFollowup {
    pub id: QueuedId,
    pub content: String,
}

/// A summary row for a session, for sidebars and resumption menus.
#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub id: SessionId,
    pub title: String,
    /// Cumulative token totals, persisted from usage events. A resumed
    /// session shows what it spent before, not just since reopen.
    pub input_tokens: u64,
    /// Cache-hit input tokens, as reported by the wire. See the Usage
    /// event's note on per-wire semantics before computing shares.
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    /// The last request's context size (prompt + completion), the live
    /// number the compaction threshold watches. Persisted so a resumed
    /// session shows its distance to the window immediately.
    pub context_tokens: u64,
    /// Settled: the human put the work down. Out of the active list,
    /// never out of the record; opening it un-settles.
    pub settled: bool,
    /// The turn ended on the provider's rate limit: a manual Resume is
    /// the way back in. Survives restarts: the crash-restart affordance
    /// falls out of it. Cleared by resuming or by any new send.
    pub limited: bool,
    /// The session's permission mode, chosen in the composer and kept
    /// here (reopening a chat reopens its posture).
    pub mode: PermissionMode,
    /// The folder this session's tools run in, recorded at first send
    /// (None: never sent, or a row older than the column). Launch
    /// resume matches on it. The store is one global memory, but a
    /// chat opened in a folder is that folder's chat.
    pub workspace_root: Option<String>,
}

/// One full-text search hit: which session, what role, and a marked
/// snippet (▶…◀ around the match).
#[derive(Clone, Debug)]
pub struct HistoryHit {
    pub session_title: String,
    pub session_id: SessionId,
    pub message_id: MessageId,
    pub role: String,
    pub snippet: String,
}

/// SQLite-backed session persistence. Synchronous on purpose: writes are
/// small, rare, and local; callers decide which thread to block. The
/// connection is mutex-guarded so the store is `Sync`: persistence can
/// then move to any thread (background writes arrive in M3).
pub struct SessionStore {
    connection: std::sync::Mutex<Connection>,
}

impl SessionStore {
    /// Opens (creating if needed) the store at `path`.
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             -- Wait out a concurrent writer (the desktop app and a
             -- headless `gertie run` share this store) instead of
             -- failing the open with SQLITE_BUSY.
             PRAGMA busy_timeout = 5000;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS sessions (
                 id INTEGER PRIMARY KEY,
                 title TEXT NOT NULL,
                 created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE TABLE IF NOT EXISTS messages (
                 id INTEGER PRIMARY KEY,
                 session_id INTEGER NOT NULL REFERENCES sessions(id),
                 parent_id INTEGER REFERENCES messages(id),
                 role TEXT NOT NULL,
                 content TEXT NOT NULL,
                 tool_call_id TEXT,
                 tool_name TEXT,
                 tool_arguments TEXT
             );
             CREATE INDEX IF NOT EXISTS messages_by_session
                 ON messages(session_id, id);
             CREATE TABLE IF NOT EXISTS queued_followups (
                 id INTEGER PRIMARY KEY,
                 session_id INTEGER NOT NULL REFERENCES sessions(id),
                 content TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS queued_followups_by_session
                 ON queued_followups(session_id, id);
             CREATE TABLE IF NOT EXISTS palette_picks (
                 query TEXT NOT NULL,
                 session_id INTEGER NOT NULL REFERENCES sessions(id),
                 picked_count INTEGER NOT NULL DEFAULT 1,
                 picked_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                 PRIMARY KEY (query, session_id)
             );",
        )?;
        // Full-text index over message content (FTS5, bundled SQLite).
        // External-content-free on purpose: messages are append-only
        // and the index mirrors them via trigger. Dead simple.
        connection.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts
                 USING fts5(content, session_id UNINDEXED);
             CREATE TRIGGER IF NOT EXISTS messages_fts_insert
                 AFTER INSERT ON messages BEGIN
                     INSERT INTO messages_fts(rowid, content, session_id)
                     VALUES (new.rowid, new.content, new.session_id);
                 END;",
        )?;
        // Stores predating the index: backfill once, when the index is
        // empty but messages exist. Triggers keep it in sync afterward.
        let indexed: i64 =
            connection.query_row("SELECT count(*) FROM messages_fts", [], |row| row.get(0))?;
        let total: i64 =
            connection.query_row("SELECT count(*) FROM messages", [], |row| row.get(0))?;
        if indexed == 0 && total > 0 {
            connection.execute(
                "INSERT INTO messages_fts(rowid, content, session_id)
                     SELECT rowid, content, session_id FROM messages",
                [],
            )?;
        }
        // Older stores predate the tool columns; add them in place rather
        // than requiring a migration dump.
        let columns: Vec<String> = {
            let mut statement = connection.prepare("PRAGMA table_info(messages)")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (column, definition) in [
            ("tool_call_id", "TEXT"),
            ("tool_name", "TEXT"),
            ("tool_arguments", "TEXT"),
        ] {
            if !columns.contains(&column.to_string()) {
                connection.execute(
                    &format!("ALTER TABLE messages ADD COLUMN {column} {definition}"),
                    [],
                )?;
            }
        }
        // Sessions gain lineage (subagent runs are child sessions) and a
        // kind: chats appear in the sidebar, worker runs stay queryable
        // children. Existing rows are chats; the column defaults to that.
        let session_columns: Vec<String> = {
            let mut statement = connection.prepare("PRAGMA table_info(sessions)")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (column, definition) in [
            ("parent_id", "INTEGER"),
            ("kind", "TEXT DEFAULT 'chat'"),
            ("input_tokens", "INTEGER DEFAULT 0"),
            ("output_tokens", "INTEGER DEFAULT 0"),
            ("last_context_tokens", "INTEGER DEFAULT 0"),
            ("cached_input_tokens", "INTEGER DEFAULT 0"),
            ("settled_at", "TEXT"),
            ("last_activity_at", "TEXT"),
            ("workspace_root", "TEXT"),
            ("limited_at", "TEXT"),
            ("mode", "TEXT NOT NULL DEFAULT 'supervised'"),
            // The session's current tip row: which message the next
            // append parents to. Read paths walk parents back from
            // here (a rewind's abandoned branch, rows never deleted,
            // drops off the walk instead of interleaving).
            ("tip_id", "INTEGER"),
        ] {
            if !session_columns.contains(&column.to_string()) {
                connection.execute(
                    &format!("ALTER TABLE sessions ADD COLUMN {column} {definition}"),
                    [],
                )?;
            }
        }
        // Existing sessions predate the activity clock: "now" is the
        // honest backfill, they were active when this column arrived.
        connection.execute(
            "UPDATE sessions SET last_activity_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE last_activity_at IS NULL",
            [],
        )?;
        // Indexes over the migrated session columns (they must exist
        // before these can be built). `sessions_by_parent` reads a
        // dispatch's runs by parent; the settle sweep looks for old,
        // unsettled chats. Both scanned the whole table per open
        // (and the sweep, on every settle check) before this. The
        // partial index matches the sweep's own WHERE: it holds only
        // the rows the sweep can ever pick.
        connection.execute_batch(
            "CREATE INDEX IF NOT EXISTS sessions_by_parent
                 ON sessions(parent_id, last_activity_at);
             CREATE INDEX IF NOT EXISTS sessions_unsettled_by_activity
                 ON sessions(last_activity_at) WHERE settled_at IS NULL;",
        )?;
        // Memory's sidecar tables (the always-layer's index). No
        // triggers: the sync pass owns these rows in batches.
        crate::memory::migrate(&connection)?;
        Ok(Self {
            connection: std::sync::Mutex::new(connection),
        })
    }

    /// Runs one closure with the store's connection. Memory's sidecar
    /// needs raw access (batched rewrites, FTS maintenance); the lock
    /// stays inside, callers never hold it across awaits.
    pub fn with_connection<T>(&self, run: impl FnOnce(&rusqlite::Connection) -> T) -> T {
        let connection = self.connection.lock().unwrap();
        run(&connection)
    }

    /// Creates a session titled from its first user message (callers may
    /// retitle later).
    pub fn create_session(&self, title: &str) -> Result<SessionId, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        connection
            .prepare("INSERT INTO sessions (title, kind) VALUES (?1, 'chat')")?
            .execute([title])?;
        Ok(SessionId(connection.last_insert_rowid() as u64))
    }

    /// Creates a worker-run session: kind `worker`, linked to the
    /// dispatching chat when the front's scope knows it. Worker runs do
    /// not appear in the sidebar; `list_child_sessions` reads them back.
    pub fn create_worker_run(
        &self,
        parent: Option<SessionId>,
        title: &str,
    ) -> Result<SessionId, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        connection
            .prepare("INSERT INTO sessions (title, kind, parent_id) VALUES (?1, 'worker', ?2)")?
            .execute(rusqlite::params![
                title,
                parent.map(|session| session.0 as i64)
            ])?;
        Ok(SessionId(connection.last_insert_rowid() as u64))
    }

    /// A chat session's worker runs, oldest first.
    pub fn list_child_sessions(
        &self,
        parent: SessionId,
    ) -> Result<Vec<SessionSummary>, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        let mut statement = connection
            .prepare("SELECT s.id, s.title, s.input_tokens, s.output_tokens, s.cached_input_tokens, s.last_context_tokens, s.mode, s.limited_at IS NOT NULL, s.workspace_root FROM sessions s WHERE s.parent_id = ?1 ORDER BY s.id")?;
        let rows = statement
            .query_map([parent.0 as i64], |row| {
                Ok(SessionSummary {
                    id: SessionId(row.get::<_, i64>(0)? as u64),
                    title: row.get(1)?,
                    input_tokens: row.get::<_, i64>(2)?.max(0) as u64,
                    output_tokens: row.get::<_, i64>(3)?.max(0) as u64,
                    cached_input_tokens: row.get::<_, i64>(4)?.max(0) as u64,
                    context_tokens: row.get::<_, i64>(5)?.max(0) as u64,
                    // Child sessions stay out of the sidebar's
                    // lifecycle machine (settled/open lists); the
                    // limited flag stays queryable: a stopped run
                    // waits for the human.
                    settled: false,
                    limited: row.get::<_, i64>(7)? != 0,
                    mode: row
                        .get::<_, String>(6)
                        .ok()
                        .and_then(|raw| raw.parse::<PermissionMode>().ok())
                        .unwrap_or_default(),
                    workspace_root: row.get::<_, Option<String>>(8).ok().flatten(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Whether a worker run has its report yet: any assistant row means
    /// the run finished (or failed loudly): the panel's status.
    pub fn worker_run_reported(&self, session_id: SessionId) -> bool {
        let connection = self.connection.lock().unwrap();
        connection
            .prepare("SELECT 1 FROM messages WHERE session_id = ?1 AND role = 'assistant' LIMIT 1")
            .and_then(|mut statement| statement.query_row([session_id.0 as i64], |_| Ok(())))
            .is_ok()
    }

    /// Adds a request's usage to the session's running totals, and
    /// remembers the request's context size (prompt + completion, what
    /// the next request carries). One call per Usage event, a single
    /// UPDATE, not a per-token write.
    pub fn add_usage(
        &self,
        session: SessionId,
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
    ) -> Result<(), rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        connection
            .prepare(
                "UPDATE sessions SET input_tokens = input_tokens + ?2,
                        output_tokens = output_tokens + ?3,
                        cached_input_tokens = cached_input_tokens + ?4,
                        last_context_tokens = ?2 + ?3 WHERE id = ?1",
            )?
            .execute(rusqlite::params![
                session.0 as i64,
                input_tokens as i64,
                output_tokens as i64,
                cached_input_tokens as i64
            ])?;
        Ok(())
    }

    /// Token totals across every session (chats and worker runs). The
    /// settings panel's bottom line: what gertie has cost, in tokens.
    pub fn total_usage(&self) -> (u64, u64, u64) {
        let connection = self.connection.lock().unwrap();
        connection
            .query_row(
                "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                       COALESCE(SUM(cached_input_tokens), 0)
                FROM sessions",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?.max(0) as u64,
                        row.get::<_, i64>(1)?.max(0) as u64,
                        row.get::<_, i64>(2)?.max(0) as u64,
                    ))
                },
            )
            .unwrap_or((0, 0, 0))
    }

    /// The sidebar as an inbox: the session whose record moved most
    /// recently first, "ordered by when each thread last came back to
    /// you" (t3code's threads list).
    /// Message ids are monotonic across all sessions, so MAX(messages.id)
    /// orders by last activity; sessions with no messages yet sink
    /// below every active one and fall back to creation order among
    /// themselves.
    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        let mut statement = connection.prepare(
            "SELECT s.id, s.title, s.input_tokens, s.output_tokens, s.cached_input_tokens,
                    s.last_context_tokens,
                    s.settled_at IS NOT NULL, s.limited_at IS NOT NULL, s.mode, s.workspace_root
             FROM sessions s
             LEFT JOIN messages m ON m.session_id = s.id
             WHERE s.kind = 'chat'
             GROUP BY s.id
             ORDER BY MAX(m.id) DESC, s.id DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(SessionSummary {
                id: SessionId(row.get::<_, i64>(0)? as u64),
                title: row.get(1)?,
                input_tokens: row.get::<_, i64>(2)?.max(0) as u64,
                output_tokens: row.get::<_, i64>(3)?.max(0) as u64,
                cached_input_tokens: row.get::<_, i64>(4)?.max(0) as u64,
                context_tokens: row.get::<_, i64>(5)?.max(0) as u64,
                settled: row.get::<_, i64>(6)? != 0,
                limited: row.get::<_, i64>(7)? != 0,
                mode: row
                    .get::<_, String>(8)
                    .ok()
                    .and_then(|raw| raw.parse::<PermissionMode>().ok())
                    .unwrap_or_default(),
                workspace_root: row.get::<_, Option<String>>(9).ok().flatten(),
            })
        })?;
        rows.collect()
    }

    /// Settles or un-settles a session: the human put the work down
    /// (or picked it back up). A timestamp, not a bool: when auto-settle
    /// arrives it needs to know how long the session has been resting.
    pub fn set_session_settled(
        &self,
        session_id: SessionId,
        settled: bool,
    ) -> Result<(), rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        connection.execute(
            "UPDATE sessions SET settled_at = CASE WHEN ?2
                THEN strftime('%Y-%m-%dT%H:%M:%fZ','now') ELSE NULL END
             WHERE id = ?1",
            rusqlite::params![session_id.0 as i64, settled],
        )?;
        Ok(())
    }

    /// Marks (or clears) a session's rate-limited state: the turn ended
    /// on the provider's limit and a manual Resume is the way back in.
    /// Cleared by resuming, or by any new send.
    pub fn set_session_limited(
        &self,
        session_id: SessionId,
        limited: bool,
    ) -> Result<(), rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        connection.execute(
            "UPDATE sessions SET limited_at = CASE WHEN ?2
                THEN strftime('%Y-%m-%dT%H:%M:%fZ','now') ELSE NULL END
             WHERE id = ?1",
            rusqlite::params![session_id.0 as i64, limited],
        )?;
        Ok(())
    }

    /// Persists a session's permission mode. Applies from the next
    /// turn on (a running turn keeps the mode it was built with).
    pub fn set_session_mode(
        &self,
        session_id: SessionId,
        mode: PermissionMode,
    ) -> Result<(), rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        connection.execute(
            "UPDATE sessions SET mode = ?2 WHERE id = ?1",
            rusqlite::params![session_id.0 as i64, mode.as_str()],
        )?;
        Ok(())
    }

    /// Records the folder this session's tools run in: the chat sets
    /// it at the first send and re-roots it on any send after a
    /// footer workspace switch. The settle hook reads it back to run
    /// a reclaim command in the right checkout.
    pub fn set_session_workspace(&self, session_id: SessionId, root: &Path) {
        let connection = self.connection.lock().unwrap();
        let _ = connection.execute(
            "UPDATE sessions SET workspace_root = ?2 WHERE id = ?1",
            rusqlite::params![session_id.0 as i64, root.to_string_lossy()],
        );
    }

    /// The folder recorded for a session, if any. Sessions older than
    /// the column return None: there is no root to reclaim in.
    pub fn session_workspace(&self, session_id: SessionId) -> Option<String> {
        let connection = self.connection.lock().unwrap();
        connection
            .query_row(
                "SELECT workspace_root FROM sessions WHERE id = ?1",
                [session_id.0 as i64],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
    }

    /// Auto-settle: lays every chat idle for at least `idle_days` down
    /// as settled: the sweep on startup, before any view loads. Only
    /// state changes here: a `settle_command` runs on manual settle,
    /// where a human pressed the button, never from this sweep.
    /// Returns how many sessions settled.
    pub fn settle_idle_sessions(&self, idle_days: u64) -> Result<u64, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        // ISO timestamps compare lexicographically: the cutoff is the
        // same format, computed server-side (SQLite) from the same
        // clock the writes use.
        let settled = connection.execute(
            "UPDATE sessions SET settled_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE kind = 'chat' AND settled_at IS NULL
               AND last_activity_at IS NOT NULL
               AND last_activity_at <
                   strftime('%Y-%m-%dT%H:%M:%fZ','now','-' || ?1 || ' days')",
            [idle_days.to_string()],
        )?;
        Ok(settled as u64)
    }

    /// Full-text search over every message ever recorded: the model's
    /// memory of past sessions. `query` is a plain phrase: it is quoted
    /// for FTS5, so model-written punctuation can't inject syntax.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<HistoryHit>, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        let quoted = format!("\"{}\"", query.replace('"', "\"\""));
        let mut statement = connection.prepare(
            "SELECT sessions.title, messages.session_id, messages.id,
                    messages.role, snippet(messages_fts, 0, '▶', '◀', '…', 16)
             FROM messages_fts
             JOIN messages ON messages.rowid = messages_fts.rowid
             JOIN sessions ON sessions.id = messages.session_id
             WHERE messages_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
        )?;
        let rows = statement.query_map(rusqlite::params![quoted, limit as i64], |row| {
            Ok(HistoryHit {
                session_title: row.get(0)?,
                session_id: SessionId(row.get::<_, i64>(1)? as u64),
                message_id: MessageId(row.get::<_, i64>(2)? as u64),
                role: row.get(3)?,
                snippet: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    /// Remembers a palette confirm: this query led to this session.
    /// The MRU table behind the palette's ranking: the thread you
    /// last picked for a query ranks higher when you type that query
    /// again. An upsert: repeat picks bump recency and the count.
    pub fn record_palette_pick(&self, query: &str, session_id: SessionId) {
        let _ = self.connection.lock().unwrap().execute(
            "INSERT INTO palette_picks (query, session_id) VALUES (?1, ?2)
             ON CONFLICT (query, session_id)
             DO UPDATE SET picked_count = picked_count + 1,
                           picked_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
            rusqlite::params![query, session_id.0 as i64],
        );
    }

    /// Sessions previously picked for this exact query, most-recently
    /// picked first: the palette's boost order. Empty for a query
    /// that never led anywhere.
    pub fn palette_boosts(&self, query: &str) -> Vec<SessionId> {
        self.connection
            .lock()
            .unwrap()
            .prepare(
                "SELECT session_id FROM palette_picks WHERE query = ?1
                 ORDER BY picked_at DESC, picked_count DESC",
            )
            .and_then(|mut statement| {
                statement
                    .query_map([query], |row| Ok(SessionId(row.get::<_, i64>(0)? as u64)))?
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The session's own record as full-text hits, oldest first: what
    /// a compaction's selection points the model back to. A query
    /// scopes to full-text matches inside this session (quoted for
    /// FTS5, same as `search`); without one, the last `limit` messages
    /// come back in order, snippet field carrying the whole content.
    /// Read-only over the rows every surface already sees.
    pub fn read_session(
        &self,
        session_id: SessionId,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<HistoryHit>, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        match query {
            None => {
                let mut statement = connection.prepare(
                    "SELECT sessions.title, messages.session_id, messages.id,
                            messages.role, messages.content
                     FROM messages
                     JOIN sessions ON sessions.id = messages.session_id
                     WHERE messages.session_id = ?1
                     ORDER BY messages.id DESC LIMIT ?2",
                )?;
                let rows = statement.query_map(
                    rusqlite::params![session_id.0 as i64, limit as i64],
                    Self::hit_row,
                )?;
                let mut hits: Vec<HistoryHit> = rows.collect::<Result<_, _>>()?;
                hits.reverse(); // conversation order, oldest first
                Ok(hits)
            }
            Some(query) => {
                let quoted = format!("\"{}\"", query.replace('"', "\"\""));
                let mut statement = connection.prepare(
                    "SELECT sessions.title, messages.session_id, messages.id,
                            messages.role, snippet(messages_fts, 0, '▶', '◀', '…', 24)
                     FROM messages_fts
                     JOIN messages ON messages.rowid = messages_fts.rowid
                     JOIN sessions ON sessions.id = messages.session_id
                     WHERE messages_fts MATCH ?1 AND messages.session_id = ?2
                     ORDER BY messages.id LIMIT ?3",
                )?;
                let rows = statement.query_map(
                    rusqlite::params![quoted, session_id.0 as i64, limit as i64],
                    Self::hit_row,
                )?;
                rows.collect()
            }
        }
    }

    fn hit_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryHit> {
        Ok(HistoryHit {
            session_title: row.get(0)?,
            session_id: SessionId(row.get::<_, i64>(1)? as u64),
            message_id: MessageId(row.get::<_, i64>(2)? as u64),
            role: row.get(3)?,
            snippet: row.get(4)?,
        })
    }

    /// Appends a message under `parent_id` (None = root-level turn).
    pub fn append_message(
        &self,
        session_id: SessionId,
        parent_id: Option<MessageId>,
        role: Role,
        content: &str,
    ) -> Result<MessageId, rusqlite::Error> {
        let mut connection = self.connection.lock().unwrap();
        // One transaction: a row whose tip and activity stamps land
        // separately is a half-written turn. A reader that walks
        // parents from the tip between the commits sees a different
        // conversation than the next one.
        let transaction = connection.transaction()?;
        transaction
            .prepare(
                "INSERT INTO messages (session_id, parent_id, role, content) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(rusqlite::params![
                session_id.0 as i64,
                parent_id.map(|id| id.0 as i64),
                role.as_str(),
                content,
            ])?;
        // Every appended row is activity: the clock auto-settle reads.
        transaction.execute(
            "UPDATE sessions SET last_activity_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id = ?1",
            [session_id.0 as i64],
        )?;
        let id = MessageId(transaction.last_insert_rowid() as u64);
        // The new row is the tip: reads walk parents back from here.
        transaction.execute(
            "UPDATE sessions SET tip_id = ?1 WHERE id = ?2",
            rusqlite::params![id.0 as i64, session_id.0 as i64],
        )?;
        transaction.commit()?;
        Ok(id)
    }

    /// Appends a tool result: the row that answers one tool call.
    pub fn append_tool_result(
        &self,
        session_id: SessionId,
        parent_id: Option<MessageId>,
        call_id: &str,
        tool_name: &str,
        arguments: &str,
        content: &str,
    ) -> Result<MessageId, rusqlite::Error> {
        let mut connection = self.connection.lock().unwrap();
        // Same transaction discipline as append_message: the row, its
        // activity stamp, and the tip move together.
        let transaction = connection.transaction()?;
        transaction
            .prepare(
                "INSERT INTO messages (session_id, parent_id, role, content, tool_call_id, tool_name, tool_arguments)
                 VALUES (?1, ?2, 'tool', ?3, ?4, ?5, ?6)",
            )?
            .execute(rusqlite::params![
                session_id.0 as i64,
                parent_id.map(|id| id.0 as i64),
                content,
                call_id,
                tool_name,
                arguments,
            ])?;
        // Tool results are activity too: the turn's work, not just
        // its words.
        transaction.execute(
            "UPDATE sessions SET last_activity_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
             WHERE id = ?1",
            [session_id.0 as i64],
        )?;
        let id = MessageId(transaction.last_insert_rowid() as u64);
        transaction.execute(
            "UPDATE sessions SET tip_id = ?1 WHERE id = ?2",
            rusqlite::params![id.0 as i64, session_id.0 as i64],
        )?;
        transaction.commit()?;
        Ok(id)
    }

    /// Rewrites a message's content in place. This is how streaming turns
    /// land without inserting a row per token.
    pub fn update_message(
        &self,
        message_id: MessageId,
        content: &str,
    ) -> Result<(), rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        connection
            .prepare("UPDATE messages SET content = ?1 WHERE id = ?2")?
            .execute(rusqlite::params![content, message_id.0 as i64])?;
        Ok(())
    }

    /// Retitles a session (e.g. from its first exchange).
    pub fn retitle_session(
        &self,
        session_id: SessionId,
        title: &str,
    ) -> Result<(), rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        connection
            .prepare("UPDATE sessions SET title = ?1 WHERE id = ?2")?
            .execute(rusqlite::params![title, session_id.0 as i64])?;
        Ok(())
    }

    /// Deletes a session, its messages, and their full-text rows: the
    /// FTS index mirrors messages via an insert-only trigger, so its
    /// rows must go by hand or search would keep finding ghosts.
    pub fn delete_session(&self, session_id: SessionId) -> Result<(), rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        let transaction = connection.unchecked_transaction()?;
        transaction.execute(
            "DELETE FROM messages_fts WHERE session_id = ?1",
            [session_id.0 as i64],
        )?;
        transaction.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            [session_id.0 as i64],
        )?;
        transaction.execute(
            "DELETE FROM queued_followups WHERE session_id = ?1",
            [session_id.0 as i64],
        )?;
        transaction.execute("DELETE FROM sessions WHERE id = ?1", [session_id.0 as i64])?;
        transaction.commit()
    }

    /// Parks a follow-up for a session that is mid-turn: a full user
    /// message that runs as its own turn when the current one ends.
    /// Rows, not UI state: a crash keeps the words.
    pub fn enqueue_followup(
        &self,
        session_id: SessionId,
        content: &str,
    ) -> Result<(), rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        connection.execute(
            "INSERT INTO queued_followups (session_id, content) VALUES (?1, ?2)",
            rusqlite::params![session_id.0 as i64, content],
        )?;
        Ok(())
    }

    /// A session's queued follow-ups, oldest-first.
    pub fn queued_followups(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<QueuedFollowup>, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        let mut statement = connection.prepare(
            "SELECT id, content FROM queued_followups WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = statement.query_map([session_id.0 as i64], |row| {
            Ok(QueuedFollowup {
                id: QueuedId(row.get::<_, i64>(0)? as u64),
                content: row.get(1)?,
            })
        })?;
        rows.collect()
    }

    /// Pulls one followup out of the queue, sent by the drain or by a
    /// force-send, returning its text. Unknown id: none.
    pub fn take_followup(&self, id: QueuedId) -> Result<Option<String>, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        let content = connection
            .query_row(
                "SELECT content FROM queued_followups WHERE id = ?1",
                [id.0 as i64],
                |row| row.get::<_, String>(0),
            )
            .ok();
        if content.is_some() {
            connection.execute("DELETE FROM queued_followups WHERE id = ?1", [id.0 as i64])?;
        }
        Ok(content)
    }

    /// Removes a queued followup without returning it (the UI's remove
    /// action). True when a row went.
    pub fn remove_followup(&self, id: QueuedId) -> Result<bool, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        let gone =
            connection.execute("DELETE FROM queued_followups WHERE id = ?1", [id.0 as i64])?;
        Ok(gone > 0)
    }

    /// A message row's parent: the keep-point an "edit from here"
    /// rewind restores the conversation to. `None` for a session's
    /// first row.
    pub fn message_parent(
        &self,
        message_id: MessageId,
    ) -> Result<Option<MessageId>, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        connection.query_row(
            "SELECT parent_id FROM messages WHERE id = ?1",
            [message_id.0 as i64],
            |row| {
                row.get::<_, Option<i64>>(0)
                    .map(|parent| parent.map(|parent| MessageId(parent as u64)))
            },
        )
    }

    /// Loads a session's messages oldest-first along the canonical
    /// path. Unbounded by design: the wire history, export, and fork
    /// need every row: the UI transcript uses the bounded
    /// `load_recent_messages` instead.
    pub fn load_messages(&self, session_id: SessionId) -> Result<Vec<Message>, rusqlite::Error> {
        let rows = self.load_rows_flat(session_id)?;
        let connection = self.connection.lock().unwrap();
        let order = Self::canonical_ids(&rows, &connection, session_id)?;
        let mut by_id: std::collections::HashMap<i64, Message> =
            rows.into_iter().map(|row| (row.id.0 as i64, row)).collect();
        Ok(order
            .into_iter()
            .map(|id| {
                by_id
                    .remove(&id)
                    .expect("canonical id comes from these rows")
            })
            .collect())
    }

    /// Every row of a session by id, no path, no order beyond id. The
    /// raw table: what `load_messages` reorders, and what fork reads
    /// (a fork may start from an off-path sibling).
    fn load_rows_flat(&self, session_id: SessionId) -> Result<Vec<Message>, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        let mut statement = connection.prepare(
            "SELECT id, parent_id, role, content, tool_call_id, tool_name, tool_arguments
             FROM messages WHERE session_id = ?1",
        )?;
        let rows = statement.query_map([session_id.0 as i64], Self::message_from_row)?;
        rows.collect()
    }

    /// The session's message ids along the canonical path, oldest
    /// first. The path walks parents back from the session's tip (the
    /// recorded `tip_id`, else its newest row), so after a rewind,
    /// the abandoned branch stays in the table (an append-only audit)
    /// while dropping off every read. A tool result hangs off the
    /// assistant row it answers (the conversation continues from the
    /// assistant), so each on-path assistant carries its own tool rows
    /// with it. A walk that can't be trusted (legacy stores written
    /// before parents were tracked, every row a root, or a corrupted
    /// chain) falls back to flat id order, which is what those rows
    /// mean.
    fn canonical_ids(
        rows: &[Message],
        connection: &Connection,
        session_id: SessionId,
    ) -> Result<Vec<i64>, rusqlite::Error> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let tip: Option<i64> = connection
            .query_row(
                "SELECT tip_id FROM sessions WHERE id = ?1",
                [session_id.0 as i64],
                |row| row.get(0),
            )
            .ok()
            .flatten()
            .filter(|tip| *tip > 0)
            .or_else(|| rows.iter().map(|row| row.id.0 as i64).max());
        let parents: std::collections::HashMap<i64, Option<i64>> = rows
            .iter()
            .map(|row| (row.id.0 as i64, row.parent_id.map(|parent| parent.0 as i64)))
            .collect();
        // Tool children by their answering assistant, in id order.
        let mut tool_children: std::collections::HashMap<i64, Vec<i64>> =
            std::collections::HashMap::new();
        for row in rows {
            if row.role == Role::Tool
                && let Some(parent) = row.parent_id
            {
                tool_children
                    .entry(parent.0 as i64)
                    .or_default()
                    .push(row.id.0 as i64);
            }
        }
        // The ancestor chain, tip → root.
        let mut chain = Vec::with_capacity(rows.len());
        let mut cursor = tip;
        let mut broken = false;
        while let Some(id) = cursor {
            chain.push(id);
            match parents.get(&id) {
                // Cycle insurance: a corrupted loop can't outgrow the
                // table.
                Some(parent) if chain.len() <= rows.len() => cursor = *parent,
                _ => {
                    broken = true;
                    break;
                }
            }
        }
        // One tree, one truth: if the walk from the tip reaches a root
        // through resolvable parents, that walk is the canonical path
        // (abandoned branches and all; a rewind-to-start even grows a
        // second root, and the tip says which branch is live). A store
        // too old to have parents (no row links to anything) or a
        // corrupted chain reads flat id order, which is what those
        // rows mean.
        let linked = rows.iter().any(|row| row.parent_id.is_some());
        if broken || !linked {
            let mut flat: Vec<i64> = rows.iter().map(|row| row.id.0 as i64).collect();
            flat.sort_unstable();
            return Ok(flat);
        }
        // Root → tip, with each assistant's tool rows riding after it:
        // the sequence the wire pairing reads.
        chain.reverse();
        let placed: std::collections::HashSet<i64> = chain.iter().copied().collect();
        let mut path = Vec::with_capacity(rows.len());
        for id in chain {
            path.push(id);
            if let Some(children) = tool_children.get(&id) {
                for child in children {
                    // A tool row the chain itself runs through (a turn
                    // that continued from the tool result) is placed by
                    // the chain; the rest ride with their assistant.
                    if !placed.contains(child) {
                        path.push(*child);
                    }
                }
            }
        }
        Ok(path)
    }

    /// Loads the most recent `limit` messages of a session, oldest-first:
    /// the bounded window a transcript renders. Opening a chat touches
    /// one page of the canonical path, not the whole archive: the
    /// parent links (two integer columns) walk the path, then only the
    /// window's rows come back with their content.
    pub fn load_recent_messages(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<Vec<Message>, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        let path = Self::canonical_ids(
            &self.light_rows(&connection, session_id)?,
            &connection,
            session_id,
        )?;
        // The window is the path's tail, oldest first, as loaded.
        let skip = path.len().saturating_sub(limit);
        let window: Vec<i64> = path[skip..].to_vec();
        Self::rows_for_ids(&connection, session_id, &window)
    }

    /// Loads up to `limit` messages strictly older than `before_id`
    /// along the canonical path, oldest-first: one page of "load
    /// earlier" in the transcript. A `before_id` no longer on the path
    /// (its branch was rewound away) pages nothing: the caller retires
    /// the stale control.
    pub fn load_messages_before(
        &self,
        session_id: SessionId,
        before_id: MessageId,
        limit: usize,
    ) -> Result<Vec<Message>, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        let path = Self::canonical_ids(
            &self.light_rows(&connection, session_id)?,
            &connection,
            session_id,
        )?;
        let cut = match path.iter().position(|id| *id == before_id.0 as i64) {
            Some(cut) => cut,
            None => return Ok(Vec::new()),
        };
        let start = cut.saturating_sub(limit);
        let window: Vec<i64> = path[start..cut].to_vec();
        Self::rows_for_ids(&connection, session_id, &window)
    }

    /// A session's rows, light of their content: only the two columns
    /// the parent walk needs.
    fn light_rows(
        &self,
        connection: &Connection,
        session_id: SessionId,
    ) -> Result<Vec<Message>, rusqlite::Error> {
        let mut statement =
            connection.prepare("SELECT id, parent_id FROM messages WHERE session_id = ?1")?;
        let rows = statement.query_map([session_id.0 as i64], |row| {
            Ok(Message {
                id: MessageId(row.get::<_, i64>(0)? as u64),
                parent_id: row
                    .get::<_, Option<i64>>(1)?
                    .map(|parent| MessageId(parent as u64)),
                role: Role::System,
                content: String::new(),
                tool_call_id: None,
                tool_name: None,
                tool_arguments: None,
            })
        })?;
        rows.collect()
    }

    /// Full rows for a window of ids, returned along the window's
    /// order.
    fn rows_for_ids(
        connection: &Connection,
        session_id: SessionId,
        ids: &[i64],
    ) -> Result<Vec<Message>, rusqlite::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let list = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let mut statement = connection.prepare(&format!(
            "SELECT id, parent_id, role, content, tool_call_id, tool_name, tool_arguments
             FROM messages WHERE session_id = ? AND id IN ({list})"
        ))?;
        let rows = statement.query_map(
            rusqlite::params_from_iter(
                std::iter::once(session_id.0 as i64).chain(ids.iter().copied()),
            ),
            Self::message_from_row,
        )?;
        let mut by_id: std::collections::HashMap<i64, Message> = rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|row| (row.id.0 as i64, row))
            .collect();
        Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
    }

    fn message_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
        Ok(Message {
            id: MessageId(row.get::<_, i64>(0)? as u64),
            parent_id: row.get::<_, Option<i64>>(1)?.map(|id| MessageId(id as u64)),
            role: Role::from_str(&row.get::<_, String>(2)?),
            content: row.get(3)?,
            tool_call_id: row.get(4)?,
            tool_name: row.get(5)?,
            tool_arguments: row.get(6)?,
        })
    }

    /// Reconstructs the canonical wire history from stored rows: assistant
    /// rows re-attach the tool calls their child tool rows answer, so a
    /// resumed session requests with exactly what the model saw before.
    pub fn load_wire_history(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<crate::request::WireMessage>, rusqlite::Error> {
        let messages = self.load_messages(session_id)?;
        Ok(Self::wire_history_from_rows(&messages))
    }

    /// The wire history for already-loaded rows: the reconstruction
    /// `load_wire_history` runs over the canonical path, and what a
    /// rewind uses to rebuild its kept prefix (rows at or before the
    /// keep point are identical on every branch).
    pub fn wire_history_from_rows(messages: &[Message]) -> Vec<crate::request::WireMessage> {
        use crate::request::{ToolCall, WireMessage};
        let mut wire = Vec::new();
        let mut index = 0;
        while index < messages.len() {
            let message = &messages[index];
            // Image markers ride inside stored content; expand them back
            // into wire images so a resumed session re-sends what the
            // model saw. A missing file degrades to a text note: the
            // conversation keeps working without the bytes.
            let (content, images) = {
                let (text, paths) = crate::request::extract_image_markers(&message.content);
                let images = paths
                    .iter()
                    .map(|path| {
                        crate::request::file_to_data_url(std::path::Path::new(path))
                            .unwrap_or_else(|| "[missing image]".to_string())
                    })
                    .collect::<Vec<_>>();
                (text, images)
            };
            match message.role {
                Role::Assistant => {
                    // Tool rows answering this assistant turn follow it.
                    let mut calls = Vec::new();
                    let mut next = index + 1;
                    while next < messages.len()
                        && messages[next].role == Role::Tool
                        && messages[next].parent_id == Some(message.id)
                    {
                        let row = &messages[next];
                        calls.push(ToolCall {
                            id: row.tool_call_id.clone().unwrap_or_default(),
                            name: row.tool_name.clone().unwrap_or_default(),
                            arguments: row.tool_arguments.clone().unwrap_or_default(),
                        });
                        next += 1;
                    }
                    wire.push(WireMessage::assistant_with_tool_calls(
                        (!content.is_empty()).then_some(content),
                        calls,
                    ));
                    for row in &messages[index + 1..next] {
                        wire.push(WireMessage::tool_result(
                            &ToolCall {
                                id: row.tool_call_id.clone().unwrap_or_default(),
                                name: row.tool_name.clone().unwrap_or_default(),
                                arguments: row.tool_arguments.clone().unwrap_or_default(),
                            },
                            &row.content,
                        ));
                    }
                    index = next;
                }
                Role::User => {
                    wire.push(WireMessage::user_with_attachments(content, images));
                    index += 1;
                }
                Role::System => {
                    wire.push(WireMessage {
                        role: Role::System,
                        content: Some(content),
                        ..WireMessage::default()
                    });
                    index += 1;
                }
                Role::Tool => index += 1, // orphan row: skip, don't guess
            }
        }
        wire
    }

    /// Forks the conversation at `upto`: a new session whose messages
    /// are the canonical path root → `upto`, copied verbatim (tool
    /// fields ride along; parents are remapped to the new ids). The
    /// parent_id column planned for this all along: a fork is a
    /// sibling branch, and the source session is untouched. Returns
    /// the new session id and the copy of `upto` (the fork point, which
    /// becomes the view's last message).
    pub fn fork_session(
        &self,
        source: SessionId,
        upto: MessageId,
        title: &str,
    ) -> Result<(SessionId, MessageId), String> {
        let messages = self
            .load_rows_flat(source)
            .map_err(|error| error.to_string())?;
        if !messages.iter().any(|message| message.id == upto) {
            return Err(format!("message {} is not in session {}", upto.0, source.0));
        }

        // The canonical path: walk parents from the fork point to the
        // root, then copy in root-first order so every parent's new id
        // exists before its child references it.
        let mut chain = Vec::new();
        let mut cursor = Some(upto);
        while let Some(id) = cursor {
            let message = messages
                .iter()
                .find(|message| message.id == id)
                .ok_or_else(|| format!("message {}: parent not in session", id.0))?;
            chain.push(message.clone());
            cursor = message.parent_id;
        }
        chain.reverse();

        // One transaction: the fork exists whole or not at all. A
        // half-copied fork (a session with some ancestors) is neither
        // a transcript nor a lie a reader could spot. This is also the
        // fork's speedup: the copies share one lock and one commit
        // instead of two per message.
        let mut connection = self.connection.lock().unwrap();
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        transaction
            .prepare("INSERT INTO sessions (title, kind) VALUES (?1, 'chat')")
            .and_then(|mut statement| statement.execute([title]))
            .map_err(|error| error.to_string())?;
        let new_session = SessionId(transaction.last_insert_rowid() as u64);
        let mut remap: std::collections::HashMap<MessageId, MessageId> =
            std::collections::HashMap::new();
        let mut fork_point = upto;
        for message in &chain {
            let mapped_parent = message.parent_id.map(|parent| remap[&parent]);
            transaction
                .prepare(
                    "INSERT INTO messages
                         (session_id, parent_id, role, content, tool_call_id, tool_name, tool_arguments)
                     SELECT ?1, ?2, role, content, tool_call_id, tool_name, tool_arguments
                     FROM messages WHERE id = ?3",
                )
                .and_then(|mut statement| {
                    statement.execute(rusqlite::params![
                        new_session.0 as i64,
                        mapped_parent.map(|parent| parent.0 as i64),
                        message.id.0 as i64,
                    ])
                })
                .map_err(|error| error.to_string())?;
            let copied = MessageId(transaction.last_insert_rowid() as u64);
            remap.insert(message.id, copied);
            fork_point = copied;
        }
        // The fork's tip is its last copied row.
        transaction
            .execute(
                "UPDATE sessions SET tip_id = ?1 WHERE id = ?2",
                rusqlite::params![fork_point.0 as i64, new_session.0 as i64],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok((new_session, fork_point))
    }

    /// Exports a session's messages as JSONL, one object per row,
    /// oldest first. The shape mirrors what the model saw (role,
    /// content, tool fields), which makes the file both a shareable
    /// transcript and a training-shaped trace. Returns the row count.
    pub fn export_session_jsonl(
        &self,
        session_id: SessionId,
        path: &Path,
    ) -> Result<usize, String> {
        let title: String = {
            let connection = self.connection.lock().unwrap();
            connection
                .query_row(
                    "SELECT title FROM sessions WHERE id = ?1",
                    [session_id.0 as i64],
                    |row| row.get(0),
                )
                .map_err(|error| format!("session {}: {error}", session_id.0))?
        };
        let messages = self
            .load_messages(session_id)
            .map_err(|error| error.to_string())?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        let file =
            std::fs::File::create(path).map_err(|error| format!("{}: {error}", path.display()))?;
        let mut out = std::io::BufWriter::new(file);
        use std::io::Write as _;
        for message in &messages {
            let row = serde_json::json!({
                "session_id": session_id.0,
                "session_title": title,
                "role": message.role.as_str(),
                "content": message.content,
                "tool_call_id": message.tool_call_id,
                "tool_name": message.tool_name,
                "tool_arguments": message.tool_arguments,
            });
            serde_json::to_writer(&mut out, &row)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            out.write_all(b"\n")
                .map_err(|error| format!("{}: {error}", path.display()))?;
        }
        Ok(messages.len())
    }
}

/// A single event on a provider's response stream. This enum is gertie's
/// stable internal contract: every provider adapts its wire format to it,
/// and neither the agent loop nor the UI ever sees vendor-shaped deltas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The provider accepted the request and is about to stream.
    Started,
    /// The next piece of assistant text. Providers coalesce to whatever
    /// granularity arrives over the wire; the loop may re-coalesce.
    Token { delta: String },
    /// A tool call the model wants executed. Deltas are normalized inside
    /// the provider layer, so this arrives whole, with arguments parsed.
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// Final usage accounting for the request. `cached_input_tokens`
    /// is as-reported by the wire: Anthropic counts it separately (its
    /// `input_tokens` excludes cache reads); OpenAI-compatible counts
    /// it as a subset of `input_tokens`. Zero when absent. Share math
    /// is the consumer's job, per wire.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
    },
    /// The stream finished normally.
    Done,
    /// The stream failed. Retries are the provider layer's job; by the time
    /// an `Error` surfaces, the policy in DESIGN.md has been exhausted.
    Error { message: String },
    /// The provider's usage/rate limit answered every retry. Not an
    /// error to report and forget: the turn ends *limited*, the state
    /// is persisted, and the UI offers a manual Resume: the ask is
    /// "wait, then continue", not "something broke".
    Limited { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_store() -> SessionStore {
        SessionStore::open(Path::new(":memory:")).expect("open in-memory store")
    }

    /// The palette's pick ledger: re-picking a session bumps it to the
    /// front of that query's boosts; other queries answer empty.
    #[test]
    fn palette_picks_rank_the_last_picked_session_first() {
        let store = memory_store();
        let one = store.create_session("one").unwrap();
        let two = store.create_session("two").unwrap();
        store.record_palette_pick("fix", one);
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.record_palette_pick("fix", two);
        assert_eq!(store.palette_boosts("fix"), vec![two, one]);
        assert!(store.palette_boosts("other").is_empty());
        // Picking the older one again flips the order back.
        store.record_palette_pick("fix", one);
        assert_eq!(store.palette_boosts("fix"), vec![one, two]);
    }

    /// After a rewind, the abandoned branch (rows are never deleted,
    /// an append-only audit) drops off every read: the canonical path
    /// walks parents back from the tip, and the tip advanced onto the
    /// new branch the moment the re-sent message landed.
    #[test]
    fn rewound_history_reads_the_new_branch_and_keeps_the_old_rows() {
        let store = memory_store();
        let session = store.create_session("rewind").unwrap();
        let q1 = store
            .append_message(session, None, Role::User, "q1")
            .unwrap();
        let a1 = store
            .append_message(session, Some(q1), Role::Assistant, "a1")
            .unwrap();
        let q2 = store
            .append_message(session, Some(a1), Role::User, "q2")
            .unwrap();
        store
            .append_message(session, Some(q2), Role::Assistant, "a2")
            .unwrap();

        // Edit from here on q2: the keep point is q2's parent (a1); the
        // next send parents there and the tip advances onto the new
        // branch.
        let q2_fixed = store
            .append_message(session, Some(a1), Role::User, "q2 fixed")
            .unwrap();
        store
            .append_message(session, Some(q2_fixed), Role::Assistant, "a3")
            .unwrap();

        let path = store.load_messages(session).unwrap();
        let contents: Vec<&str> = path.iter().map(|row| row.content.as_str()).collect();
        assert_eq!(contents, vec!["q1", "a1", "q2 fixed", "a3"]);

        // The wire history a resumed session re-sends (no stale q2).
        let wire = store.load_wire_history(session).unwrap();
        let wire_texts: Vec<String> = wire
            .iter()
            .filter_map(|message| message.content.clone())
            .collect();
        assert_eq!(wire_texts, vec!["q1", "a1", "q2 fixed", "a3"]);

        // The bounded page and "load earlier" both follow the path.
        let recent = store.load_recent_messages(session, 3).unwrap();
        let recent_texts: Vec<&str> = recent.iter().map(|row| row.content.as_str()).collect();
        assert_eq!(recent_texts, vec!["a1", "q2 fixed", "a3"]);
        let earlier = store
            .load_messages_before(session, q2_fixed, 10)
            .unwrap()
            .into_iter()
            .map(|row| row.content)
            .collect::<Vec<_>>();
        assert_eq!(earlier, vec!["q1", "a1"]);
        // A before-id off the path (the abandoned branch) pages nothing.
        assert!(
            store
                .load_messages_before(session, MessageId(q2.0 + 1), 10)
                .unwrap()
                .is_empty()
        );

        // The audit: nothing was deleted.
        let total: i64 = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                [session.0 as i64],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(total, 6);
    }

    /// Stores written before parents were tracked hold linear history
    /// with NULL parents: the walk can't cover them, so flat id order
    /// (what those rows mean) is the fallback.
    #[test]
    fn legacy_rows_without_parents_read_flat() {
        let store = memory_store();
        let session = store.create_session("legacy").unwrap();
        // Simulate the old store: rows landed, then parents were wiped.
        let first = store
            .append_message(session, None, Role::User, "one")
            .unwrap();
        store
            .append_message(session, Some(first), Role::Assistant, "two")
            .unwrap();
        store
            .connection
            .lock()
            .unwrap()
            .execute("UPDATE messages SET parent_id = NULL", [])
            .unwrap();

        let rows = store.load_messages(session).unwrap();
        let contents: Vec<&str> = rows.iter().map(|row| row.content.as_str()).collect();
        assert_eq!(contents, vec!["one", "two"]);
    }

    /// A rewind to the very start parents the next send to nothing:
    /// a second root. The tip still says which branch is live: the
    /// walk reads the new conversation; the old rows stay as audit.
    #[test]
    fn a_rewind_to_the_start_grows_a_second_root_the_tip_still_rules() {
        let store = memory_store();
        let session = store.create_session("from scratch").unwrap();
        let q1 = store
            .append_message(session, None, Role::User, "wrong start")
            .unwrap();
        store
            .append_message(session, Some(q1), Role::Assistant, "answer")
            .unwrap();
        // Edit from here on q1: keep-point is None; the re-sent
        // message is a second root.
        let q1_fixed = store
            .append_message(session, None, Role::User, "right start")
            .unwrap();
        store
            .append_message(session, Some(q1_fixed), Role::Assistant, "answer 2")
            .unwrap();

        let rows = store.load_messages(session).unwrap();
        let contents: Vec<&str> = rows.iter().map(|row| row.content.as_str()).collect();
        assert_eq!(contents, vec!["right start", "answer 2"]);
    }

    /// A fork's copy ends with the fork point as its tip, so the new
    /// session's own canonical walk reproduces what was copied.
    #[test]
    fn a_fork_walks_its_own_path() {
        let store = memory_store();
        let session = store.create_session("source").unwrap();
        let q1 = store
            .append_message(session, None, Role::User, "q1")
            .unwrap();
        let a1 = store
            .append_message(session, Some(q1), Role::Assistant, "a1")
            .unwrap();
        store
            .append_message(session, Some(a1), Role::User, "q2")
            .unwrap();

        let (forked, point) = store.fork_session(session, a1, "fork").unwrap();
        // The returned fork point is the COPY of a1 (a new row in the
        // new session, not the source's row).
        assert_ne!(point, a1);
        let rows = store.load_messages(forked).unwrap();
        let contents: Vec<&str> = rows.iter().map(|row| row.content.as_str()).collect();
        assert_eq!(contents, vec!["q1", "a1"]);
    }

    #[test]
    fn strip_invisible_removes_bidi_zero_width_and_tag_characters() {
        // Zero-width spacing and joiners, the bidi marks, the
        // embeddings/overrides, the isolates, the Arabic letter mark,
        // and the tag alphabet: all vanish.
        let dirty = "pay\u{200B}to\u{2060}x\u{FEFF} \u{200E}a\u{200F}b\u{061C}c";
        assert_eq!(strip_invisible(dirty), "paytox abc");
        let flipped = "1\u{202E}st\u{202C}2 \u{2066}iso\u{2069}";
        assert_eq!(strip_invisible(flipped), "1st2 iso");
        assert_eq!(strip_invisible("ok\u{E0000}\u{E0041}"), "ok");
    }

    #[test]
    fn strip_invisible_leaves_meaningful_invisible_text_alone() {
        // Emoji ZWJ sequences, soft hyphens, and variation selectors
        // carry meaning. Stripping would corrupt the words they shape.
        let alive = "\u{1F468}\u{200D}\u{1F4BB} cau\u{00AD}tion\u{FE0F}";
        assert_eq!(strip_invisible(alive), alive);
        assert_eq!(strip_invisible(""), "");
    }

    #[test]
    fn the_mode_persists_with_the_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::open(&tmp.path().join("store.db")).unwrap();
        let session = store.create_session("chat").unwrap();
        // A fresh session reads the default posture.
        assert_eq!(
            store.list_sessions().unwrap()[0].mode,
            PermissionMode::Supervised
        );

        store
            .set_session_mode(session, PermissionMode::AcceptEdits)
            .unwrap();
        assert_eq!(
            store.list_sessions().unwrap()[0].mode,
            PermissionMode::AcceptEdits
        );

        // A hand-edited row can't name a real mode: it reads as the
        // default rather than inventing a posture.
        let raw = rusqlite::Connection::open(tmp.path().join("store.db")).unwrap();
        raw.execute("UPDATE sessions SET mode = 'weird'", [])
            .unwrap();
        assert_eq!(
            store.list_sessions().unwrap()[0].mode,
            PermissionMode::Supervised
        );
    }

    #[test]
    fn usage_persists_per_session_and_sums_across_all() {
        let store = memory_store();
        let chat = store.create_session("chat").unwrap();
        let worker = store.create_worker_run(Some(chat), "worker").unwrap();

        store.add_usage(chat, 100, 10, 0).unwrap();
        store.add_usage(chat, 40, 5, 0).unwrap();
        store.add_usage(worker, 200, 30, 0).unwrap();

        let chats = store.list_sessions().unwrap();
        let chat_row = chats.iter().find(|summary| summary.id == chat).unwrap();
        assert_eq!((chat_row.input_tokens, chat_row.output_tokens), (140, 15));
        // The last request's size is the live context gauge: prompt +
        // completion of the most recent usage event, not a sum.
        assert_eq!(chat_row.context_tokens, 40 + 5);
        // The all-sessions total includes worker runs too: that's the
        // real cost.
        assert_eq!(store.total_usage(), (340, 45, 0));
    }

    #[test]
    fn cached_input_tokens_persist_and_sum() {
        let store = memory_store();
        let chat = store.create_session("cached").unwrap();

        // Two requests with cache hits, one without: the column
        // accumulates across calls, both the per-session row and the
        // all-sessions tuple surface the sum.
        store.add_usage(chat, 100, 10, 80).unwrap();
        store.add_usage(chat, 100, 10, 90).unwrap();
        store.add_usage(chat, 100, 10, 0).unwrap();

        let row = store
            .list_sessions()
            .unwrap()
            .iter()
            .find(|summary| summary.id == chat)
            .unwrap()
            .clone();
        assert_eq!(row.cached_input_tokens, 170);
        assert_eq!(store.total_usage(), (300, 30, 170));
    }

    #[test]
    fn full_text_search_finds_and_snippets() {
        let store = memory_store();
        let session = store.create_session("compaction debugging").unwrap();
        store
            .append_message(
                session,
                None,
                Role::User,
                "the safe_tail_start slice was wrong",
            )
            .unwrap();
        store
            .append_message(session, None, Role::User, "unrelated weather chat")
            .unwrap();

        let hits = store.search("safe_tail", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_title, "compaction debugging");
        assert!(hits[0].snippet.contains("safe_tail"));
        // Quoting means punctuation can't inject FTS syntax.
        let hits = store.search("\"weird; AND OR (query)", 10).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn read_session_scopes_to_one_session_in_conversation_order() {
        let store = memory_store();
        let session = store.create_session("the one").unwrap();
        let other = store.create_session("the other").unwrap();
        for (role, text) in [
            (Role::User, "first ask"),
            (Role::Assistant, "first answer"),
            (Role::User, "second ask mentions the wug"),
            (Role::Assistant, "second answer"),
        ] {
            store.append_message(session, None, role, text).unwrap();
        }
        store
            .append_message(other, None, Role::User, "another session mentions the wug")
            .unwrap();

        // No query: the newest N, oldest first.
        let hits = store.read_session(session, None, 3).unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].snippet, "first answer");
        assert_eq!(hits[2].snippet, "second answer");

        // A query: matches only from this session, in order, full-text.
        let hits = store.read_session(session, Some("wug"), 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("wug"));

        // An empty record reads as empty, not as an error.
        let empty = store.create_session("empty").unwrap();
        assert!(store.read_session(empty, None, 10).unwrap().is_empty());
    }

    #[test]
    fn sessions_round_trip_through_the_store() {
        let store = memory_store();
        let session = store.create_session("first chat").unwrap();

        let user = store
            .append_message(session, None, Role::User, "hello")
            .unwrap();
        let assistant = store
            .append_message(session, Some(user), Role::Assistant, "")
            .unwrap();
        store.update_message(assistant, "hello back").unwrap();

        let messages = store.load_messages(session).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "hello");
        assert_eq!(messages[0].parent_id, None);
        assert_eq!(messages[1].parent_id, Some(user));
        assert_eq!(messages[1].content, "hello back");

        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session);
        assert_eq!(sessions[0].title, "first chat");
    }

    #[test]
    fn settle_is_persistent_state_not_a_view_filter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let store = SessionStore::open(&path).unwrap();
        let session = store.create_session("done deal").unwrap();
        assert!(!store.list_sessions().unwrap()[0].settled);

        // Settled: out of the active list, never out of the record.
        store.set_session_settled(session, true).unwrap();
        assert!(store.list_sessions().unwrap()[0].settled);

        // Reopen: a fresh process reads the same state.
        drop(store);
        let store = SessionStore::open(&path).unwrap();
        assert!(store.list_sessions().unwrap()[0].settled);

        // Picking the work back up clears it.
        store.set_session_settled(session, false).unwrap();
        assert!(!store.list_sessions().unwrap()[0].settled);
    }

    #[test]
    fn the_idle_sweep_settles_only_what_has_rested_long_enough() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let store = SessionStore::open(&path).unwrap();
        let old = store.create_session("old work").unwrap();
        let fresh = store.create_session("fresh work").unwrap();
        store.append_message(old, None, Role::User, "x").unwrap();
        store.append_message(fresh, None, Role::User, "y").unwrap();

        // Age one session's clock by hand: the store file is plain
        // SQLite; the test opens it beside the store.
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute(
            "UPDATE sessions SET last_activity_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1",
            [old.0 as i64],
        )
        .unwrap();
        drop(raw);

        // Ten idle days: only the aged session has rested that long.
        assert_eq!(store.settle_idle_sessions(10).unwrap(), 1);
        let summaries = store.list_sessions().unwrap();
        assert!(
            summaries
                .iter()
                .find(|summary| summary.id == old)
                .unwrap()
                .settled
        );
        assert!(
            !summaries
                .iter()
                .find(|summary| summary.id == fresh)
                .unwrap()
                .settled
        );

        // Settled is a resting state, not a re-stamp: the sweep has
        // nothing left to do.
        assert_eq!(store.settle_idle_sessions(10).unwrap(), 0);
    }

    #[test]
    fn the_workspace_roots_the_reclaim_command() {
        let store = memory_store();
        let session = store.create_session("rooted").unwrap();
        assert_eq!(
            store.session_workspace(session),
            None,
            "no root before a send records one"
        );
        store.set_session_workspace(session, Path::new("/tmp/some-checkout"));
        assert_eq!(
            store.session_workspace(session).as_deref(),
            Some("/tmp/some-checkout")
        );
    }

    #[test]
    fn bounded_loads_window_the_transcript_without_touching_the_archive() {
        let store = memory_store();
        let session = store.create_session("long chat").unwrap();
        let mut parent = None;
        for round in 0..10 {
            parent = Some(
                store
                    .append_message(session, parent, Role::User, &format!("u{round}"))
                    .unwrap(),
            );
            parent = Some(
                store
                    .append_message(session, parent, Role::Assistant, &format!("a{round}"))
                    .unwrap(),
            );
        }
        // Twenty rows total. A window of 6 gets the LAST 6, oldest first.
        let window = store.load_recent_messages(session, 6).unwrap();
        assert_eq!(window.len(), 6);
        assert_eq!(window[0].content, "u7");
        assert_eq!(window[5].content, "a9");

        // "Load earlier" pages from the window's oldest id, oldest first.
        let older = store
            .load_messages_before(session, window[0].id, 6)
            .unwrap();
        assert_eq!(older.len(), 6);
        assert_eq!(older[0].content, "u4");
        assert_eq!(older[5].content, "a6");

        // And again: three pages cover eighteen rows.
        let head = store.load_messages_before(session, older[0].id, 6).unwrap();
        assert_eq!(head.len(), 6);
        assert_eq!(head[0].content, "u1");
        assert_eq!(head[5].content, "a3");

        // The last page is short: pagination stops at the session's
        // true start, and twenty rows come back whole when asked for.
        let tail = store.load_messages_before(session, head[0].id, 6).unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].content, "u0");
        assert_eq!(store.load_recent_messages(session, 100).unwrap().len(), 20);

        // Another session's rows never leak through a window.
        let other = store.create_session("other").unwrap();
        store
            .append_message(other, None, Role::User, "foreign")
            .unwrap();
        assert_eq!(store.load_recent_messages(session, 100).unwrap().len(), 20);
        assert_eq!(store.load_recent_messages(other, 100).unwrap().len(), 1);
        assert!(
            store
                .load_messages_before(other, tail[0].id, 6)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn export_writes_one_json_object_per_row() {
        let store = memory_store();
        let session = store.create_session("shared trace").unwrap();
        let user = store
            .append_message(session, None, Role::User, "hello")
            .unwrap();
        store
            .append_message(session, Some(user), Role::Assistant, "hello back")
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("trace.jsonl");
        let count = store.export_session_jsonl(session, &path).unwrap();
        assert_eq!(count, 2);

        let raw = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).expect("every line is one JSON object"))
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["role"], "user");
        assert_eq!(lines[0]["content"], "hello");
        assert_eq!(lines[0]["session_title"], "shared trace");
        assert_eq!(lines[1]["role"], "assistant");
        assert_eq!(lines[1]["content"], "hello back");
        // Tool-only fields ride along as nulls, never silently absent.
        assert!(lines[0]["tool_call_id"].is_null());
        // A missing session is an error, not an empty file.
        assert!(store.export_session_jsonl(SessionId(9999), &path).is_err());
    }

    #[test]
    fn wire_history_expands_image_markers_back_into_images() {
        let store = memory_store();
        let session = store.create_session("with picture").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shot.png");
        std::fs::write(&path, b"\x89PNG-bytes").unwrap();
        store
            .append_message(
                session,
                None,
                Role::User,
                &format!("look\n\n{{{{image:{}}}}}", path.display()),
            )
            .unwrap();

        let wire = store.load_wire_history(session).unwrap();
        assert_eq!(wire.len(), 1);
        // Clean text on the wire: the marker never reaches the model.
        assert_eq!(wire[0].content.as_deref(), Some("look"));
        assert_eq!(wire[0].attachments.len(), 1);
        let (media_type, payload) =
            crate::request::data_url_parts(&wire[0].attachments[0]).unwrap();
        assert_eq!(media_type, "image/png");
        use base64::Engine as _;
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(payload)
                .unwrap(),
            b"\x89PNG-bytes"
        );
    }

    #[test]
    fn forks_are_sibling_nodes_not_overwrites() {
        let store = memory_store();
        let session = store.create_session("forked").unwrap();
        let root = store
            .append_message(session, None, Role::User, "pick one")
            .unwrap();
        let a = store
            .append_message(session, Some(root), Role::Assistant, "a")
            .unwrap();
        let b = store
            .append_message(session, Some(root), Role::Assistant, "b")
            .unwrap();

        // The reads follow the tip's branch; the sibling stays in the
        // table: an append-only audit, never an overwrite.
        let canonical = store.load_messages(session).unwrap();
        assert_eq!(canonical.len(), 2);
        assert_eq!(canonical[1].content, "b");
        let flat = store.load_rows_flat(session).unwrap();
        assert_eq!(flat.len(), 3);
        assert_ne!(a, b);
        assert_eq!(flat[1].parent_id, flat[2].parent_id);
    }

    #[test]
    fn forking_copies_the_path_and_leaves_the_source_whole() {
        let store = memory_store();
        let session = store.create_session("original").unwrap();
        let q1 = store
            .append_message(session, None, Role::User, "first question")
            .unwrap();
        let a1 = store
            .append_message(session, Some(q1), Role::Assistant, "first answer")
            .unwrap();
        let tool = store
            .append_tool_result(session, Some(a1), "call-1", "peek", "{}", "peeked")
            .unwrap();
        let _q2 = store
            .append_message(session, Some(tool), Role::User, "second question")
            .unwrap();

        // Fork at the first answer: the new session gets root → a1.
        let (fork_id, fork_point) = store.fork_session(session, a1, "original (fork)").unwrap();
        assert_ne!(fork_id, session);
        assert_eq!(store.load_messages(fork_id).unwrap().len(), 2);
        let forked = store.load_messages(fork_id).unwrap();
        assert_eq!(forked[0].content, "first question");
        assert_eq!(forked[0].parent_id, None);
        assert_eq!(forked[1].content, "first answer");
        assert_eq!(forked[1].parent_id, Some(forked[0].id));
        assert_eq!(fork_point, forked[1].id);
        // Tool rows fork verbatim when in the copied path.
        let (tool_fork, _) = store.fork_session(session, tool, "tool fork").unwrap();
        let tool_rows = store.load_messages(tool_fork).unwrap();
        assert_eq!(tool_rows.len(), 3);
        assert_eq!(tool_rows[2].role, Role::Tool);
        assert_eq!(tool_rows[2].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(tool_rows[2].tool_name.as_deref(), Some("peek"));
        // The source session is untouched: a fork adds, never rewrites.
        assert_eq!(store.load_messages(session).unwrap().len(), 4);
        assert_eq!(store.list_sessions().unwrap().len(), 3);
        // A fork point outside the session is an error, not a panic.
        assert!(
            store
                .fork_session(session, MessageId(9999), "nope")
                .is_err()
        );
    }

    #[test]
    fn the_newest_session_lists_first() {
        let store = memory_store();
        let first = store.create_session("first").unwrap();
        let second = store.create_session("second").unwrap();
        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions[0].id, second);
        assert_eq!(sessions.last().unwrap().id, first);
    }

    #[test]
    fn the_session_that_came_back_last_lists_first() {
        // Activity ordering, not creation ordering: an older session
        // whose record moved most recently outranks a newer, quieter
        // one. A silent session falls back to creation order.
        let store = memory_store();
        let older = store.create_session("older").unwrap();
        let newer = store.create_session("newer").unwrap();
        store
            .append_message(older, None, Role::User, "the older thread is alive again")
            .unwrap();

        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions[0].id, older);
        assert_eq!(sessions[1].id, newer);
    }

    #[test]
    fn deleting_a_session_leaves_nothing_behind() {
        let store = memory_store();
        let session = store.create_session("doomed").unwrap();
        let kept = store.create_session("kept").unwrap();
        store
            .append_message(session, None, Role::User, "ghost words for search")
            .unwrap();
        store
            .append_message(kept, None, Role::User, "kept words")
            .unwrap();

        store.retitle_session(session, "renamed once").unwrap();
        let sessions = store.list_sessions().unwrap();
        assert!(
            sessions
                .iter()
                .any(|summary| summary.id == session && summary.title == "renamed once")
        );

        store.delete_session(session).unwrap();
        assert!(
            store
                .list_sessions()
                .unwrap()
                .iter()
                .all(|summary| summary.id != session)
        );
        assert!(store.load_messages(session).unwrap().is_empty());
        // The FTS index has no delete trigger: it must not report the
        // dead session's words.
        assert!(store.search("ghost words", 10).unwrap().is_empty());
        assert_eq!(store.search("kept words", 10).unwrap().len(), 1);
    }

    #[test]
    fn wire_history_rebuilds_assistant_tool_pairings() {
        let store = memory_store();
        let session = store.create_session("tools").unwrap();

        let user = store
            .append_message(session, None, Role::User, "fix it")
            .unwrap();
        let assistant = store
            .append_message(session, Some(user), Role::Assistant, "on it")
            .unwrap();
        store
            .append_tool_result(
                session,
                Some(assistant),
                "call_1",
                "edit_file",
                r#"{"path":"x"}"#,
                "edited x",
            )
            .unwrap();
        let assistant_2 = store
            .append_message(session, Some(assistant), Role::Assistant, "done")
            .unwrap();

        let history = store.load_wire_history(session).unwrap();
        assert_eq!(history.len(), 4); // user, assistant+call, tool result, assistant
        assert_eq!(history[0], WireMessage::user("fix it"));
        assert_eq!(history[1].tool_calls.len(), 1);
        assert_eq!(history[1].tool_calls[0].name, "edit_file");
        assert_eq!(history[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(history[3], WireMessage::assistant("done"));
        let _ = assistant_2;
    }
}
