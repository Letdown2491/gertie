//! Memory tools: the model's window on long-term memory. The store
//! (gertie-core) keeps markdown files as the record and a SQLite
//! sidecar as the derived index; these tools serve both. Writes stay
//! inside the two tier roots, and the posture is advisory: memory is
//! recall, never policy, so the tools neither prompt nor checkpoint
//! (the M6 plan's call), they only ever touch memory.

use crate::{Tool, ToolResult, blocking};
use gertie_core::memory::Tier;
use serde::Deserialize;
use serde_json::json;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

/// The tier a model path addresses: its leading `project/` or
/// `global/` segment, defaulting to project. A bare `project` or
/// `global` names that tier's root.
fn split_tier(path: &str) -> (Tier, &str) {
    if let Some(rest) = path.strip_prefix("project/") {
        (Tier::Project, rest)
    } else if let Some(rest) = path.strip_prefix("global/") {
        (Tier::Global, rest)
    } else if path == "global" {
        (Tier::Global, "")
    } else if path == "project" {
        (Tier::Project, "")
    } else {
        (Tier::Project, path)
    }
}

/// The tier prefix the model spelled, if any. Rename inherits the
/// source tier when the target spells none.
fn explicit_tier(path: &str) -> Option<(Tier, &str)> {
    if let Some(rest) = path.strip_prefix("project/") {
        Some((Tier::Project, rest))
    } else {
        path.strip_prefix("global/")
            .map(|rest| (Tier::Global, rest))
    }
}

/// Resolves a tier-relative path under the tier root with the same
/// canonicalize-and-contain rule every path tool uses (`resolve` in
/// lib.rs). Absolute paths are a spec violation here: the tier is
/// named by the leading segment, whatever an absolute path points at.
fn resolve_in(root: &Path, rel: &str, given: &str) -> Result<PathBuf, String> {
    if Path::new(rel).is_absolute() {
        return Err(format!("memory: path must be tier-relative: {given}"));
    }
    crate::resolve(root, rel).map_err(|error| format!("memory: {error}"))
}

fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
}

/// MEMORY.md is the tier's derived index: the tool refuses to create,
/// edit, insert into, or delete it, and refuses it as a rename target.
/// Only the audit trail writes it.
fn refuse_memory_md(path: &Path) -> Result<(), String> {
    if file_name(path) == "MEMORY.md" {
        return Err(
            "memory: MEMORY.md is the derived index, never hand-authored through this tool".into(),
        );
    }
    Ok(())
}

#[derive(Deserialize)]
struct MemoryArgs {
    command: String,
    path: String,
    /// create: the file's whole content, sanitized at the entrance.
    #[serde(default)]
    content: Option<String>,
    /// str_replace: the exact anchor, must occur exactly once.
    #[serde(default)]
    old_str: Option<String>,
    /// str_replace and insert: the replacement or inserted text.
    #[serde(default)]
    new_str: Option<String>,
    /// insert: the 1-based line the new text lands after.
    #[serde(default)]
    insert_line: Option<u64>,
    /// rename: the target name, same tier.
    #[serde(default)]
    new_path: Option<String>,
    /// view: `start:end`, 1-based, the end optional (through EOF).
    #[serde(default)]
    view_range: Option<String>,
}

/// The model's read and write surface over long-term memory: the six
/// commands (view, create, str_replace, insert, delete, rename) across
/// both tiers. Reads touch only the disk; every mutating command
/// appends the audit line to the tier's MEMORY.md and syncs the
/// sidecar, rusqlite failures surface as tool errors.
pub struct Memory {
    store: Arc<gertie_core::MemoryStore>,
}

impl Memory {
    pub fn new(store: Arc<gertie_core::MemoryStore>) -> Self {
        Self { store }
    }
}

