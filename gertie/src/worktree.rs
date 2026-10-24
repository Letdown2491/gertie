//! Per-tab git worktrees: a parallel chat that wants to edit files
//! without colliding with the tab next door works in its own worktree
//! of the same repo. Worktrees live under `<repo>/.gertie/worktrees/`
//! — kept out of git via the local exclude (`.git/info/exclude`),
//! never the user's `.gitignore` — and persist across runs: agent work
//! is never deleted behind your back. Prune stale ones with
//! `git worktree remove`.

use std::path::{Path, PathBuf};

/// Creates (or reopens) a worktree named `name`, branched from HEAD.
/// Reuses an existing worktree or branch of the same name — tab ids
/// are per-run, so a stale one from an earlier run is a legitimate
/// continuation, not a collision. Fails with git's message if the repo
/// isn't one or git is unavailable.
pub fn create(repo_root: &Path, name: &str) -> Result<PathBuf, String> {
    // The worktrees dir must never show up as untracked noise in the
    // main checkout; the local exclude is repo-private and reversible.
    let exclude = repo_root.join(".git/info/exclude");
    if exclude.is_file() {
        let contents = std::fs::read_to_string(&exclude).map_err(|error| error.to_string())?;
        if !contents.lines().any(|line| line.trim() == ".gertie/") {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&exclude)
                .map_err(|error| error.to_string())?;
            writeln!(file, "\n# gertie worktrees\n.gertie/").map_err(|error| error.to_string())?;
        }
    }

    let target = repo_root.join(".gertie/worktrees").join(name);
    if target.join(".git").is_file() {
        return Ok(target); // already a worktree — reuse it
    }
    std::fs::create_dir_all(target.parent().expect("worktrees dir has a parent"))
        .map_err(|error| error.to_string())?;

    let branch = format!("gertie/{name}");
    let fresh = std::process::Command::new("git")
        .args(["worktree", "add", "-b", &branch])
        .arg(&target)
        .arg("HEAD")
        .current_dir(repo_root)
        .output();
    match fresh {
        Ok(output) if output.status.success() => Ok(target),
        Ok(output) => {
            // The branch may already exist from an earlier run: attach
            // to it instead of failing. If that also fails, report the
            // original error — it names the real problem.
            let attached = std::process::Command::new("git")
                .args(["worktree", "add"])
                .arg(&target)
                .arg(&branch)
                .current_dir(repo_root)
                .output();
            match attached {
                Ok(output) if output.status.success() => Ok(target),
                _ => Err(String::from_utf8_lossy(&output.stderr).trim().to_string()),
            }
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real one-commit git repo; worktree add branches from HEAD.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("git runs")
        };
        run(&["init"]);
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
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
        dir
    }

    #[test]
    fn creates_a_worktree_branched_from_head() {
        let repo = repo();
        let root = create(repo.path(), "tab-0").expect("worktree created");

        assert!(root.join(".git").is_file(), "a worktree has a .git file");
        assert!(root.join("f.txt").is_file(), "the checkout has the files");
        let branch = std::process::Command::new("git")
            .args(["-C"])
            .arg(&root)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branch.stdout).contains("gertie/tab-0"),
            "the worktree is on its own branch"
        );
        // The main checkout never sees the worktrees dir.
        let exclude = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
        assert!(exclude.lines().any(|line| line.trim() == ".gertie/"));
        assert!(
            !repo.path().join(".gitignore").exists(),
            "the user's .gitignore is untouched"
        );
    }

    #[test]
    fn reuses_an_existing_worktree_of_the_same_name() {
        let repo = repo();
        let first = create(repo.path(), "tab-0").expect("first");
        // Uncommitted work marks the worktree as a real workspace —
        // reusing it must not clobber anything.
        std::fs::write(first.join("f.txt"), "changed").unwrap();

        let second = create(repo.path(), "tab-0").expect("second");
        assert_eq!(first, second);
        assert_eq!(
            std::fs::read_to_string(second.join("f.txt")).unwrap(),
            "changed",
            "reopening keeps the agent's work"
        );
    }

    #[test]
    fn a_non_repo_is_a_clear_failure() {
        let dir = tempfile::tempdir().unwrap();
        assert!(create(dir.path(), "tab-0").is_err());
    }
}
