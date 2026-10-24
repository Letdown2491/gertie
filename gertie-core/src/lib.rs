//! gertie-core: the headless agent harness.
//!
//! This crate is the whole harness minus the UI. It must never depend on a
//! GUI framework: it is embeddable from a CLI, a server, or an editor. The
//! UI-facing `gertie` crate is one client of many.
//!
//! Currently ships the two foundations everything else hangs off: the
//! tree-shaped message model with SQLite-backed sessions, and the typed
//! provider event stream. The agent loop, permissions, checkpoints, and the
//! `agent` tool land in M2.

use rusqlite::Connection;
use std::path::Path;

/// Identifies one message in a session's message tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MessageId(pub u64);

/// Identifies a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u64);

/// Who authored a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    System,
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
}

/// A summary row for a session, for sidebars and resumption menus.
#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub id: SessionId,
    pub title: String,
}

/// SQLite-backed session persistence. Synchronous on purpose: writes are
/// small, rare, and local; callers decide which thread to block.
pub struct SessionStore {
    connection: Connection,
}

impl SessionStore {
    /// Opens (creating if needed) the store at `path`.
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
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
                 content TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS messages_by_session
                 ON messages(session_id, id);",
        )?;
        Ok(Self { connection })
    }

    /// Creates a session titled from its first user message (callers may
    /// retitle later).
    pub fn create_session(&self, title: &str) -> Result<SessionId, rusqlite::Error> {
        self.connection
            .prepare("INSERT INTO sessions (title) VALUES (?1)")?
            .execute([title])?;
        Ok(SessionId(self.connection.last_insert_rowid() as u64))
    }

    /// Newest session first, for the sidebar.
    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>, rusqlite::Error> {
        let mut statement = self
            .connection
            .prepare("SELECT id, title FROM sessions ORDER BY id DESC")?;
        let rows = statement.query_map([], |row| {
            Ok(SessionSummary {
                id: SessionId(row.get::<_, i64>(0)? as u64),
                title: row.get(1)?,
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
        self.connection
            .prepare("INSERT INTO messages (session_id, parent_id, role, content) VALUES (?1, ?2, ?3, ?4)")?
            .execute(rusqlite::params![
                session_id.0 as i64,
                parent_id.map(|id| id.0 as i64),
                role.as_str(),
                content,
            ])?;
        Ok(MessageId(self.connection.last_insert_rowid() as u64))
    }

    /// Rewrites a message's content in place — this is how streaming turns
    /// land without inserting a row per token.
    pub fn update_message(
        &self,
        message_id: MessageId,
        content: &str,
    ) -> Result<(), rusqlite::Error> {
        self.connection
            .prepare("UPDATE messages SET content = ?1 WHERE id = ?2")?
            .execute(rusqlite::params![content, message_id.0 as i64])?;
        Ok(())
    }

    /// Retitles a session (e.g. from its first exchange).
    pub fn retitle_session(&self, session_id: SessionId, title: &str) -> Result<(), rusqlite::Error> {
        self.connection
            .prepare("UPDATE sessions SET title = ?1 WHERE id = ?2")?
            .execute(rusqlite::params![title, session_id.0 as i64])?;
        Ok(())
    }

    /// Loads a session's messages oldest-first. M1 renders the flat path;
    /// the tree walk arrives with forking.
    pub fn load_messages(&self, session_id: SessionId) -> Result<Vec<Message>, rusqlite::Error> {
        let mut statement = self.connection.prepare(
            "SELECT id, parent_id, role, content FROM messages
             WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = statement.query_map([session_id.0 as i64], |row| {
            Ok(Message {
                id: MessageId(row.get::<_, i64>(0)? as u64),
                parent_id: row.get::<_, Option<i64>>(1)?.map(|id| MessageId(id as u64)),
                role: Role::from_str(&row.get::<_, String>(2)?),
                content: row.get(3)?,
            })
        })?;
        rows.collect()
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
    fn the_newest_session_lists_first() {
        let store = memory_store();
        let first = store.create_session("first").unwrap();
        let second = store.create_session("second").unwrap();
        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions[0].id, second);
        assert_eq!(sessions.last().unwrap().id, first);
    }
}