impl Tool for Memory {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn description(&self) -> String {
        "Read and write long-term memory. Two tiers, chosen by the \
         path's leading segment: `project/` (the repo's \
         `.gertie/memory/`, versioned with the clone) and `global/` \
         (the user's data dir, follows them across projects). Topic \
         files hold one-line markdown entries; MEMORY.md is a derived \
         index, never create, edit, or delete it. Memory is advisory: \
         recall, not policy. The index in your context was snapshotted \
         at session start, edits surface next session."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "enum": ["view", "create", "str_replace", "insert", "delete", "rename"],
                    "description": "view lists a tier root or reads a file; create writes new content; str_replace replaces an exact single match; insert adds text after a line; delete removes a file; rename moves within the tier"
                },
                "path": {
                    "type": "string",
                    "description": "tier-relative: `project/<file>.md`, `global/<file>.md`, or a bare `project`/`global` to view that tier's root"
                },
                "content": {"type": "string", "description": "create: the file's full content"},
                "old_str": {"type": "string", "description": "str_replace: exact text to replace, must match exactly once"},
                "new_str": {"type": "string", "description": "str_replace: replacement text; insert: text placed after insert_line"},
                "insert_line": {"type": "integer", "description": "insert: 1-based line the text lands after"},
                "new_path": {"type": "string", "description": "rename: target name within the same tier"},
                "view_range": {"type": "string", "description": "view: `start:end` 1-based lines, the end optional"}
            },
            "required": ["command", "path"]
        })
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        let store = self.store.clone();
        let arguments = arguments.to_string();
        blocking(move || {
            let args: MemoryArgs = crate::parse(&arguments, "memory")?;
            let (tier, rel) = split_tier(&args.path);
            let root = store.root(tier);
            // The tier root is created on demand: a fresh boot has no
            // memory yet, and resolve needs the root on disk.
            std::fs::create_dir_all(&root)
                .map_err(|error| format!("memory: {}: {error}", root.display()))?;
            let path = resolve_in(&root, rel, &args.path)?;
            match args.command.as_str() {
                "view" => view(tier, &path, args.view_range.as_deref()),
                "create" => mutate(&store, tier, &args.path, "create", || create(&path, &args)),
                "str_replace" => mutate(&store, tier, &args.path, "str_replace", || {
                    str_replace(&path, &args)
                }),
                "insert" => mutate(&store, tier, &args.path, "insert", || insert(&path, &args)),
                "delete" => mutate(&store, tier, &args.path, "delete", || delete(&path)),
                "rename" => mutate(&store, tier, &args.path, "rename", || {
                    rename(&store, tier, &path, &args)
                }),
                other => Err(format!(
                    "memory: unknown command {other:?}; one of view, create, str_replace, \
                     insert, delete, rename"
                )),
            }
        })
    }
}

/// view: a root or directory lists its `.md` files (one line each,
/// subdirectories with a trailing slash); a file returns numbered
/// lines, `view_range` trims to `start:end`.
fn view(tier: Tier, path: &Path, view_range: Option<&str>) -> ToolResult {
    if path.is_dir() {
        let mut listed: Vec<String> = std::fs::read_dir(path)
            .map_err(|error| format!("memory: {error}"))?
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                if entry.path().is_dir() {
                    return Some(format!("- {}/{}/", tier.name(), name));
                }
                let is_md = entry.path().extension().and_then(|e| e.to_str()) == Some("md");
                is_md.then(|| format!("- {}/{}", tier.name(), name))
            })
            .collect();
        listed.sort();
        return Ok(if listed.is_empty() {
            "no memory files yet".into()
        } else {
            listed.join("\n")
        });
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("memory: {}: {error}", path.display()))?;
    let lines: Vec<&str> = text.lines().collect();
    let (start, end) = match view_range {
        Some(range) => parse_view_range(range, lines.len())?,
        None => (1usize, lines.len()),
    };
    let numbered: Vec<String> = lines
        .iter()
        .enumerate()
        .skip(start - 1)
        .take(end - start + 1)
        .map(|(index, line)| format!("{:>6}  {line}", index + 1))
        .collect();
    Ok(crate::limit(numbered.join("\n")))
}

/// `start:end`, 1-based, the end optional. An off-range or reversed
/// range is an argument error, the model corrects it.
fn parse_view_range(range: &str, len: usize) -> Result<(usize, usize), String> {
    let (start, end) = range
        .split_once(':')
        .ok_or_else(|| format!("memory: view_range is `start:end`, got {range:?}"))?;
    let start: usize = start
        .trim()
        .parse()
        .map_err(|_| format!("memory: view_range start must be a line number, got {start:?}"))?;
    let end: usize = if end.trim().is_empty() {
        len
    } else {
        end.trim()
            .parse()
            .map_err(|_| format!("memory: view_range end must be a line number, got {end:?}"))?
    };
    if start == 0 || end < start || start > len {
        return Err(format!(
            "memory: view_range {start}:{end} is outside the file ({len} lines)"
        ));
    }
    Ok((start, end.min(len)))
}

