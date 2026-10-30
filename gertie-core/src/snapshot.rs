//! Checkpoints without git: a content-addressed snapshot store.
//!
//! Before any mutating tool touches a file, the loop records the file's
//! prior bytes here. Objects live under `<dir>/objects/<sha256>` and are
//! deduplicated by hash; the ledger ties (session, path) to the object the
//! file had before the edit. Restoring is reading a hash-addressed blob
//! back — no diffs to compute, no git repo required.
//!
//! Turns: the front mints a turn row when it hands work to the loop, so
//! the checkpoints recorded during that turn — and only those — can be
//! reviewed as one diff. Membership is checkpoint ids in the turn's
//! window `(before, next turn's before]`, session-filtered; the ledger
//! answers "what did this turn change" from objects alone, so a past
//! turn's diff never shifts under later edits.

use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Identifies one checkpoint in the ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CheckpointId(pub u64);

/// Identifies one minted turn window in the ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnId(pub i64);

/// One turn's window into the ledger: minted by the front at send time,
/// before the loop records anything for the turn. The boundary is the
/// session's highest checkpoint id at mint time; the turn's checkpoints
/// are the ids above it, up to the next minted turn's boundary.
#[derive(Clone, Debug)]
pub struct TurnBounds {
    pub id: TurnId,
    pub session_id: crate::SessionId,
    /// The user message row that started the turn — how a tool card in
    /// the transcript finds its turn again after a reload.
    pub user_message_id: crate::MessageId,
    pub before: CheckpointId,
}

/// Where a turn-touched path's end-of-turn content lives. Distinct
/// variants, not sentinels: "the next checkpoint object holds it" and
/// "disk is the answer" are different facts.
#[derive(Clone, Debug)]
pub enum AfterState {
    /// The path was touched again after this turn; its next checkpoint
    /// object holds the content as this turn left it.
    Object(String),
    /// Never touched again — current disk content is what this turn
    /// left. (Reading disk is the caller's job: it owns the root the
    /// tool-relative paths resolve against.)
    Disk,
}

/// One path a turn touched, as the ledger saw it.
#[derive(Clone, Debug)]
pub struct TurnFileChange {
    pub path: PathBuf,
    /// The turn's first checkpoint for this path — its object is the
    /// before content, and the restore point for undoing the turn.
    pub checkpoint: CheckpointId,
    /// sha256 of the before object.
    pub before_hash: String,
    pub after: AfterState,
}

/// One recorded "what this file looked like before" entry.
#[derive(Clone, Debug)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub path: PathBuf,
    /// sha256 of the object holding the prior content.
    pub hash: String,
}

pub struct SnapshotStore {
    /// `Connection` is `!Sync` (statement caches are `RefCell`s); the
    /// store is shared across the loop's awaits, so lock per operation.
    /// Writes are small and local; contention is nil.
    connection: std::sync::Mutex<Connection>,
    objects: PathBuf,
}

impl SnapshotStore {
    /// Opens (creating if needed) the store rooted at `dir` — typically
    /// `~/.local/share/gertie/snapshots`.
    pub fn open(dir: &Path) -> Result<Self, SnapshotError> {
        let objects = dir.join("objects");
        std::fs::create_dir_all(&objects)?;
        let connection = Connection::open(dir.join("checkpoints.db"))?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             -- Same story as SessionStore: a second process holding the
             -- write lock is a reason to wait, not to fail the open.
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS checkpoints (
                 id INTEGER PRIMARY KEY,
                 session_id INTEGER,
                 path TEXT NOT NULL,
                 hash TEXT NOT NULL,
                 created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE INDEX IF NOT EXISTS checkpoints_by_session
                 ON checkpoints(session_id, id);
            CREATE TABLE IF NOT EXISTS turns (
                id INTEGER PRIMARY KEY,
                session_id INTEGER NOT NULL,
                user_message_id INTEGER NOT NULL,
                before_checkpoint INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
            );
            CREATE INDEX IF NOT EXISTS turns_by_session
                ON turns(session_id, user_message_id);",
        )?;
        Ok(Self {
            connection: std::sync::Mutex::new(connection),
            objects,
        })
    }

