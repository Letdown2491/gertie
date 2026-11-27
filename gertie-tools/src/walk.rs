//! The one workspace walk: grep, glob, the repo map and the composer's
//! @-mentions all list files the same way. Symlinks are never followed
//! (a link out of the workspace would list, or loop over, a tree the
//! permission gate never saw), and build output, dependencies and VCS
//! directories are never entered below the walk's base.

use std::path::Path;

/// Directories never entered below the base: build output,
/// dependencies, VCS, caches, gertie's own state. A walk *based* in one
/// (grep with `path: "target"`, glob `target/**`) still lists it: the
/// skip is for wandering in, not for asking.
pub const SKIPPED_DIRS: [&str; 12] = [
    ".git",
    ".gertie",
    "target",
    "node_modules",
    "vendor",
    "dist",
    "build",
    "__pycache__",
    ".venv",
    ".pixi",
    ".mypy_cache",
    ".pytest_cache",
];

/// The files under `base`, sorted by name within each directory.
/// `hidden: false` also skips every dot-named entry below the base.
pub fn files(base: &Path, hidden: bool) -> impl Iterator<Item = walkdir::DirEntry> {
    walkdir::WalkDir::new(base)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(move |entry| {
            // Depth 0 is the base itself (a tempdir's name starts with
            // '.'), and it must never filter itself out.
            entry.depth() == 0 || {
                let name = entry.file_name().to_string_lossy();
                (hidden || !name.starts_with('.'))
                    && !(entry.file_type().is_dir() && SKIPPED_DIRS.contains(&name.as_ref()))
            }
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
}

/// `path` relative to `root`, '/'-separated on every platform. None
/// when `path` is not under `root`.
pub fn relative(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    Some(
        relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// The leading components of a glob pattern that hold no wildcard: the
/// directory a glob walk can start from instead of the root.
pub fn literal_prefix(pattern: &str) -> &str {
    let mut end = 0;
    for (index, part) in pattern.split('/').enumerate() {
        if part.contains(['*', '?', '[', ']', '{', '}']) {
            break;
        }
        end = if index == 0 {
            part.len()
        } else {
            end + 1 + part.len()
        };
    }
    // Only whole directories: a wildcard-free pattern is a file path,
    // and its parent is the base.
    if end == pattern.len() {
        return pattern.rsplit_once('/').map_or("", |(dir, _)| dir);
    }
    pattern[..end].trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_prefixes_stop_at_the_first_wildcard() {
        assert_eq!(literal_prefix("src/**/*.rs"), "src");
        assert_eq!(literal_prefix("**/*.rs"), "");
        assert_eq!(literal_prefix("a/b/c*.rs"), "a/b");
        assert_eq!(literal_prefix("a/b/c.rs"), "a/b");
        assert_eq!(literal_prefix("c.rs"), "");
    }

    #[test]
    fn the_walk_skips_build_output_but_not_its_own_base() {
        let tmp = tempfile::tempdir().unwrap();
        for path in [
            "src/a.rs",
            "target/debug/x.rs",
            ".hidden/h.rs",
            "node_modules/m.js",
        ] {
            let path = tmp.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let names = |base: &Path, hidden| {
            files(base, hidden)
                .filter_map(|entry| relative(tmp.path(), entry.path()))
                .collect::<Vec<_>>()
        };
        assert_eq!(names(tmp.path(), false), vec!["src/a.rs"]);
        assert_eq!(names(tmp.path(), true), vec![".hidden/h.rs", "src/a.rs"]);
        assert_eq!(
            names(&tmp.path().join("target"), false),
            vec!["target/debug/x.rs"]
        );
    }
}
