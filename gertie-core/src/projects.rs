//! Projects: groups the human makes and files conversations into.
//!
//! A project is a name and a folder. Membership is explicit, recorded
//! on the session (`sessions.project_id`): a conversation is in a
//! project only because it was moved there or started from the
//! project's +. Where a chat happens to run decides nothing. The folder
//! is the project's default workspace, where its + starts new chats.
//! Removing a project takes it off the list and lets its chats go back
//! to the ungrouped list; no session, file or folder is deleted.

use crate::{SessionId, SessionStore, SessionSummary};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// One project row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: i64,
    /// The default workspace for the project's new chats (absolute,
    /// canonicalized when added).
    pub root: String,
    pub name: String,
    pub collapsed: bool,
}

pub(crate) fn migrate(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS projects (
             id INTEGER PRIMARY KEY,
             root TEXT NOT NULL UNIQUE,
             name TEXT NOT NULL,
             position INTEGER NOT NULL,
             collapsed INTEGER NOT NULL DEFAULT 0
         );
         -- Named one-bit settings (the Other group's fold).
         CREATE TABLE IF NOT EXISTS store_flags (
             name TEXT PRIMARY KEY
         );",
    )?;
    undo_folder_seed(connection)
}

/// A pre-release build made one project per folder that existing chats
/// ran in, flagged `projects_seeded`. Projects are the human's to make,
/// so those go: every project still carrying its folder's own name
/// (a renamed one was adopted and stays), then the flag.
fn undo_folder_seed(connection: &Connection) -> rusqlite::Result<()> {
    let seeded = connection
        .prepare("SELECT 1 FROM store_flags WHERE name = 'projects_seeded'")?
        .exists([])?;
    if !seeded {
        return Ok(());
    }
    let rows: Vec<(i64, String, String)> = {
        let mut statement = connection.prepare("SELECT id, root, name FROM projects")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (id, root, name) in rows {
        if name == default_name(Path::new(&root)) {
            connection.execute("DELETE FROM projects WHERE id = ?1", [id])?;
        }
    }
    connection.execute("DELETE FROM store_flags WHERE name = 'projects_seeded'", [])?;
    Ok(())
}

/// The default name for a folder: its last component, or the whole
/// path for a root with none.
pub fn default_name(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string())
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
                .prepare("SELECT id, root, name, collapsed FROM projects ORDER BY position, id")?;
            let rows = statement.query_map([], |row| {
                Ok(Project {
                    id: row.get(0)?,
                    root: row.get(1)?,
                    name: row.get(2)?,
                    collapsed: row.get::<_, i64>(3)? != 0,
                })
            })?;
            rows.collect()
        })
    }

    /// Adds a project for a folder, last in order, named after the
    /// folder unless a name is given. The root is canonicalized when it
    /// exists, so the same folder reached two ways is one project; a
    /// folder that is already a project returns the existing row.
    pub fn add_project(&self, root: &Path, name: Option<&str>) -> rusqlite::Result<Project> {
        let root: PathBuf = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let root_text = root.display().to_string();
        let name = name
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| default_name(&root));
        self.with_connection(|connection| {
            connection.execute(
                "INSERT INTO projects (root, name, position)
                 VALUES (?1, ?2, (SELECT COALESCE(MAX(position), 0) + 1 FROM projects))
                 ON CONFLICT(root) DO NOTHING",
                rusqlite::params![root_text, name],
            )?;
            connection.query_row(
                "SELECT id, root, name, collapsed FROM projects WHERE root = ?1",
                [&root_text],
                |row| {
                    Ok(Project {
                        id: row.get(0)?,
                        root: row.get(1)?,
                        name: row.get(2)?,
                        collapsed: row.get::<_, i64>(3)? != 0,
                    })
                },
            )
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
    /// (`None`).
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
    /// ungrouped list; sessions, files and the folder are untouched.
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
    fn membership_is_explicit_and_running_in_the_folder_files_nothing() {
        let store = SessionStore::open(Path::new(":memory:")).unwrap();
        let project = store.add_project(Path::new("/w/alpha"), None).unwrap();
        let inside = store.create_session("filed").unwrap();
        let outside = store.create_session("loose").unwrap();
        store.set_session_workspace(outside, Path::new("/w/alpha"));
        store.set_session_project(inside, Some(project.id)).unwrap();
        let projects = store.list_projects().unwrap();
        let owner = |id: SessionId| {
            let sessions = store.list_sessions().unwrap();
            let summary = sessions.iter().find(|summary| summary.id == id).unwrap();
            project_of(&projects, summary).map(|project| project.id)
        };
        assert_eq!(owner(inside), Some(project.id));
        assert_eq!(owner(outside), None);
        store.set_session_project(inside, None).unwrap();
        assert_eq!(owner(inside), None);
    }

    #[test]
    fn projects_add_rename_collapse_and_remove_without_touching_sessions() {
        let store = SessionStore::open(Path::new(":memory:")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let first = store.add_project(dir.path(), None).unwrap();
        assert_eq!(
            first.name,
            default_name(&dir.path().canonicalize().unwrap())
        );
        // The same folder again is the same project.
        assert_eq!(
            store.add_project(dir.path(), Some("again")).unwrap().id,
            first.id
        );
        let second = store
            .add_project(Path::new("/no/such/folder"), Some("Named"))
            .unwrap();
        assert_eq!(second.name, "Named");
        store.rename_project(first.id, "  Gertie  ").unwrap();
        store.rename_project(first.id, "   ").unwrap(); // blank is ignored
        store.set_project_collapsed(second.id, true).unwrap();
        let listed = store.list_projects().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "Gertie");
        assert!(listed[1].collapsed);

        let session = store.create_session("chat").unwrap();
        store.set_session_workspace(session, Path::new("/no/such/folder"));
        store.set_session_project(session, Some(second.id)).unwrap();
        store.remove_project(second.id).unwrap();
        assert_eq!(store.list_projects().unwrap().len(), 1);
        let summary = store.list_sessions().unwrap().pop().unwrap();
        assert_eq!(summary.project_id, None, "the chat is ungrouped again");
        assert_eq!(summary.workspace_root.as_deref(), Some("/no/such/folder"));
    }

    #[test]
    fn the_pre_release_folder_seed_is_undone_once_keeping_renamed_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.db");
        {
            let store = SessionStore::open(&path).unwrap();
            store.add_project(Path::new("/w/noscat"), None).unwrap();
            let kept = store.add_project(Path::new("/w/gertie"), None).unwrap();
            store.rename_project(kept.id, "Gertie app").unwrap();
            store.set_store_flag("projects_seeded", true).unwrap();
        }
        let store = SessionStore::open(&path).unwrap();
        let names: Vec<String> = store
            .list_projects()
            .unwrap()
            .into_iter()
            .map(|project| project.name)
            .collect();
        assert_eq!(names, vec!["Gertie app"]);
        assert!(!store.store_flag("projects_seeded"));
        // Once: a project added afterwards under its folder's name stays.
        store.add_project(Path::new("/w/noscat"), None).unwrap();
        drop(store);
        assert_eq!(
            SessionStore::open(&path)
                .unwrap()
                .list_projects()
                .unwrap()
                .len(),
            2
        );
    }
}