    /// Records `content_before` as the prior state of `path`. Callers
    /// snapshot *before* running a mutating tool; content is deduplicated
    /// by hash, so re-recording unchanged files costs one hash.
    pub fn record(
        &self,
        session_id: Option<crate::SessionId>,
        path: &Path,
        content_before: &[u8],
    ) -> Result<CheckpointId, SnapshotError> {
        let hash = format!("{:x}", Sha256::digest(content_before));
        let object = self.objects.join(&hash);
        if !object.exists() {
            // Write-then-rename so a crash can't leave a truncated object
            // that later looks present.
            let mut temporary = self.objects.join(format!(".{hash}.tmp"));
            temporary.set_extension("tmp");
            let mut file = std::fs::File::create(&temporary)?;
            file.write_all(content_before)?;
            drop(file);
            std::fs::rename(&temporary, &object)?;
        }
        let connection = self.connection.lock().unwrap();
        connection
            .prepare("INSERT INTO checkpoints (session_id, path, hash) VALUES (?1, ?2, ?3)")?
            .execute(rusqlite::params![
                session_id.map(|id| id.0 as i64),
                path.to_string_lossy(),
                hash,
            ])?;
        Ok(CheckpointId(connection.last_insert_rowid() as u64))
    }

    /// Snapshots a file's current content if it exists; a missing file is
    /// still a meaningful prior state (restoring means deleting), recorded
    /// as an empty object for v1.
    pub fn record_file(
        &self,
        session_id: Option<crate::SessionId>,
        path: &Path,
    ) -> Result<CheckpointId, SnapshotError> {
        let content = std::fs::read(path).unwrap_or_default();
        self.record(session_id, path, &content)
    }

