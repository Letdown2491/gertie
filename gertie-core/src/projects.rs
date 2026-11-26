//! Projects: named groups the human files conversations into.
//!
//! A project is a name, nothing more: not a folder, not a workspace.
//! Membership is explicit, recorded on the session
//! (`sessions.project_id`): a conversation is in a project only because
//! it was moved there or started from the project's +, and at most one.
//! Where a chat runs stays the chat's own business. Removing a project
//! takes it off the list and returns its chats to the ungrouped list;
//! no session is deleted.

use crate::{SessionId, SessionStore, SessionSummary};
use rusqlite::Connection;
use std::path::Path;

/// One project row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub collapsed: bool,
}

pub(crate) fn migrate(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS projects (
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL,
             position INTEGER NOT NULL,
             collapsed INTEGER NOT NULL DEFAULT 0
         );
         -- Named one-bit settings (the Other group's fold).
         CREATE TABLE IF NOT EXISTS store_flags (
             name TEXT PRIMARY KEY
         );",
    )?;
    drop_pre_release_folders(connection)
}

/// Pre-release builds gave a project a folder (`root`), and one of them
/// made a project per folder that existing chats ran in (flagged
/// `projects_seeded`). Both go: the seeded projects that still carry
/// their folder's own name (a renamed one was adopted and stays), then
/// the column, by rebuilding the table without it.
fn drop_pre_release_folders(connection: &Connection) -> rusqlite::Result<()> {
    let columns: Vec<String> = {
        let mut statement = connection.prepare("PRAGMA table_info(projects)")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if !columns.iter().any(|column| column == "root") {
        return Ok(());
    }
    let seeded = connection
        .prepare("SELECT 1 FROM store_flags WHERE name = 'projects_seeded'")?
        .exists([])?;
    if seeded {
        let rows: Vec<(i64, String, String)> = {
            let mut statement = connection.prepare("SELECT id, root, name FROM projects")?;
            let rows =
                statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for (id, root, name) in rows {
            let folder_name = Path::new(&root)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
            if folder_name.as_deref() == Some(name.as_str()) {
                connection.execute("DELETE FROM projects WHERE id = ?1", [id])?;
                connection.execute(
                    "UPDATE sessions SET project_id = NULL WHERE project_id = ?1",
                    [id],
                )?;
            }
        }
    }
    connection.execute_batch(
        "BEGIN;
         CREATE TABLE projects_named (
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL,
             position INTEGER NOT NULL,
             collapsed INTEGER NOT NULL DEFAULT 0
         );
         INSERT INTO projects_named (id, name, position, collapsed)
             SELECT id, name, position, collapsed FROM projects;
         DROP TABLE projects;
         ALTER TABLE projects_named RENAME TO projects;
         DELETE FROM store_flags WHERE name = 'projects_seeded';
         COMMIT;",
    )
}

/// The project a conversation was filed under, if it still exists.
pub fn project_of<'a>(projects: &'a [Project], summary: &SessionSummary) -> Option<&'a Project> {
    let id = summary.project_id?;
    projects.iter().find(|project| project.id == id)
}

