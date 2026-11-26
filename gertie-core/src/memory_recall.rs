//! Recall on touch (#36): when the work touches a file, the memory
//! entries citing that file ride the tool result.
//!
//! Recall used to depend on the model choosing to look (the frozen
//! index, `search_memory`, `memory view`). Strong models look; weaker
//! ones mostly don't, and they gain the most from memory. So the
//! harness attaches it, the way a nested AGENTS.md loads on touch
//! (`directory_instructions`): riding a tool result keeps the cached
//! request prefix byte-stable, and the transcript shows what the model
//! was given, where.
//!
//! "Already recalled" is a scan of the history for the block's marker,
//! no other state: a compaction that drops the block lets the next
//! touch recall it again, exactly when it matters. The cost is bounded
//! three ways: only files some live entry cites, a few entries per
//! file, a few files per session.

use crate::Role;
use crate::memory::{MemoryEntry, MemoryStore};
use crate::request::WireMessage;
use std::path::Path;

/// Entries attached per file: the strongest few, not the whole file's
/// history.
pub const RECALL_PER_FILE: usize = 5;

/// Files recalled per session (per history, so per compaction window).
pub const RECALL_PER_SESSION: usize = 12;

const OPENING: &str = "[memory: entries citing ";
const CLOSING: &str = "[end memory]";

/// The block's opening line for `relative`, also its marker.
fn marker(relative: &str) -> String {
    format!("{OPENING}{relative}]")
}

/// The recall block owed to a successful call that touched `target`
/// (resolved and contained, the checkpoint hook's rules). None when
/// no live entry cites the file, the file is memory itself, the block
/// is already in `history`, or the session's recall budget is spent.
pub(crate) fn block_for(
    store: &MemoryStore,
    root: &Path,
    target: &Path,
    history: &[WireMessage],
) -> Option<String> {
    let root = root.canonicalize().ok()?;
    let target = target.canonicalize().ok()?;
    if !target.is_file() {
        return None;
    }
    let relative = target
        .strip_prefix(&root)
        .ok()?
        .components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    // Reading memory's own files is consulting it already.
    if relative.starts_with(".gertie/") {
        return None;
    }
    let marker = marker(&relative);
    let mut recalled = 0usize;
    for message in history.iter().filter(|message| message.role == Role::Tool) {
        let Some(content) = message.content.as_deref() else {
            continue;
        };
        if content.contains(&format!("\n{marker}\n")) {
            return None;
        }
        recalled += content.matches(&format!("\n{OPENING}")).count();
    }
    if recalled >= RECALL_PER_SESSION {
        return None;
    }
    let mut citing: Vec<MemoryEntry> = store
        .scan()
        .into_iter()
        .filter(|entry| entry.valid_until.is_none())
        .filter(|entry| entry.evidence.iter().any(|cited| cited.path == relative))
        .collect();
    if citing.is_empty() {
        return None;
    }
    // The strongest first: votes, then the newest.
    citing.sort_by(|a, b| {
        let score = |entry: &MemoryEntry| entry.helpful as i64 - entry.harmful as i64;
        score(b)
            .cmp(&score(a))
            .then_with(|| number(&b.id).cmp(&number(&a.id)))
    });
    let lines: Vec<String> = citing
        .iter()
        .take(RECALL_PER_FILE)
        .map(|entry| {
            let mut line = format!("- [{}] {}", entry.id, entry.text);
            if !store.changed_evidence(entry).is_empty() {
                line.push_str(
                    " (evidence changed since this was written: re-check it before relying on it)",
                );
            }
            if !entry.recheck.is_empty() {
                line.push_str(&format!(
                    " (flagged: it rests on {}, which was corrected; re-check it)",
                    entry.recheck.join(", ")
                ));
            }
            line
        })
        .collect();
    Some(format!(
        "\n\n{marker}\nMemory from earlier sessions about this file. If what you are about \
         to do contradicts an entry, name the entry and ask the user before acting, unless \
         their message already addressed it. If an entry is simply out of date, supersede it \
         with the remember tool:\n{}\n{CLOSING}",
        lines.join("\n")
    ))
}

/// The entry ids a tool result's recall blocks carry: what the harness
/// put in front of the model, so it counts as consulted.
pub fn recalled_ids(result: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut inside = false;
    for line in result.lines() {
        if line.starts_with(OPENING) {
            inside = true;
            continue;
        }
        if line.starts_with(CLOSING) {
            inside = false;
            continue;
        }
        if inside
            && let Some(rest) = line.strip_prefix("- [")
            && let Some((id, _)) = rest.split_once(']')
            && !ids.iter().any(|seen| seen == id)
        {
            ids.push(id.to_string());
        }
    }
    ids
}

