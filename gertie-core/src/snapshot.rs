//! Checkpoints without git: a content-addressed snapshot store.
//!
//! Before any mutating tool touches a file, the loop records the file's
//! prior bytes here. Objects live under `<dir>/objects/<sha256>` and are
//! deduplicated by hash; the ledger ties (session, path) to the object the
//! file had before the edit. Restoring is reading a hash-addressed blob
//! back — no diffs to compute, no git repo required.

use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Identifies one checkpoint in the ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CheckpointId(pub u64);

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
                 ON checkpoints(session_id, id);",
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
        std::fs::copy(self.objects.join(hash), &path)?;
        Ok(path)
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
}