    /// A session's checkpoints, oldest first.
    pub fn list(&self, session_id: crate::SessionId) -> Result<Vec<Checkpoint>, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        let mut statement = connection
            .prepare("SELECT id, path, hash FROM checkpoints WHERE session_id = ?1 ORDER BY id")?;
        let rows = statement.query_map([session_id.0 as i64], |row| {
            Ok(Checkpoint {
                id: CheckpointId(row.get::<_, i64>(0)? as u64),
                path: PathBuf::from(row.get::<_, String>(1)?),
                hash: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    /// Writes the checkpoint's object back to its path. Returns the path
    /// that was restored.
    pub fn restore(&self, checkpoint: CheckpointId) -> Result<PathBuf, SnapshotError> {
        self.restore_under(checkpoint, Path::new("."))
    }

    /// Restores with tool-relative paths resolved against `root` — the
    /// workspace the tools ran in, which is not always the caller's
    /// process CWD.
    pub fn restore_under(
        &self,
        checkpoint: CheckpointId,
        root: &Path,
    ) -> Result<PathBuf, SnapshotError> {
        let connection = self.connection.lock().unwrap();
        let row = connection.query_row(
            "SELECT path, hash FROM checkpoints WHERE id = ?1",
            [checkpoint.0 as i64],
            |row| {
                Ok((
                    PathBuf::from(row.get::<_, String>(0)?),
                    row.get::<_, String>(1)?,
                ))
            },
        )?;
        let (path, hash) = row;
        let path = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };
        std::fs::copy(self.objects.join(hash), &path)?;
        Ok(path)
    }

    /// Mints a turn window: the front calls this the moment before it
    /// hands the user's message to the loop. Everything the loop's
    /// mutating tools record from here — for this session — belongs to
    /// the turn, until the next mint.
    pub fn begin_turn(
        &self,
        session_id: crate::SessionId,
        user_message_id: crate::MessageId,
    ) -> Result<TurnBounds, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        let before: i64 = connection.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM checkpoints WHERE session_id = ?1",
            [session_id.0 as i64],
            |row| row.get(0),
        )?;
        connection
            .prepare(
                "INSERT INTO turns (session_id, user_message_id, before_checkpoint)
                 VALUES (?1, ?2, ?3)",
            )?
            .execute(rusqlite::params![
                session_id.0 as i64,
                user_message_id.0 as i64,
                before
            ])?;
        Ok(TurnBounds {
            id: TurnId(connection.last_insert_rowid()),
            session_id,
            user_message_id,
            before: CheckpointId(before as u64),
        })
    }

    /// The turn a transcript position belongs to: the latest mint whose
    /// user message is at or before this row. Steered messages (user
    /// pushes mid-turn) have no mint of their own, so they resolve to
    /// the turn they steered.
    pub fn turn_for_message(
        &self,
        session_id: crate::SessionId,
        message_id: crate::MessageId,
    ) -> Result<Option<TurnBounds>, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        match connection.query_row(
            "SELECT id, user_message_id, before_checkpoint FROM turns
             WHERE session_id = ?1 AND user_message_id <= ?2
             ORDER BY user_message_id DESC LIMIT 1",
            rusqlite::params![session_id.0 as i64, message_id.0 as i64],
            |row| {
                Ok(TurnBounds {
                    id: TurnId(row.get(0)?),
                    session_id,
                    user_message_id: crate::MessageId(row.get::<_, i64>(1)? as u64),
                    before: CheckpointId(row.get::<_, i64>(2)? as u64),
                })
            },
        ) {
            Ok(turn) => Ok(Some(turn)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// What the turn changed: one entry per path the turn's checkpoints
    /// recorded, oldest state first. "Before" is the turn's first
    /// checkpoint object for the path; "after" is the path's next
    /// checkpoint object in the ledger, or [`AfterState::Disk`] when
    /// nothing touched it again — a past turn's diff never shifts under
    /// later edits.
    pub fn turn_changes(&self, turn: &TurnBounds) -> Result<Vec<TurnFileChange>, rusqlite::Error> {
        let connection = self.connection.lock().unwrap();
        // The window's top edge: the next minted turn's boundary, or
        // open-ended for the newest turn.
        let top: i64 = connection
            .query_row(
                "SELECT MIN(before_checkpoint) FROM turns
                 WHERE session_id = ?1 AND before_checkpoint > ?2",
                rusqlite::params![turn.session_id.0 as i64, turn.before.0 as i64],
                |row| row.get::<_, Option<i64>>(0),
            )?
            .unwrap_or(i64::MAX);
        let mut statement = connection.prepare(
            "SELECT id, path, hash FROM checkpoints
             WHERE session_id = ?1 AND id > ?2 AND id <= ?3 ORDER BY id",
        )?;
        let rows = statement.query_map(
            rusqlite::params![turn.session_id.0 as i64, turn.before.0 as i64, top],
            |row| {
                Ok((
                    CheckpointId(row.get::<_, i64>(0)? as u64),
                    PathBuf::from(row.get::<_, String>(1)?),
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        // Rows borrow the statement — collect inside the arm.
        let recorded: Vec<(CheckpointId, PathBuf, String)> = rows.collect::<Result<_, _>>()?;
        drop(statement);

        // First checkpoint per path wins: it holds the before-turn
        // content; later ones in the window are the turn's own
        // intermediate states.
        let mut changes = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (id, path, hash) in recorded {
            if !seen.insert(path.clone()) {
                continue;
            }
            let after = match connection.query_row(
                "SELECT hash FROM checkpoints
                 WHERE session_id = ?1 AND path = ?2 AND id > ?3 ORDER BY id LIMIT 1",
                rusqlite::params![turn.session_id.0 as i64, path.to_string_lossy(), top],
                |row| row.get::<_, String>(0),
            ) {
                Ok(hash) => AfterState::Object(hash),
                Err(rusqlite::Error::QueryReturnedNoRows) => AfterState::Disk,
                Err(error) => return Err(error),
            };
            changes.push(TurnFileChange {
                path,
                checkpoint: id,
                before_hash: hash,
                after,
            });
        }
        Ok(changes)
    }

    /// Reads a checkpoint object's bytes back — the diff view's "before".
    pub fn read_object(&self, hash: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(self.objects.join(hash))
    }
}

#[derive(Debug)]
pub enum SnapshotError {
    Io(std::io::Error),
    Db(rusqlite::Error),
}

impl From<std::io::Error> for SnapshotError {
    fn from(error: std::io::Error) -> Self {
        SnapshotError::Io(error)
    }
}

impl From<rusqlite::Error> for SnapshotError {
    fn from(error: rusqlite::Error) -> Self {
        SnapshotError::Db(error)
    }
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotError::Io(error) => write!(f, "{error}"),
            SnapshotError::Db(error) => write!(f, "{error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in(dir: &Path) -> SnapshotStore {
        SnapshotStore::open(dir).expect("open snapshot store")
    }

    #[test]
    fn records_and_restores_file_content() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let file = tmp.path().join("notes.txt");
        std::fs::write(&file, "before").unwrap();

        let checkpoint = store.record_file(Some(crate::SessionId(1)), &file).unwrap();
        std::fs::write(&file, "after").unwrap();
        store.restore(checkpoint).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "before");
    }

    #[test]
    fn identical_content_stores_one_object() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, "same").unwrap();

        let first = store.record_file(None, &file).unwrap();
        let second = store.record_file(None, &file).unwrap();
        assert_ne!(first, second);
        let objects: Vec<_> = std::fs::read_dir(tmp.path().join("objects"))
            .unwrap()
            .collect();
        assert_eq!(objects.len(), 1);
    }

    #[test]
    fn sessions_list_only_their_own_checkpoints() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        store.record_file(Some(crate::SessionId(1)), &file).unwrap();
        store.record_file(Some(crate::SessionId(2)), &file).unwrap();

        assert_eq!(store.list(crate::SessionId(1)).unwrap().len(), 1);
        assert_eq!(store.list(crate::SessionId(2)).unwrap().len(), 1);
    }

    fn session_and_message(id: u64) -> (crate::SessionId, crate::MessageId) {
        (crate::SessionId(id), crate::MessageId(id * 10))
    }

    #[test]
    fn a_turns_window_covers_exactly_its_checkpoints() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let (session, first) = session_and_message(1);
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        std::fs::write(&a, "one").unwrap();
        std::fs::write(&b, "bee").unwrap();

        let turn1 = store.begin_turn(session, first).unwrap();
        let a_before = store.record_file(Some(session), &a).unwrap();
        store.record_file(Some(session), &b).unwrap();
        std::fs::write(&a, "two").unwrap();

        let turn2 = store
            .begin_turn(session, crate::MessageId(first.0 + 1))
            .unwrap();
        store.record_file(Some(session), &a).unwrap();

        // Turn 1 touched two paths; a's before is its first checkpoint,
        // and its after lives in turn 2's checkpoint — not disk.
        let changes1 = store.turn_changes(&turn1).unwrap();
        assert_eq!(changes1.len(), 2);
        let a1 = changes1
            .iter()
            .find(|change| change.path == a)
            .expect("a.txt in turn 1");
        assert_eq!(a1.checkpoint, a_before);
        assert_eq!(store.read_object(&a1.before_hash).unwrap(), b"one");
        match &a1.after {
            AfterState::Object(hash) => {
                assert_eq!(store.read_object(hash).unwrap(), b"two")
            }
            other => panic!("expected turn 2's object as a's after, got {other:?}"),
        }
        // b was never touched again: disk is the answer.
        let b1 = changes1.iter().find(|change| change.path == b).unwrap();
        assert!(matches!(b1.after, AfterState::Disk));

        // Turn 2 saw a at "two"; nothing after it touched anything.
        let changes2 = store.turn_changes(&turn2).unwrap();
        assert_eq!(changes2.len(), 1);
        assert_eq!(store.read_object(&changes2[0].before_hash).unwrap(), b"two");
        assert!(matches!(changes2[0].after, AfterState::Disk));
    }

    #[test]
    fn a_turn_with_no_checkpoints_answers_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let (session, first) = session_and_message(1);
        let turn = store.begin_turn(session, first).unwrap();
        assert!(store.turn_changes(&turn).unwrap().is_empty());
    }