fn number(id: &str) -> u32 {
    id[1..].parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::ToolCall;
    use std::sync::Arc;

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: std::path::PathBuf,
        store: MemoryStore,
    }

    fn fixture(entries: &str) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join(".gertie/memory")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/pool.rs"), "fn pool() {}\n").unwrap();
        std::fs::write(root.join("src/other.rs"), "fn other() {}\n").unwrap();
        let hash = crate::memory::blob_hash(b"fn pool() {}\n");
        std::fs::write(
            root.join(".gertie/memory/gotchas.md"),
            entries.replace("HASH", &hash),
        )
        .unwrap();
        let store = MemoryStore::open(
            root.join(".gertie/memory"),
            root.join("global"),
            Arc::new(crate::SessionStore::open(Path::new(":memory:")).unwrap()),
        );
        Fixture {
            _tmp: tmp,
            root,
            store,
        }
    }

    fn tool_row(content: &str) -> WireMessage {
        let call = ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        };
        WireMessage::tool_result(&call, content)
    }

    const ENTRIES: &str = "\
- [p1] helpful=0 harmful=0 the pool is sized for postgres [evidence: src/pool.rs@HASH] [source: added: 2026-11-01]
- [p2] helpful=3 harmful=0 warm the pool at boot [evidence: src/pool.rs@HASH] [source: added: 2026-11-01]
- [p3] helpful=0 harmful=0 old pool advice [evidence: src/pool.rs@HASH] [source: added: 2026-11-01] [valid until: 2026-11-02]
- [p4] helpful=0 harmful=0 the pool once had 4 slots [evidence: src/pool.rs@000000000000] [recheck: p9] [source: added: 2026-11-01]
- [p5] helpful=0 harmful=0 about another file [evidence: src/other.rs@000000000000] [source: added: 2026-11-01]
";

    #[test]
    fn touching_a_cited_file_recalls_its_live_entries_strongest_first() {
        let f = fixture(ENTRIES);
        let block = block_for(&f.store, &f.root, &f.root.join("src/pool.rs"), &[]).unwrap();
        assert!(
            block.contains("[memory: entries citing src/pool.rs]"),
            "{block}"
        );
        let ids = recalled_ids(&block);
        assert_eq!(ids[0], "p2", "votes first: {ids:?}");
        assert!(ids.contains(&"p1".to_string()) && ids.contains(&"p4".to_string()));
        assert!(!ids.contains(&"p3".to_string()), "superseded stays out");
        assert!(
            !ids.contains(&"p5".to_string()),
            "another file's entry stays out"
        );
        let p4 = block
            .lines()
            .find(|line| line.starts_with("- [p4]"))
            .unwrap();
        assert!(
            p4.contains("evidence changed") && p4.contains("rests on p9"),
            "{p4}"
        ); // A remembered failure must stop the model, not footnote the
        // edit (the heed eval: "recall, not rules" read as permission).
        assert!(
            block.contains("contradicts an entry") && block.contains("ask the user before acting"),
            "{block}"
        );
    }

    #[test]
    fn nothing_is_attached_without_a_citing_entry_or_for_memory_itself() {
        let f = fixture(ENTRIES);
        std::fs::write(f.root.join("src/plain.rs"), "x").unwrap();
        assert!(block_for(&f.store, &f.root, &f.root.join("src/plain.rs"), &[]).is_none());
        let memory_file = f.root.join(".gertie/memory/gotchas.md");
        assert!(block_for(&f.store, &f.root, &memory_file, &[]).is_none());
    }

    #[test]
    fn a_file_is_recalled_once_and_the_session_budget_holds() {
        let f = fixture(ENTRIES);
        let target = f.root.join("src/pool.rs");
        let block = block_for(&f.store, &f.root, &target, &[]).unwrap();
        let history = vec![tool_row(&format!("file text{block}"))];
        assert!(
            block_for(&f.store, &f.root, &target, &history).is_none(),
            "once"
        );
        let spent: String = (0..RECALL_PER_SESSION)
            .map(|index| format!("\n{OPENING}f{index}.rs]\n- [p1] x\n{CLOSING}"))
            .collect();
        let history = vec![tool_row(&spent)];
        assert!(
            block_for(&f.store, &f.root, &target, &history).is_none(),
            "budget"
        );
    }

    #[test]
    fn the_per_file_cap_holds() {
        let many: String = (1..=9)
            .map(|index| {
                format!(
                    "- [p{index}] helpful=0 harmful=0 fact {index} [evidence: src/pool.rs@HASH] [source: added: 2026-11-01]\n"
                )
            })
            .collect();
        let f = fixture(&many);
        let block = block_for(&f.store, &f.root, &f.root.join("src/pool.rs"), &[]).unwrap();
        assert_eq!(recalled_ids(&block).len(), RECALL_PER_FILE);
        assert_eq!(recalled_ids(&block)[0], "p9", "newest first among equals");
    }

    #[test]
    fn recalled_ids_read_only_inside_the_block() {
        let text = "- [p7] a line of the file itself\n\n[memory: entries citing a.rs]\n- [p2] x\n[end memory]";
        assert_eq!(recalled_ids(text), vec!["p2".to_string()]);
    }
}