impl SessionStore {
    /// Every project, in sidebar order.
    pub fn list_projects(&self) -> rusqlite::Result<Vec<Project>> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare("SELECT id, name, collapsed FROM projects ORDER BY position, id")?;
            let rows = statement.query_map([], |row| {
                Ok(Project {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    collapsed: row.get::<_, i64>(2)? != 0,
                })
            })?;
            rows.collect()
        })
    }

    /// Makes a project, last in order. None for a blank name: a project
    /// nobody can tell apart is worse than none.
    pub fn add_project(&self, name: &str) -> rusqlite::Result<Option<Project>> {
        let name = name.trim();
        if name.is_empty() {
            return Ok(None);
        }
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO projects (name, position)
                 VALUES (?1, (SELECT COALESCE(MAX(position), 0) + 1 FROM projects))",
                [name],
            )?;
            Ok(Some(Project {
                id: connection.last_insert_rowid(),
                name: name.to_string(),
                collapsed: false,
            }))
        })
    }

    pub fn rename_project(&self, id: i64, name: &str) -> rusqlite::Result<()> {
        let name = name.trim();
        if name.is_empty() {
            return Ok(());
        }
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE projects SET name = ?2 WHERE id = ?1",
                    rusqlite::params![id, name],
                )
                .map(|_| ())
        })
    }

    pub fn set_project_collapsed(&self, id: i64, collapsed: bool) -> rusqlite::Result<()> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE projects SET collapsed = ?2 WHERE id = ?1",
                    rusqlite::params![id, collapsed as i64],
                )
                .map(|_| ())
        })
    }

    /// Files a conversation under a project, or takes it out of one
    /// (`None`). One project at most: filing moves it.
    pub fn set_session_project(
        &self,
        session: SessionId,
        project: Option<i64>,
    ) -> rusqlite::Result<()> {
        self.with_connection(|connection| {
            connection
                .execute(
                    "UPDATE sessions SET project_id = ?2 WHERE id = ?1",
                    rusqlite::params![session.0 as i64, project],
                )
                .map(|_| ())
        })
    }

    /// Takes a project off the list. Its conversations go back to the
    /// ungrouped list; no session is touched otherwise.
    pub fn remove_project(&self, id: i64) -> rusqlite::Result<()> {
        self.with_connection(|connection| {
            connection.execute(
                "UPDATE sessions SET project_id = NULL WHERE project_id = ?1",
                [id],
            )?;
            connection
                .execute("DELETE FROM projects WHERE id = ?1", [id])
                .map(|_| ())
        })
    }

    /// A named one-bit setting kept in the store (the "Other" group's
    /// fold today).
    pub fn store_flag(&self, name: &str) -> bool {
        self.with_connection(|connection| {
            connection
                .prepare("SELECT 1 FROM store_flags WHERE name = ?1")
                .and_then(|mut statement| statement.exists([name]))
                .unwrap_or(false)
        })
    }

    pub fn set_store_flag(&self, name: &str, on: bool) -> rusqlite::Result<()> {
        self.with_connection(|connection| {
            if on {
                connection.execute(
                    "INSERT OR IGNORE INTO store_flags (name) VALUES (?1)",
                    [name],
                )
            } else {
                connection.execute("DELETE FROM store_flags WHERE name = ?1", [name])
            }
            .map(|_| ())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership_is_explicit_and_one_project_at_most() {
        let store = SessionStore::open(Path::new(":memory:")).unwrap();
        let research = store.add_project("Research").unwrap().unwrap();
        let errands = store.add_project("Errands").unwrap().unwrap();
        let chat = store.create_session("filed").unwrap();
        let loose = store.create_session("loose").unwrap();
        let owner = |id: SessionId| {
            let projects = store.list_projects().unwrap();
            let sessions = store.list_sessions().unwrap();
            let summary = sessions.iter().find(|summary| summary.id == id).unwrap();
            project_of(&projects, summary).map(|project| project.id)
        };
        assert_eq!(owner(chat), None, "a new chat is in no project");
        store.set_session_project(chat, Some(research.id)).unwrap();
        assert_eq!(owner(chat), Some(research.id));
        store.set_session_project(chat, Some(errands.id)).unwrap();
        assert_eq!(owner(chat), Some(errands.id), "filing again moves it");
        assert_eq!(owner(loose), None);
        store.set_session_project(chat, None).unwrap();
        assert_eq!(owner(chat), None);
    }

    #[test]
    fn projects_add_rename_collapse_and_remove_without_touching_sessions() {
        let store = SessionStore::open(Path::new(":memory:")).unwrap();
        assert_eq!(
            store.add_project("   ").unwrap(),
            None,
            "a blank name makes nothing"
        );
        let first = store.add_project("  Gertie  ").unwrap().unwrap();
        assert_eq!(first.name, "Gertie");
        // Names need not be unique: a project is its row, not its name.
        let second = store.add_project("Gertie").unwrap().unwrap();
        assert_ne!(first.id, second.id);
        store.rename_project(second.id, "Notes").unwrap();
        store.rename_project(second.id, "   ").unwrap(); // blank is ignored
        store.set_project_collapsed(second.id, true).unwrap();
        let listed = store.list_projects().unwrap();
        assert_eq!(listed[1].name, "Notes");
        assert!(listed[1].collapsed);

        let session = store.create_session("chat").unwrap();
        store.set_session_workspace(session, Path::new("/some/folder"));
        store.set_session_project(session, Some(second.id)).unwrap();
        store.remove_project(second.id).unwrap();
        assert_eq!(store.list_projects().unwrap().len(), 1);
        let summary = store.list_sessions().unwrap().pop().unwrap();
        assert_eq!(summary.project_id, None, "the chat is ungrouped again");
        assert_eq!(summary.workspace_root.as_deref(), Some("/some/folder"));
    }

    #[test]
    fn a_pre_release_store_loses_its_folders_and_the_seeded_projects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE projects (
                         id INTEGER PRIMARY KEY,
                         root TEXT NOT NULL UNIQUE,
                         name TEXT NOT NULL,
                         position INTEGER NOT NULL,
                         collapsed INTEGER NOT NULL DEFAULT 0
                     );
                     CREATE TABLE store_flags (name TEXT PRIMARY KEY);
                     INSERT INTO projects VALUES (1, '/w/noscat', 'noscat', 1, 0);
                     INSERT INTO projects VALUES (2, '/w/gertie', 'Gertie app', 2, 1);
                     INSERT INTO store_flags VALUES ('projects_seeded');",
                )
                .unwrap();
        }
        let store = SessionStore::open(&path).unwrap();
        assert_eq!(
            store.list_projects().unwrap(),
            vec![Project {
                id: 2,
                name: "Gertie app".into(),
                collapsed: true
            }]
        );
        assert!(!store.store_flag("projects_seeded"));
        drop(store);
        // Idempotent: a second open finds nothing to do.
        assert_eq!(
            SessionStore::open(&path)
                .unwrap()
                .list_projects()
                .unwrap()
                .len(),
            1
        );
    }
}