/// create: the file must not exist yet, parents are created, the whole
/// content passes the sanitizer (one entrance, one cap).
fn create(path: &Path, args: &MemoryArgs) -> ToolResult {
    refuse_memory_md(path)?;
    let content = args
        .content
        .as_deref()
        .ok_or_else(|| "memory: create needs content".to_string())?;
    if path.exists() {
        return Err(format!("memory: {} already exists", path.display()));
    }
    let content = gertie_core::sanitize_untrusted(content, 8192);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("memory: {}: {error}", parent.display()))?;
    }
    std::fs::write(path, &content)
        .map_err(|error| format!("memory: {}: {error}", path.display()))?;
    Ok(format!("created {}", path.display()))
}

/// str_replace: the anchor must occur exactly once, so the edit can
/// never land somewhere the model did not look at.
fn str_replace(path: &Path, args: &MemoryArgs) -> ToolResult {
    refuse_memory_md(path)?;
    let old_str = args
        .old_str
        .as_deref()
        .ok_or_else(|| "memory: str_replace needs old_str".to_string())?;
    let new_str = args
        .new_str
        .as_deref()
        .ok_or_else(|| "memory: str_replace needs new_str".to_string())?;
    if old_str.is_empty() {
        return Err("memory: old_str is empty".into());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("memory: {}: {error}", path.display()))?;
    let count = text.matches(old_str).count();
    if count == 0 {
        return Err(format!(
            "memory: old_str not found in {}, read the file and anchor on exact text",
            path.display()
        ));
    }
    if count > 1 {
        return Err(format!(
            "memory: old_str matches {count} times in {}; include more surrounding text",
            path.display()
        ));
    }
    let edited = text.replacen(old_str, &gertie_core::sanitize_untrusted(new_str, 8192), 1);
    std::fs::write(path, &edited)
        .map_err(|error| format!("memory: {}: {error}", path.display()))?;
    Ok(format!("edited {}", path.display()))
}

/// insert: the new text lands after the given 1-based line, so a
/// merge is always a splice, never a whole-file rewrite.
fn insert(path: &Path, args: &MemoryArgs) -> ToolResult {
    refuse_memory_md(path)?;
    let new_str = args
        .new_str
        .as_deref()
        .ok_or_else(|| "memory: insert needs new_str".to_string())?;
    let at = args
        .insert_line
        .ok_or_else(|| "memory: insert needs insert_line".to_string())? as usize;
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("memory: {}: {error}", path.display()))?;
    let lines: Vec<&str> = text.lines().collect();
    if at == 0 || at > lines.len() {
        return Err(format!(
            "memory: insert_line {at} is outside the file ({} lines); it is the 1-based line \
             the new text lands after",
            lines.len()
        ));
    }
    let new_str = gertie_core::sanitize_untrusted(new_str, 8192);
    let inserted: Vec<&str> = if new_str.is_empty() {
        vec![""]
    } else {
        new_str.lines().collect()
    };
    let mut edited: Vec<&str> = Vec::with_capacity(lines.len() + inserted.len());
    edited.extend_from_slice(&lines[..at]);
    edited.extend_from_slice(&inserted);
    edited.extend_from_slice(&lines[at..]);
    let mut out = edited.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write(path, out).map_err(|error| format!("memory: {}: {error}", path.display()))?;
    Ok(format!("inserted after line {at}"))
}

/// delete: remove one file, the index is untouchable.
fn delete(path: &Path) -> ToolResult {
    refuse_memory_md(path)?;
    std::fs::remove_file(path).map_err(|error| format!("memory: {}: {error}", path.display()))?;
    Ok(format!("deleted {}", path.display()))
}

