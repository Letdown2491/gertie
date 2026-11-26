//! Directory instructions: an `AGENTS.md` below the workspace root,
//! loaded the first time a tool call touches a path under it.
//!
//! The root and global files are framing, sent on every request. A
//! nested file is not: most turns never go near `migrations/`, so its
//! rules ride the result of the call that first reaches that
//! directory. Riding a tool result keeps the cached request prefix
//! byte-stable (loading one costs no cache miss), and it makes the
//! record honest: the transcript shows what the model was given, where
//! it was given it.
//!
//! "Already loaded" is a scan of the history for the block's marker,
//! no other state. A compaction that drops the block drops the marker
//! with it, so the next touch under that directory loads it again,
//! exactly when the rules matter. A resumed session follows the same
//! rule.

use crate::Role;
use crate::request::WireMessage;
use std::path::{Path, PathBuf};

/// The per-file cap for every instruction file, root, global and
/// nested alike: enough for a page of rules, small enough that a
/// hostile 500 KB file costs nothing.
pub const INSTRUCTIONS_CAP: usize = 2048;

const FILE_NAME: &str = "AGENTS.md";

/// The block's opening line for the file at `relative` (a
/// workspace-relative path with `/` separators). Also the "already
/// loaded" marker, so it names the file exactly.
fn marker(relative: &str) -> String {
    format!("[directory instructions: {relative}]")
}

/// The instruction blocks owed to the call whose arguments name
/// `target` (already resolved and contained, the checkpoint hook's
/// rules), outermost directory first, each in its marker block. None
/// when the path is at the root, nothing nested exists, or every file
/// found is already in `history`.
pub(crate) fn blocks_for(root: &Path, target: &Path, history: &[WireMessage]) -> Option<String> {
    let root = root.canonicalize().ok()?;
    let start = if target.is_dir() {
        target.to_path_buf()
    } else {
        target.parent()?.to_path_buf()
    };
    // Directories strictly below the root, innermost first. The root's
    // own file is framing already; anything above it is not ours.
    let mut directories: Vec<PathBuf> = Vec::new();
    let mut directory = start.as_path();
    while directory != root && directory.starts_with(&root) {
        directories.push(directory.to_path_buf());
        directory = directory.parent()?;
    }
    let mut blocks = String::new();
    for directory in directories.iter().rev() {
        let path = directory.join(FILE_NAME);
        if !path.is_file() {
            continue;
        }
        // A symlinked AGENTS.md pointing out of the workspace is not
        // the workspace's instructions.
        let Ok(resolved) = path.canonicalize() else {
            continue;
        };
        if !resolved.starts_with(&root) {
            continue;
        }
        let Ok(relative) = path.strip_prefix(&root) else {
            continue;
        };
        let relative = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let marker = marker(&relative);
        if already_loaded(history, &marker) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&resolved) else {
            continue;
        };
        let text = crate::agent::sanitize_untrusted(&text, INSTRUCTIONS_CAP);
        let scope = relative.strip_suffix(FILE_NAME).unwrap_or(&relative);
        blocks.push_str(&format!(
            "\n\n{marker}\nThese bind work on files under {scope}.\n{}\n[end directory instructions]",
            text.trim_end()
        ));
    }
    if blocks.is_empty() {
        None
    } else {
        Some(blocks)
    }
}

/// Whether a tool result in `history` already carries the block that
/// opens with `marker`. Only tool rows count: a user quoting the
/// marker does not load anything.
fn already_loaded(history: &[WireMessage], marker: &str) -> bool {
    let opening = format!("\n{marker}\n");
    history.iter().any(|message| {
        message.role == Role::Tool
            && message
                .content
                .as_deref()
                .is_some_and(|content| content.contains(&opening))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::ToolCall;

    fn tool_row(content: &str) -> WireMessage {
        let call = ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        };
        WireMessage::tool_result(&call, content)
    }

    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "root rules\n").unwrap();
        std::fs::write(root.join("a/AGENTS.md"), "outer rules\n").unwrap();
        std::fs::write(root.join("a/b/AGENTS.md"), "inner rules\n").unwrap();
        std::fs::write(root.join("a/b/x.rs"), "fn main() {}\n").unwrap();
        (tmp, root)
    }

    #[test]
    fn a_touch_loads_every_nested_file_outermost_first() {
        let (_tmp, root) = workspace();
        let blocks = blocks_for(&root, &root.join("a/b/x.rs"), &[]).unwrap();
        let outer = blocks
            .find("[directory instructions: a/AGENTS.md]")
            .unwrap();
        let inner = blocks
            .find("[directory instructions: a/b/AGENTS.md]")
            .unwrap();
        assert!(outer < inner);
        assert!(blocks.contains("outer rules"));
        assert!(blocks.contains("inner rules"));
        assert!(blocks.contains("These bind work on files under a/b/."));
        // The root's file is framing, never a block.
        assert!(!blocks.contains("root rules"));
    }

    #[test]
    fn a_loaded_file_is_not_loaded_again_until_its_block_is_gone() {
        let (_tmp, root) = workspace();
        let first = blocks_for(&root, &root.join("a/b/x.rs"), &[]).unwrap();
        let history = vec![tool_row(&format!("fn main() {{}}{first}"))];
        assert_eq!(blocks_for(&root, &root.join("a/b/x.rs"), &history), None);
        // A sibling under a/ owes nothing new either.
        assert_eq!(blocks_for(&root, &root.join("a/y.rs"), &history), None);
        // Compaction dropped the row: the next touch loads again.
        assert!(blocks_for(&root, &root.join("a/b/x.rs"), &[]).is_some());
    }

    #[test]
    fn only_a_tool_row_counts_as_loaded() {
        let (_tmp, root) = workspace();
        let quoted = WireMessage::user("\n[directory instructions: a/AGENTS.md]\n");
        let blocks = blocks_for(&root, &root.join("a/y.rs"), &[quoted]).unwrap();
        assert!(blocks.contains("outer rules"));
    }

    #[test]
    fn a_directory_target_counts_from_itself() {
        let (_tmp, root) = workspace();
        let blocks = blocks_for(&root, &root.join("a/b"), &[]).unwrap();
        assert!(blocks.contains("inner rules"));
    }

    #[test]
    fn root_level_paths_and_bare_directories_owe_nothing() {
        let (_tmp, root) = workspace();
        std::fs::create_dir(root.join("plain")).unwrap();
        assert_eq!(blocks_for(&root, &root.join("README.md"), &[]), None);
        assert_eq!(blocks_for(&root, &root.join("plain/z.rs"), &[]), None);
    }

    #[test]
    fn a_hostile_file_is_capped_and_cleaned() {
        let (_tmp, root) = workspace();
        let hostile = format!("ok\u{200B}{}", "x".repeat(500_000));
        std::fs::write(root.join("a/AGENTS.md"), hostile).unwrap();
        let blocks = blocks_for(&root, &root.join("a/y.rs"), &[]).unwrap();
        assert!(!blocks.contains('\u{200B}'));
        assert!(blocks.matches('x').count() <= INSTRUCTIONS_CAP);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_workspace_is_not_read() {
        let (_tmp, root) = workspace();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("AGENTS.md"), "foreign rules\n").unwrap();
        std::fs::create_dir(root.join("c")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("AGENTS.md"), root.join("c/AGENTS.md"))
            .unwrap();
        assert_eq!(blocks_for(&root, &root.join("c/z.rs"), &[]), None);
    }
}
