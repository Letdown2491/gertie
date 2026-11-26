//! Projects: named folders the sidebar groups conversations under.
//!
//! A project is a folder and a name, nothing more. A conversation
//! belongs to the project whose root is the longest prefix of the
//! conversation's `workspace_root`, so a worktree chat (its checkout
//! lives under `<repo>/.gertie/worktrees/`) lands under its repo with
//! no bookkeeping, and a folder nested inside another project's folder
//! can be its own project. Removing a project only takes it off the
//! list: no session, file or folder is touched, and its conversations
//! fall back to the "Other" group.

use crate::{SessionStore, SessionSummary};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// One project row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: i64,
    /// The folder, as recorded (absolute, canonicalized when added).
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
         -- One-time store steps, by name (the projects seed today).
         CREATE TABLE IF NOT EXISTS store_flags (
             name TEXT PRIMARY KEY
         );",
    )
}

/// The default name for a folder: its last component, or the whole
/// path for a root with none.
pub fn default_name(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string())
}

/// Which project a conversation belongs to: the project whose root is
/// the longest component-wise prefix of `workspace_root`. None for a
/// conversation with no recorded folder or one outside every project.
pub fn project_for<'a>(
    projects: &'a [Project],
    workspace_root: Option<&str>,
) -> Option<&'a Project> {
    let root = Path::new(workspace_root?);
    projects
        .iter()
        .filter(|project| root.starts_with(&project.root))
        .max_by_key(|project| Path::new(&project.root).components().count())
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

    /// Adds a folder as a project, last in order, named after the
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

    /// Takes a project off the list. Sessions, files and the folder are
    /// untouched; the conversations show under "Other" until the folder
    /// is added again.
    pub fn remove_project(&self, id: i64) -> rusqlite::Result<()> {
        self.with_connection(|connection| {
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

    /// The upgrade step, once per store: every distinct folder the
    /// existing conversations ran in becomes its own project, oldest
    /// first, except `home` (a chat that ran in the home folder is not
    /// a project, it stays under "Other") and worktree checkouts (they
    /// belong to their repo). Returns how many projects it created.
    pub fn seed_projects(&self, home: Option<&Path>) -> rusqlite::Result<usize> {
        let seeded: bool = self.with_connection(|connection| {
            connection
                .prepare("SELECT 1 FROM store_flags WHERE name = 'projects_seeded'")?
                .exists([])
        })?;
        if seeded {
            return Ok(0);
        }
        let mut roots: Vec<String> = Vec::new();
        let mut sessions = self.list_sessions()?;
        // list_sessions is newest first; seed in the order work began.
        sessions.reverse();
        for SessionSummary { workspace_root, .. } in sessions {
            let Some(root) = workspace_root else {
                continue;
            };
            let path = Path::new(&root);
            // The home folder reached through a symlink (/home vs
            // /var/home) is still the home folder.
            let is_home = home.is_some_and(|home| {
                path == home
                    || matches!(
                        (path.canonicalize(), home.canonicalize()),
                        (Ok(a), Ok(b)) if a == b
                    )
            });
            if is_home || root.contains("/.gertie/worktrees/") {
                continue;
            }
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        let mut created = 0;
        for root in &roots {
            let before = self.list_projects()?.len();
            self.add_project(Path::new(root), None)?;
            if self.list_projects()?.len() > before {
                created += 1;
            }
        }
        self.with_connection(|connection| {
            connection.execute(
                "INSERT OR IGNORE INTO store_flags (name) VALUES ('projects_seeded')",
                [],
            )
        })?;
        Ok(created)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: i64, root: &str) -> Project {
        Project {
            id,
            root: root.into(),
            name: default_name(Path::new(root)),
            collapsed: false,
        }
    }

    #[test]
    fn a_chat_belongs_to_the_longest_matching_root() {
        let projects = vec![
            project(1, "/w/repo"),
            project(2, "/w/repo/sub"),
            project(3, "/w/other"),
        ];
        let id = |root: Option<&str>| project_for(&projects, root).map(|project| project.id);
        assert_eq!(id(Some("/w/repo")), Some(1));
        assert_eq!(id(Some("/w/repo/.gertie/worktrees/tab-1")), Some(1));
        assert_eq!(id(Some("/w/repo/sub/x")), Some(2));
        // Component-wise: /w/repository is not under /w/repo.
        assert_eq!(id(Some("/w/repository")), None);
        assert_eq!(id(None), None);
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
        store.remove_project(second.id).unwrap();
        assert_eq!(store.list_projects().unwrap().len(), 1);
        assert_eq!(
            store.session_workspace(session).as_deref(),
            Some("/no/such/folder"),
            "the conversation keeps its folder"
        );
    }

    #[test]
    fn the_seed_makes_one_project_per_folder_once() {
        let store = SessionStore::open(Path::new(":memory:")).unwrap();
        for root in [
            "/w/noscat",
            "/w/gertie",
            "/w/noscat",
            "/home/me",
            "/w/gertie/.gertie/worktrees/tab-2",
        ] {
            let session = store.create_session("chat").unwrap();
            store.set_session_workspace(session, Path::new(root));
        }
        store.create_session("never sent").unwrap();
        assert_eq!(store.seed_projects(Some(Path::new("/home/me"))).unwrap(), 2);
        let names: Vec<String> = store
            .list_projects()
            .unwrap()
            .into_iter()
            .map(|project| project.name)
            .collect();
        assert_eq!(names, vec!["noscat", "gertie"]);
        // Once: a removed project is not resurrected by the next launch.
        let noscat = store.list_projects().unwrap()[0].id;
        store.remove_project(noscat).unwrap();
        assert_eq!(store.seed_projects(Some(Path::new("/home/me"))).unwrap(), 0);
        assert_eq!(store.list_projects().unwrap().len(), 1);
    }
}