/// rename: a move within one tier. Cross-tier moves are refused (a
/// fact belongs to the tier it was written for) and both ends refuse
/// MEMORY.md.
fn rename(
    store: &Arc<gertie_core::MemoryStore>,
    tier: Tier,
    path: &Path,
    args: &MemoryArgs,
) -> ToolResult {
    refuse_memory_md(path)?;
    let new_path = args
        .new_path
        .as_deref()
        .ok_or_else(|| "memory: rename needs new_path".to_string())?;
    let (new_tier, new_rel) = match explicit_tier(new_path) {
        Some((spelled, _rest)) if spelled != tier => {
            return Err(format!(
                "memory: rename stays within one tier, {new_path} addresses {}",
                tier_name(spelled)
            ));
        }
        Some((spelled, rest)) => (spelled, rest),
        None => (tier, new_path),
    };
    let target = resolve_in(&store.root(new_tier), new_rel, new_path)?;
    refuse_memory_md(&target)?;
    if target.exists() {
        return Err(format!("memory: {} already exists", target.display()));
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("memory: {}: {error}", parent.display()))?;
    }
    std::fs::rename(path, &target)
        .map_err(|error| format!("memory: {}: {error}", path.display()))?;
    Ok(format!("renamed to {}", target.display()))
}

/// The tier a model path names, for messages and listings.
fn tier_name(tier: Tier) -> &'static str {
    match tier {
        Tier::Project => "project",
        Tier::Global => "global",
    }
}

/// Runs one mutating command, then leaves the trail: the audit line
/// lands in the tier's MEMORY.md and the sidecar syncs. Centralized
/// so no command can forget either half; a failed audit or sync is
/// the tool error (the file change itself already happened).
fn mutate(
    store: &Arc<gertie_core::MemoryStore>,
    tier: Tier,
    given: &str,
    command: &str,
    run: impl FnOnce() -> ToolResult,
) -> ToolResult {
    let output = run()?;
    audit(store, tier, given, command)?;
    sync(store)?;
    Ok(output)
}

/// The one-line audit trail: every mutating command appends
/// `- <date> <command> <path>` to the tier's MEMORY.md, the path
/// spelled the way the model spelled it. The index is otherwise
/// machine-derived and never hand-authored through this tool; the
/// trail is its only changelog, and being prose to the parser it
/// never reaches the sidecar.
fn audit(
    store: &Arc<gertie_core::MemoryStore>,
    tier: Tier,
    given: &str,
    command: &str,
) -> Result<(), String> {
    let date = store.today().map_err(|error| format!("memory: {error}"))?;
    append_line(
        &store.root(tier).join("MEMORY.md"),
        &format!("- {date} {command} {given}"),
    )
}