    #[test]
    fn turns_resolve_from_their_user_message_and_carry_steers() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let (session, first) = session_and_message(1);
        assert!(store.turn_for_message(session, first).unwrap().is_none());

        let turn = store.begin_turn(session, first).unwrap();
        // Steered pushes land after the mint — they resolve to the turn
        // they steered, not to nothing.
        let resolved = store
            .turn_for_message(session, crate::MessageId(first.0 + 3))
            .unwrap()
            .expect("steer resolves to its turn");
        assert_eq!(resolved.id, turn.id);
        // Another session's mint is not this session's answer.
        let (other, _) = session_and_message(2);
        assert!(store.turn_for_message(other, first).unwrap().is_none());
    }

    #[test]
    fn restore_resolves_relative_paths_against_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_in(tmp.path());
        let workspace = tempfile::tempdir().unwrap();
        let relative = std::path::Path::new("src/lib.rs");
        let absolute = workspace.path().join(relative);
        std::fs::create_dir_all(absolute.parent().unwrap()).unwrap();
        std::fs::write(&absolute, "before").unwrap();

        // Record via the content-level API: the ledger stores the path
        // as the tools spelled it (relative), which is exactly what the
        // resolution under test has to handle.
        let checkpoint = store
            .record(Some(crate::SessionId(1)), relative, b"before")
            .unwrap();
        std::fs::write(&absolute, "after").unwrap();
        store.restore_under(checkpoint, workspace.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&absolute).unwrap(),
            "before",
            "relative paths resolve against the given root"
        );
    }
}
