//! gertie-core: the headless agent harness.
//!
//! This crate is the whole harness minus the UI. It must never depend on a
//! GUI framework: it is embeddable from a CLI, a server, or an editor. The
//! UI-facing `gertie` crate is one client of many.

pub mod agent;
pub mod agent_loop;
pub mod permissions;
pub mod provider;
pub mod request;
pub mod snapshot;
pub mod tool;

pub use agent::{AgentTool, AgentType, builtin_agent_types, sanitize_untrusted, worker_registry};
pub use agent_loop::{
    LoopEvent, PermissionDecision, SharedHistory, ToolRegistry, TurnSpec, run_turn,
};
pub use permissions::{PendingAsk, PermissionBroker, PromptSource};
pub use provider::Provider;
pub use request::{
    IMAGE_MARKER_PREFIX, Request, ToolCall, ToolSpec, WireMessage, data_url_parts,
    extract_image_markers, file_to_data_url,
};
use rusqlite::Connection;
pub use snapshot::{Checkpoint, CheckpointId, SnapshotStore};
use std::path::Path;
pub use tool::{Tool, ToolResult};

/// Identifies one message in a session's message tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageId(pub u64);

/// Identifies a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u64);

/// The front's current session, shared with machinery that runs outside
/// any session's knowledge — subagent dispatch — so worker runs can
/// record their lineage. One atomic cell: the front sets it whenever the
/// open session changes; dispatch reads it once. `None` means unknown —
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