/// Mirrors the store's own append: a file not ending in a newline
/// would swallow the next line into its last one.
fn append_line(path: &Path, line: &str) -> Result<(), String> {
    use std::io::Write as _;
    let needs_newline = std::fs::read(path)
        .map(|bytes| !bytes.is_empty() && bytes.last() != Some(&b'\n'))
        .unwrap_or(false);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if needs_newline {
        file.write_all(b"\n")
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    file.write_all(line.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// Every mutating command syncs the sidecar (the index must recall
/// what the files now say); reads never do.
fn sync(store: &Arc<gertie_core::MemoryStore>) -> Result<(), String> {
    store
        .sync()
        .map_err(|error| format!("memory: sync: {error}"))
}

/// Full-text search over long-term memory. Pointers out, the model
/// reads the file for the body; hits carry the entry text because
/// that text is the point of the search.
pub struct SearchMemory {
    store: Arc<gertie_core::MemoryStore>,
}

impl SearchMemory {
    pub fn new(store: Arc<gertie_core::MemoryStore>) -> Self {
        Self { store }
    }
}

impl Tool for SearchMemory {
    fn name(&self) -> &'static str {
        "search_memory"
    }

    fn description(&self) -> String {
        "Search long-term memory (decisions, gotchas, preferences, \
         workflows across sessions). Returns id, file, and date \
         pointers; read the file for the full entry."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "text to look for"},
                "limit": {"type": "integer", "description": "max hits (default 10, max 50)"}
            },
            "required": ["query"]
        })
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        let store = self.store.clone();
        let arguments = arguments.to_string();
        blocking(move || {
            #[derive(serde::Deserialize)]
            struct Args {
                query: String,
                limit: Option<usize>,
            }
            let args: Args = crate::parse(&arguments, "search_memory")?;
            let hits = store
                .search(&args.query, args.limit.unwrap_or(10).clamp(1, 50))
                .map_err(|error| format!("search_memory: {error}"))?;
            if hits.is_empty() {
                return Ok("no memory matches".into());
            }
            let mut out = String::new();
            for hit in hits {
                out.push_str(&format!(
                    "[{}] {}, added {}\n   {}\n\n",
                    hit.id,
                    hit.file,
                    hit.added_at.as_deref().unwrap_or("-"),
                    hit.text
                ));
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tmp: &tempfile::TempDir) -> Arc<gertie_core::MemoryStore> {
        Arc::new(gertie_core::MemoryStore::open(
            tmp.path().join("project"),
            tmp.path().join("global"),
            Arc::new(gertie_core::SessionStore::open(std::path::Path::new(":memory:")).unwrap()),
        ))
    }

    /// Tests run outside a runtime; `blocking` futures settle on first
    /// poll. Poll a second time anyway, so a tool that lies about
    /// readiness fails here instead of in production.
    fn settle<'a>(
        future: &mut Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>,
    ) -> ToolResult {
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        match future.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(result) => result,
            std::task::Poll::Pending => {
                std::thread::sleep(std::time::Duration::from_millis(10));
                match future.as_mut().poll(&mut cx) {
                    std::task::Poll::Ready(result) => result,
                    std::task::Poll::Pending => panic!("tool future never settled"),
                }
            }
        }
    }

    fn run_tool(tool: &Memory, arguments: serde_json::Value) -> ToolResult {
        let text = arguments.to_string();
        let mut future = tool.run(&text);
        settle(&mut future)
    }

    fn run_search(tool: &SearchMemory, arguments: serde_json::Value) -> ToolResult {
        let text = arguments.to_string();
        let mut future = tool.run(&text);
        settle(&mut future)
    }

    #[test]
    fn a_fresh_tier_view_lists_nothing_and_creates_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = Memory::new(store(&tmp));
        let out = run_tool(&tool, json!({"command": "view", "path": "project"})).unwrap();
        assert!(out.contains("no memory files yet"));
        assert!(tmp.path().join("project").is_dir());
    }

    #[test]
    fn create_then_str_replace_then_view_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = Memory::new(store(&tmp));

        // Topic files carry entry-grammar lines; prose and headings are
        // allowed (they survive every sync untouched) but never reach
        // the searchable sidecar. This one is a real entry.
        let out = run_tool(
            &tool,
            json!({
                "command": "create",
                "path": "project/gotchas.md",
                "content": "# Gotchas\n\n- [p1] helpful=0 harmful=0 work slowly\n"
            }),
        )
        .unwrap();
        assert!(out.contains("created"));

        let view = run_tool(
            &tool,
            json!({"command": "view", "path": "project/gotchas.md"}),
        )
        .unwrap();
        assert!(view.contains("# Gotchas"));
        assert!(view.contains("work slowly"));

        let out = run_tool(
            &tool,
            json!({
                "command": "str_replace",
                "path": "project/gotchas.md",
                "old_str": "work slowly",
                "new_str": "measure twice"
            }),
        )
        .unwrap();
        assert!(out.contains("edited"));
        let view = run_tool(
            &tool,
            json!({"command": "view", "path": "project/gotchas.md"}),
        )
        .unwrap();
        assert!(view.contains("measure twice"));
        assert!(!view.contains("work slowly"));

        // The sidecar is synced after mutations: the new words are
        // searchable right away.
        let hits = tool.store.search("measure twice", 10).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn str_replace_with_zero_or_multiple_matches_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = Memory::new(store(&tmp));
        run_tool(
            &tool,
            json!({
                "command": "create",
                "path": "project/notes.md",
                "content": "same word, same word again\n"
            }),
        )
        .unwrap();
        let zero = run_tool(
            &tool,
            json!({
                "command": "str_replace",
                "path": "project/notes.md",
                "old_str": "absent",
                "new_str": "x"
            }),
        )
        .unwrap_err();
        assert!(zero.contains("not found"));
        let twice = run_tool(
            &tool,
            json!({
                "command": "str_replace",
                "path": "project/notes.md",
                "old_str": "same word",
                "new_str": "x"
            }),
        )
        .unwrap_err();
        assert!(twice.contains("matches 2 times"));
    }

    #[test]
    fn path_traversal_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = Memory::new(store(&tmp));
        for hostile in [
            json!({"command": "view", "path": "../escape.md"}),
            json!({"command": "view", "path": "/etc/passwd"}),
            json!({"command": "create", "path": "project/../out.md", "content": "x"}),
        ] {
            let error = run_tool(&tool, hostile).unwrap_err();
            assert!(
                error.contains("escape") || error.contains("root") || error.contains("memory:"),
                "{error}"
            );
        }
        assert!(!tmp.path().join("escape.md").exists());
        assert!(!tmp.path().join("out.md").exists());
    }

    #[test]
    fn memory_md_is_never_created_edited_or_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = Memory::new(store(&tmp));
        assert!(
            run_tool(
                &tool,
                json!({"command": "create", "path": "project/MEMORY.md", "content": "x"})
            )
            .is_err()
        );
        assert!(
            run_tool(
                &tool,
                json!({"command": "delete", "path": "project/MEMORY.md"})
            )
            .is_err()
        );
        assert!(
            run_tool(
                &tool,
                json!({"command": "rename", "path": "project/MEMORY.md", "new_path": "notes.md"})
            )
            .is_err()
        );
        assert!(!tmp.path().join("project/MEMORY.md").exists());
    }

    #[test]
    fn every_mutation_leaves_one_audit_line() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = Memory::new(store(&tmp));
        run_tool(
            &tool,
            json!({
                "command": "create",
                "path": "project/decisions.md",
                "content": "one\n"
            }),
        )
        .unwrap();
        run_tool(
            &tool,
            json!({
                "command": "str_replace",
                "path": "project/decisions.md",
                "old_str": "one",
                "new_str": "two"
            }),
        )
        .unwrap();
        let audit = std::fs::read_to_string(tmp.path().join("project/MEMORY.md")).unwrap();
        let lines: Vec<&str> = audit.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 2, "one line per mutation: {lines:?}");
        assert!(lines[0].contains("create project/decisions.md"));
        assert!(lines[1].contains("str_replace project/decisions.md"));
    }

    #[test]
    fn insert_delete_rename_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let tool = Memory::new(store(&tmp));
        run_tool(
            &tool,
            json!({
                "command": "create",
                "path": "global/notes.md",
                "content": "first\nsecond\n"
            }),
        )
        .unwrap();
        run_tool(
            &tool,
            json!({
                "command": "insert",
                "path": "global/notes.md",
                "insert_line": 1,
                "new_str": "between"
            }),
        )
        .unwrap();
        let view = run_tool(&tool, json!({"command": "view", "path": "global/notes.md"})).unwrap();
        assert!(view.contains("first"));
        assert!(view.contains("between"));
        assert!(view.contains("second"));

        run_tool(
            &tool,
            json!({"command": "rename", "path": "global/notes.md", "new_path": "memo.md"}),
        )
        .unwrap();
        assert!(!tmp.path().join("global/notes.md").exists());
        assert!(tmp.path().join("global/memo.md").exists());

        run_tool(
            &tool,
            json!({"command": "delete", "path": "global/memo.md"}),
        )
        .unwrap();
        assert!(!tmp.path().join("global/memo.md").exists());
    }

    #[test]
    fn search_memory_returns_hits_with_their_text() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = store(&tmp);
        memory
            .apply(
                &[gertie_core::Op::Add {
                    file: "gotchas.md".into(),
                    text: "the flake is the teardown".into(),
                    source: Some("12".into()),
                }],
                4096,
            )
            .unwrap();
        let tool = SearchMemory::new(memory);
        let out = run_search(&tool, json!({"query": "teardown"})).unwrap();
        assert!(out.contains("[p1]"));
        assert!(out.contains("gotchas.md"));
        assert!(out.contains("the flake is the teardown"));
        let none = run_search(&tool, json!({"query": "xyzzynope"})).unwrap();
        assert!(none.contains("no memory matches"));
    }
}
