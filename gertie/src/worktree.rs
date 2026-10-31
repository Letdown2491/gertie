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
/// Keeps `.gertie/` out of git status via the repo-local exclude —
/// never the user's `.gitignore`. Reversible and repo-private; the
/// worktrees dir and the pastes dir both live behind it.
pub fn ensure_local_exclude(repo_root: &Path) -> Result<(), String> {
    let exclude = repo_root.join(".git/info/exclude");
    if !exclude.is_file() {
        return Ok(()); // not a repo, or bare — nothing to exclude from
    }
    let contents = std::fs::read_to_string(&exclude).map_err(|error| error.to_string())?;
    if contents.lines().any(|line| line.trim() == ".gertie/") {
        return Ok(());
    }
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&exclude)
        .map_err(|error| error.to_string())?;
    writeln!(file, "\n# gertie worktrees\n.gertie/").map_err(|error| error.to_string())
}

/// Creates (or reopens) a worktree named `name`, branched from HEAD.
/// Reuses an existing worktree or branch of the same name — tab ids
/// are per-run, so a stale one from an earlier run is a legitimate
/// continuation, not a collision. Fails with git's message if the repo
/// isn't one or git is unavailable.
pub fn create(repo_root: &Path, name: &str) -> Result<PathBuf, String> {
    ensure_local_exclude(repo_root)?;

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

/// Everything a worktree tab needs to name its branch after the
/// session's first message: the worktree root and the prefix the
/// repo's overlay configured (gertie/ when it says nothing).
#[derive(Clone, Debug)]
pub struct BranchNaming {
    pub root: PathBuf,
    pub prefix: String,
}

/// The default branch prefix. The repo's overlay may set another;
/// anything git-hostile falls back to this.
const DEFAULT_PREFIX: &str = "gertie/";

/// The branch prefix a repo may configure (`[worktree]
/// branch_prefix = "feat/"` in its overlay): sane ref characters
/// only, or the default. A prefix is repo-owned policy, so it's
/// validated, not rewritten.
pub fn sane_prefix(prefix: &str) -> String {
    let ok = !prefix.is_empty()
        && !prefix.starts_with('-')
        && prefix
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '/' | '_' | '.'));
    if ok {
        prefix.to_string()
    } else {
        DEFAULT_PREFIX.to_string()
    }
}

/// The branch name from a message: lowercase, runs of
/// non-alphanumerics to single dashes, trimmed, capped at 40 — a
/// stable, readable, git-safe slug. ASCII only: a message that
/// slugifies to nothing (emoji, scripts outside ASCII) leaves the
/// branch on its temporary name rather than guessing.
pub fn slugify(message: &str) -> Option<String> {
    let mut slug = String::new();
    let mut last_was_dash = false;
    for ch in message.to_lowercase().chars() {
        if slug.len() >= 40 {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_was_dash = false;
        } else if !slug.is_empty() && !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }
    // Dashes only: '.' never survives, so the git-hostile `.lock`
    // suffix cannot occur, and a trailing dash trims off.
    let slug = slug.trim_matches('-').to_string();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() || slug == "-" {
        None
    } else {
        Some(slug)
    }
}