/// A summary row for a session, for sidebars and resumption menus.
#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub id: SessionId,
    pub title: String,
    /// Cumulative token totals, persisted from usage events. A resumed
    /// session shows what it spent before, not just since reopen.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The last request's context size (prompt + completion) — the live
    /// number the compaction threshold watches. Persisted so a resumed
    /// session shows its distance to the window immediately.
    pub context_tokens: u64,
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
/// connection is mutex-guarded so the store is `Sync` — persistence can
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
                 ON messages(session_id, id);",
        )?;
        // Full-text index over message content (FTS5, bundled SQLite).
        // External-content-free on purpose: messages are append-only
        // and the index mirrors them via trigger — dead simple.
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
        // kind: chats appear in the sidebar, worker runs in the agents
        // panel. Existing rows are chats; the column defaults to that.
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
        ] {
            if !session_columns.contains(&column.to_string()) {
                connection.execute(
                    &format!("ALTER TABLE sessions ADD COLUMN {column} {definition}"),
                    [],
                )?;
            }
        }
        Ok(Self {
            connection: std::sync::Mutex::new(connection),
        })
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
    /// not appear in the sidebar; the agents panel lists them.
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

    /// A chat session's worker runs, oldest first — the agents panel's
    /// rows.
    pub fn list_child_sessions(
        &self,
        parent: SessionId,
    ) -> Result<Vec<SessionSummary>, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        let mut statement = connection
            .prepare("SELECT id, title, input_tokens, output_tokens, last_context_tokens FROM sessions WHERE parent_id = ?1 ORDER BY id")?;
        let rows = statement
            .query_map([parent.0 as i64], |row| {
                Ok(SessionSummary {
                    id: SessionId(row.get::<_, i64>(0)? as u64),
                    title: row.get(1)?,
                    input_tokens: row.get::<_, i64>(2)?.max(0) as u64,
                    output_tokens: row.get::<_, i64>(3)?.max(0) as u64,
                    context_tokens: row.get::<_, i64>(4)?.max(0) as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Whether a worker run has its report yet: any assistant row means
    /// the run finished (or failed loudly) — the panel's status.
    pub fn worker_run_reported(&self, session_id: SessionId) -> bool {
        let connection = self.connection.lock().unwrap();
        connection
            .prepare("SELECT 1 FROM messages WHERE session_id = ?1 AND role = 'assistant' LIMIT 1")
            .and_then(|mut statement| statement.query_row([session_id.0 as i64], |_| Ok(())))
            .is_ok()
    }

    /// Adds a request's usage to the session's running totals, and
    /// remembers the request's context size (prompt + completion — what
    /// the next request carries). One call per Usage event — a single
    /// UPDATE, not a per-token write.
    pub fn add_usage(
        &self,
        session: SessionId,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<(), rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        connection
            .prepare(
                "UPDATE sessions SET input_tokens = input_tokens + ?2,
                        output_tokens = output_tokens + ?3,
                        last_context_tokens = ?2 + ?3 WHERE id = ?1",
            )?
            .execute(rusqlite::params![
                session.0 as i64,
                input_tokens as i64,
                output_tokens as i64
            ])?;
        Ok(())
    }

    /// Token totals across every session — chats and worker runs. The
    /// settings panel's bottom line: what gertie has cost, in tokens.
    pub fn total_usage(&self) -> (u64, u64) {
        let connection = self.connection.lock().unwrap();
        connection
            .query_row(
                "SELECT COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0)
                 FROM sessions",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?.max(0) as u64,
                        row.get::<_, i64>(1)?.max(0) as u64,
                    ))
                },
            )
            .unwrap_or((0, 0))
    }

    /// Newest session first, for the sidebar.
    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        let mut statement = connection.prepare(
            "SELECT id, title, input_tokens, output_tokens, last_context_tokens FROM sessions WHERE kind = 'chat' ORDER BY id DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(SessionSummary {
                id: SessionId(row.get::<_, i64>(0)? as u64),
                title: row.get(1)?,
                input_tokens: row.get::<_, i64>(2)?.max(0) as u64,
                output_tokens: row.get::<_, i64>(3)?.max(0) as u64,
                context_tokens: row.get::<_, i64>(4)?.max(0) as u64,
            })
        })?;
        rows.collect()
    }

    /// Full-text search over every message ever recorded — the model's
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

    /// Appends a message under `parent_id` (None = root-level turn).
    pub fn append_message(
        &self,
        session_id: SessionId,
        parent_id: Option<MessageId>,
        role: Role,
        content: &str,
    ) -> Result<MessageId, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        connection
            .prepare(
                "INSERT INTO messages (session_id, parent_id, role, content) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(rusqlite::params![
                session_id.0 as i64,
                parent_id.map(|id| id.0 as i64),
                role.as_str(),
                content,
            ])?;
        Ok(MessageId(connection.last_insert_rowid() as u64))
    }

    /// Appends a tool result — the row that answers one tool call.
    pub fn append_tool_result(
        &self,
        session_id: SessionId,
        parent_id: Option<MessageId>,
        call_id: &str,
        tool_name: &str,
        arguments: &str,
        content: &str,
    ) -> Result<MessageId, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        connection
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
        Ok(MessageId(connection.last_insert_rowid() as u64))
    }

    /// Rewrites a message's content in place — this is how streaming turns
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

    /// Deletes a session, its messages, and their full-text rows — the
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
        transaction.execute("DELETE FROM sessions WHERE id = ?1", [session_id.0 as i64])?;
        transaction.commit()
    }

    /// Loads a session's messages oldest-first. M1 renders the flat path;
    /// the tree walk arrives with forking.
    pub fn load_messages(&self, session_id: SessionId) -> Result<Vec<Message>, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        let mut statement = connection.prepare(
            "SELECT id, parent_id, role, content, tool_call_id, tool_name, tool_arguments
             FROM messages WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = statement.query_map([session_id.0 as i64], |row| {
            Ok(Message {
                id: MessageId(row.get::<_, i64>(0)? as u64),
                parent_id: row.get::<_, Option<i64>>(1)?.map(|id| MessageId(id as u64)),
                role: Role::from_str(&row.get::<_, String>(2)?),
                content: row.get(3)?,
                tool_call_id: row.get(4)?,
                tool_name: row.get(5)?,
                tool_arguments: row.get(6)?,
            })
        })?;
        rows.collect()
    }

    /// Reconstructs the canonical wire history from stored rows: assistant
    /// rows re-attach the tool calls their child tool rows answer, so a
    /// resumed session requests with exactly what the model saw before.
    pub fn load_wire_history(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<crate::request::WireMessage>, rusqlite::Error> {
        use crate::request::{ToolCall, WireMessage};
        let messages = self.load_messages(session_id)?;
        let mut wire = Vec::new();
        let mut index = 0;
        while index < messages.len() {
            let message = &messages[index];
            // Image markers ride inside stored content; expand them back
            // into wire images so a resumed session re-sends what the
            // model saw. A missing file degrades to a text note — the
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
                    wire.push(WireMessage::user_with_images(content, images));
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
        Ok(wire)
    }

    /// Forks the conversation at `upto`: a new session whose messages
    /// are the canonical path root → `upto`, copied verbatim (tool
    /// fields ride along; parents are remapped to the new ids). The
    /// parent_id column planned for this all along — a fork is a
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
            .load_messages(source)
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

        let new_session = self
            .create_session(title)
            .map_err(|error| error.to_string())?;
        let mut remap: std::collections::HashMap<MessageId, MessageId> =
            std::collections::HashMap::new();
        let mut fork_point = upto;
        for message in &chain {
            let copied = {
                let connection = self.connection.lock().unwrap();
                let mapped_parent = message.parent_id.map(|parent| remap[&parent]);
                connection
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
                MessageId(connection.last_insert_rowid() as u64)
            };
            remap.insert(message.id, copied);
            fork_point = copied;
        }
        Ok((new_session, fork_point))
    }

    /// Exports a session's messages as JSONL — one object per row,
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
    /// Final usage accounting for the request.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    /// The stream finished normally.
    Done,
    /// The stream failed. Retries are the provider layer's job; by the time
    /// an `Error` surfaces, the policy in DESIGN.md has been exhausted.
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_store() -> SessionStore {
        SessionStore::open(Path::new(":memory:")).expect("open in-memory store")
    }

    #[test]
    fn usage_persists_per_session_and_sums_across_all() {
        let store = memory_store();
        let chat = store.create_session("chat").unwrap();
        let worker = store.create_worker_run(Some(chat), "worker").unwrap();

        store.add_usage(chat, 100, 10).unwrap();
        store.add_usage(chat, 40, 5).unwrap();
        store.add_usage(worker, 200, 30).unwrap();

        let chats = store.list_sessions().unwrap();
        let chat_row = chats.iter().find(|summary| summary.id == chat).unwrap();
        assert_eq!((chat_row.input_tokens, chat_row.output_tokens), (140, 15));
        // The last request's size is the live context gauge: prompt +
        // completion of the most recent usage event, not a sum.
        assert_eq!(chat_row.context_tokens, 40 + 5);
        // The all-sessions total includes worker runs too — that's the
        // real cost.
        assert_eq!(store.total_usage(), (340, 45));
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
        // Clean text on the wire — the marker never reaches the model.
        assert_eq!(wire[0].content.as_deref(), Some("look"));
        assert_eq!(wire[0].images.len(), 1);
        let (media_type, payload) = crate::request::data_url_parts(&wire[0].images[0]).unwrap();
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

        let messages = store.load_messages(session).unwrap();
        assert_eq!(messages.len(), 3);
        assert_ne!(a, b);
        assert_eq!(messages[1].parent_id, messages[2].parent_id);
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
        // The source session is untouched — a fork adds, never rewrites.
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
        // The FTS index has no delete trigger — it must not report the
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
