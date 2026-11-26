//! Pure helpers ChatView uses: number formatting, the mode menu's
//! rows, path display, and the @-mention matcher. No state, no gpui:
//! each carries its unit tests here instead of chat.rs's (which held
//! none of them).

use gertie_core::PermissionMode;

/// 100k (`12.3k`), whole k above (`123k`).
pub fn human_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.)
    } else if count >= 100_000 {
        format!("{:.0}k", count as f64 / 1000.)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1000.)
    } else {
        format!("{count}")
    }
}

/// the menu names the dangerous choice without dropping its gate.
pub fn mode_menu_rows(current: PermissionMode) -> Vec<(PermissionMode, &'static str, bool)> {
    use PermissionMode::*;
    [
        (Supervised, "everything asks", current == Supervised),
        (
            AcceptEdits,
            "edits run unasked, bash asks",
            current == AcceptEdits,
        ),
        (
            Auto,
            "edits and sandboxed bash run unasked",
            current == Auto,
        ),
        (FullAccess, "nothing asks, ever", current == FullAccess),
    ]
    .into_iter()
    .collect()
}

/// The replacement chain of a superseded entry, by id: whatever
/// replaced it, then whatever replaced that, in order. Empty for a
/// current entry. A cycle (a hand edit) or a missing id ends the walk.
pub fn memory_chain(entries: &[gertie_core::MemoryEntry], id: &str) -> Vec<String> {
    let mut chain: Vec<String> = Vec::new();
    let mut current = id;
    while let Some(next) = entries
        .iter()
        .find(|entry| entry.id == current)
        .and_then(|entry| entry.superseded_by.as_deref())
    {
        if next == id || chain.iter().any(|seen| seen == next) {
            break;
        }
        chain.push(next.to_string());
        current = next;
    }
    chain
}

/// `HOME`, or a path outside it: printed whole.
pub fn tildify(path: &std::path::Path) -> String {
    let home = std::env::var("HOME").ok().filter(|home| !home.is_empty());
    match home {
        Some(home) if path.starts_with(&home) => {
            let tail = path.strip_prefix(&home).unwrap_or(path);
            if tail.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", tail.display())
            }
        }
        _ => path.display().to_string(),
    }
}

/// The @-mention matcher, exposed for tests (doc'ed, not hidden: the
/// integration tests reach it through the crate's public surface).
pub fn mention_matches_for_test(input: &str, root: &std::path::Path) -> Vec<MentionMatch> {
    mention_matches(input, root)
}

/// One row of the @-mention popup: a workspace file the picker offers.
#[derive(Debug, Clone)]
pub struct MentionMatch {
    /// Path relative to the workspace root, what the agent reads by.
    pub path: String,
    /// The trailing `@query` token to erase on acceptance.
    pub token: String,
}

/// The @-mention's candidate list: files under the workspace root
/// whose relative path contains the query, directory scratch
/// (.git, target, node_modules, .gertie) excluded, capped so a big
/// repo renders a popup, not a novel. The bare `@` lists nothing
/// until a character narrows it: with thousands of files the first
/// keystroke should already mean something.
pub fn mention_matches(input: &str, root: &std::path::Path) -> Vec<MentionMatch> {
    // The token: an `@` that starts the input or follows whitespace,
    // up to the caret (the input state's value is all we see; the
    // token is the whole trailing `@...` run, mentions are typed at
    // the end like slash commands).
    let input = input.trim_end_matches('\n');
    let Some(at) = input.rfind('@') else {
        return Vec::new();
    };
    let at_line_start = at == 0
        || input[..at]
            .chars()
            .last()
            .is_some_and(|c| c.is_whitespace());
    if !at_line_start {
        return Vec::new();
    }
    let query = &input[at + 1..];
    // A query with spaces is a sentence again, not a mention.
    if query.is_empty() || query.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    let token = input[at..].to_string();
    // The walk's budget in directory entries: a repo with a vendored
    // tree or a data directory is millions of entries, and the popup
    // owes an answer now, not a complete one.
    const MAX_VISITED: usize = 50_000;
    let mut visited = 0usize;
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            visited += 1;
            if visited > MAX_VISITED {
                stack.clear();
                break;
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if file_type.is_dir() {
                if matches!(
                    name.as_str(),
                    ".git" | ".gertie" | "target" | "node_modules" | "dist"
                ) {
                    continue;
                }
                stack.push(path);
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(name.clone());
            if !relative.contains(query) {
                continue;
            }
            files.push(relative);
            if files.len() >= 64 {
                return files
                    .into_iter()
                    .map(|path| MentionMatch {
                        path,
                        token: token.clone(),
                    })
                    .collect();
            }
        }
    }
    files.sort();
    files
        .into_iter()
        .map(|path| MentionMatch {
            path,
            token: token.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, by: Option<&str>) -> gertie_core::MemoryEntry {
        gertie_core::MemoryEntry {
            id: id.into(),
            file: "decisions.md".into(),
            text: id.into(),
            helpful: 0,
            harmful: 0,
            source_session: None,
            added_at: None,
            valid_until: by.map(|_| "2026-11-26".into()),
            superseded_by: by.map(Into::into),
            links: Vec::new(),
            evidence: Vec::new(),
            recheck: Vec::new(),
        }
    }

    #[test]
    fn the_chain_follows_each_replacement_and_stops_on_a_cycle() {
        let rows = vec![
            entry("p12", Some("p31")),
            entry("p31", Some("p40")),
            entry("p40", None),
        ];
        assert_eq!(memory_chain(&rows, "p12"), vec!["p31", "p40"]);
        assert!(memory_chain(&rows, "p40").is_empty());
        let looped = vec![entry("p1", Some("p2")), entry("p2", Some("p1"))];
        assert_eq!(memory_chain(&looped, "p1"), vec!["p2"]);
    }
}