/// Names the worktree's branch after the session's first message:
/// `fix-the-flaky-widget` under the configured prefix. Only the
/// temporary branches gertie made (`gertie/tab-N`) are renamed — a
/// branch you checked out is yours, never ours. Any failure (the
/// slug branch already exists — two tabs opened on the same first
/// words — git refused, the root isn't a worktree) leaves the
/// temporary name standing: the work works either way.
pub fn name_branch_from_first_message(naming: &BranchNaming, message: &str) -> Result<(), String> {
    let BranchNaming { root, prefix } = naming;
    // A worktree's .git is a file; the main checkout's is a
    // directory. The main checkout's branch is never ours to rename.
    if !root.join(".git").is_file() {
        return Ok(());
    }
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .map_err(|error| error.to_string())
    };
    let current = run(&["rev-parse", "--abbrev-ref", "HEAD"])
        .map_err(|error| error.to_string())
        .and_then(|output| {
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
            }
        })?;
    // Not gertie's temporary name: nothing to do, no error.
    if !current.starts_with(DEFAULT_PREFIX) || !current.contains("/tab-") {
        return Ok(());
    }
    let Some(slug) = slugify(message) else {
        return Ok(());
    };
    let branch = format!("{}{}", sane_prefix(prefix), slug);
    if branch == current {
        return Ok(());
    }
    let renamed = run(&["branch", "-m", &branch]).and_then(|output| {
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    });
    // A failure keeps the temporary branch — say why on stderr, in
    // case someone is reading, and let the work go on.
    if let Err(error) = &renamed {
        eprintln!("warning: branch stays {current}: {error}");
    }
    renamed
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

    #[test]
    fn the_first_message_names_the_branch_the_temporary_name_stands_on_failure() {
        let repo = repo();
        let root = create(repo.path(), "tab-0").expect("worktree created");
        let naming = BranchNaming {
            root: root.clone(),
            prefix: "gertie/".into(),
        };

        name_branch_from_first_message(&naming, "Fix the flaky widget test!").expect("renamed");
        let branch = std::process::Command::new("git")
            .args(["-C"])
            .arg(&root)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branch.stdout).contains("gertie/fix-the-flaky-widget-test"),
            "the branch is the message's slug under the prefix"
        );

        // Naming again is a no-op — the branch is no longer gertie's
        // temporary one, whatever the message says.
        name_branch_from_first_message(&naming, "a second message entirely").expect("quiet");
        let branch = std::process::Command::new("git")
            .args(["-C"])
            .arg(&root)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branch.stdout).contains("gertie/fix-the-flaky-widget-test"),
            "the first message's name sticks"
        );
    }

    #[test]
    fn a_taken_slug_or_an_empty_one_leaves_the_temporary_name() {
        let repo = repo();
        let root = create(repo.path(), "tab-1").expect("worktree created");
        // A sibling branch already holds the slug the next tab's
        // first message would produce.
        std::process::Command::new("git")
            .args(["branch", "gertie/duplicate-me"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        let naming = BranchNaming {
            root: root.clone(),
            prefix: "gertie/".into(),
        };
        assert!(name_branch_from_first_message(&naming, "duplicate me").is_err());
        let branch = std::process::Command::new("git")
            .args(["-C"])
            .arg(&root)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branch.stdout).contains("gertie/tab-1"),
            "on failure the temporary name remains"
        );

        // A message with nothing to slugify (emoji, CJK) doesn't
        // rename either — the work goes on under the temporary name.
        name_branch_from_first_message(&naming, "🚀").expect("quiet");
        let branch = std::process::Command::new("git")
            .args(["-C"])
            .arg(&root)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&branch.stdout).contains("gertie/tab-1"));
    }

    #[test]
    fn the_main_checkout_is_never_renamed() {
        let repo = repo();
        // Not a worktree: .git is a directory. The naming is a no-op
        // even when the branch looks like gertie's.
        std::process::Command::new("git")
            .args(["branch", "gertie/tab-9"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "gertie/tab-9"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        let naming = BranchNaming {
            root: repo.path().to_path_buf(),
            prefix: "gertie/".into(),
        };
        name_branch_from_first_message(&naming, "rename me anyway").expect("quiet no-op");
        let branch = std::process::Command::new("git")
            .args(["-C"])
            .arg(repo.path())
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branch.stdout).contains("gertie/tab-9"),
            "the main checkout's branch is the user's"
        );
    }

    #[test]
    fn slugs_are_lowercase_dash_joined_and_capped() {
        assert_eq!(
            slugify("Fix the flaky widget test!").as_deref(),
            Some("fix-the-flaky-widget-test")
        );
        assert_eq!(
            slugify("  leading and-- trailing  ").as_deref(),
            Some("leading-and-trailing")
        );
        assert_eq!(
            slugify("LOCK the .lock file").as_deref(),
            Some("lock-the-lock-file")
        );
        let long = slugify(&"word ".repeat(20)).expect("a wordy message");
        assert!(long.len() <= 40, "the slug is capped: {long}");
        assert_eq!(slugify("🚀🎉"), None, "nothing sluggable is None");
        // A `.lock` suffix is structurally impossible: '.' slugifies
        // to a dash, so "something.lock" is "something-lock" — which
        // git accepts.
        assert_eq!(slugify("something.lock").as_deref(), Some("something-lock"));
    }

    #[test]
    fn a_hostile_prefix_falls_back_to_the_default() {
        assert_eq!(sane_prefix("feat/"), "feat/");
        assert_eq!(sane_prefix("gertie/"), "gertie/");
        assert_eq!(sane_prefix(""), "gertie/");
        assert_eq!(sane_prefix("-steal"), "gertie/");
        assert_eq!(sane_prefix("a b"), "gertie/");
        assert_eq!(sane_prefix("a~b"), "gertie/");
    }
}
