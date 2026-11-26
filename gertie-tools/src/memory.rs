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
    /// Absent reads as view: the view-only tool's schema has no command.
    #[serde(default)]
    command: Option<String>,
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
    /// The file commands (create, str_replace, insert, delete, rename).
    /// Off, the tool is view only and remember is the one write path
    /// (#42): models given two ways to write a fact wrote it twice, and
    /// the write commands are most of this tool's tokens.
    writes: bool,
}

impl Memory {
    pub fn new(store: Arc<gertie_core::MemoryStore>) -> Self {
        Self {
            store,
            writes: true,
        }
    }

    /// View only: browse and read; remember writes.
    pub fn view_only(store: Arc<gertie_core::MemoryStore>) -> Self {
        Self {
            store,
            writes: false,
        }
    }
}

impl Tool for Memory {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn terse_description(&self) -> String {
        if self.writes {
            "Read and write long-term memory files under `project/` or `global/`; to save \
             a fact, prefer remember."
                .into()
        } else {
            "Read long-term memory: `project`, `global`, or `project/<file>.md`; remember \
             writes."
                .into()
        }
    }

    fn description(&self) -> String {
        if !self.writes {
            return "Read long-term memory: `project` or `global` lists a tier's topic \
                    files, `project/<file>.md` reads one (view_range `start:end` for part). \
                    To save or correct a fact, use remember."
                .into();
        }
        "Read and write long-term memory. Two tiers, chosen by the \
         path's leading segment: `project/` (the repo's \
         `.gertie/memory/`, versioned with the clone) and `global/` \
         (the user's data dir, follows them across projects). Topic \
         files hold one-line markdown entries; MEMORY.md is a derived \
         index, never create, edit, or delete it. Memory is advisory: \
         recall, not policy. The index in your context was snapshotted \
         at session start, edits surface next session. Use view to \
         browse; to write a fact, prefer the remember tool (it handles \
         ids, evidence and supersedes for you). If you edit a file here \
         directly, a changed fact is superseded, never rewritten or \
         deleted."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        if !self.writes {
            return json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "`project`, `global`, or `project/<file>.md`"},
                    "view_range": {"type": "string", "description": "`start:end` 1-based lines, the end optional"}
                },
                "required": ["path"]
            });
        }
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
        let writes = self.writes;
        blocking(move || {
            let args: MemoryArgs = crate::parse(&arguments, "memory")?;
            let command = match args.command.as_deref() {
                None | Some("view") => "view",
                Some(_) if !writes => {
                    return Err("memory: this tool only reads; save or correct a fact with \
                                remember (action add, supersede, update, archive or link)"
                        .into());
                }
                Some(command) => command,
            };
            let (tier, rel) = split_tier(&args.path);
            let root = store.root(tier);
            // The tier root is created on demand: a fresh boot has no
            // memory yet, and resolve needs the root on disk.
            std::fs::create_dir_all(&root)
                .map_err(|error| format!("memory: {}: {error}", root.display()))?;
            let path = resolve_in(&root, rel, &args.path)?;
            match command {
                "view" => view(tier, &path, args.view_range.as_deref()),
                "create" => mutate(&store, tier, &args.path, "create", || {
                    create(&store, tier, &path, &args)
                }),
                "str_replace" => mutate(&store, tier, &args.path, "str_replace", || {
                    str_replace(&store, tier, &path, &args)
                }),
                "insert" => mutate(&store, tier, &args.path, "insert", || {
                    insert(&store, tier, &path, &args)
                }),
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

/// The structured write path (#35): the model fills a schema, the
/// harness writes the grammar. Every action lands through
/// `MemoryStore::apply`, the same ops extraction uses, so there is one
/// write path with one set of rules (ids, evidence hashing, supersede
/// flags). Inputs are checked before apply so a refusal can say how to
/// do it right; apply itself only reports "skipped".
pub struct Remember {
    store: Arc<gertie_core::MemoryStore>,
}

impl Remember {
    pub fn new(store: Arc<gertie_core::MemoryStore>) -> Self {
        Self { store }
    }
}

#[derive(serde::Deserialize)]
struct RememberArgs {
    action: String,
    #[serde(default)]
    tier: Option<String>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    evidence: Vec<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    by: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    relation: Option<String>,
}

impl Tool for Remember {
    fn name(&self) -> &'static str {
        "remember"
    }

    fn terse_description(&self) -> String {
        "Save long-term memory, one fact per call: add a new fact, supersede (id and new text) a changed one instead of rewriting it, archive, update, or link.".into()
    }

    fn description(&self) -> String {
        "Write long-term memory: facts worth knowing in a future session (decisions, \
         gotchas, conventions, the user's preferences). One fact per call, one line, \
         specific enough to act on. Actions: add (a new fact, with the files it rests on \
         as evidence); supersede (a fact changed: give the old id and either the new \
         `text` or the replacing id as `by`; never rewrite a fact into a different one); \
         update (fix wording only, same fact); archive (no longer true, nothing replaces \
         it); link (relate two entries). Search memory first so you do not add a \
         duplicate. Memory is recall, not policy: rules belong in AGENTS.md."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string",
                           "enum": ["add", "supersede", "update", "archive", "link"]},
                "text": {"type": "string",
                         "description": "add, update, supersede: the fact, one line"},
                "file": {"type": "string",
                         "description": "add: the topic file, e.g. gotchas.md, decisions.md, conventions.md, preferences.md"},
                "evidence": {"type": "array", "items": {"type": "string"},
                             "description": "add, supersede: workspace paths the fact rests on (files this session read or wrote); gertie records their hashes and flags the entry when they change"},
                "id": {"type": "string",
                       "description": "supersede, update, archive: the entry, e.g. p12; link: the entry the link starts from"},
                "by": {"type": "string",
                       "description": "supersede: an existing entry that replaces id (or give text instead)"},
                "to": {"type": "string", "description": "link: the other entry"},
                "relation": {"type": "string",
                             "enum": gertie_core::memory::RELATIONS,
                             "description": "link: how id relates to `to`"},
                "tier": {"type": "string", "enum": ["project", "global"],
                         "description": "add: project (this repo, the default) or global (about the user, all projects)"}
            },
            "required": ["action"]
        })
    }

    fn run<'a>(
        &'a self,
        arguments: &'a str,
    ) -> Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>> {
        let store = self.store.clone();
        let arguments = arguments.to_string();
        blocking(move || {
            let args: RememberArgs = crate::parse(&arguments, "remember")?;
            let tier = match args.tier.as_deref() {
                Some("global") => Tier::Global,
                None | Some("project") => Tier::Project,
                Some(other) => {
                    return Err(format!(
                        "remember: tier is project or global, got {other:?}"
                    ));
                }
            };
            std::fs::create_dir_all(store.root(tier))
                .map_err(|error| format!("remember: {error}"))?;
            let output = remember_action(&store, tier, &args)?;
            audit(
                &store,
                tier,
                &format!("{} {}", args.action, describe_target(&args)),
                "remember",
            )?;
            sync(&store)?;
            Ok(output)
        })
    }
}

fn describe_target(args: &RememberArgs) -> String {
    args.id
        .clone()
        .or_else(|| args.file.clone())
        .unwrap_or_default()
}

/// One remember call, checked, applied, and described.
fn remember_action(
    store: &Arc<gertie_core::MemoryStore>,
    tier: Tier,
    args: &RememberArgs,
) -> ToolResult {
    use gertie_core::Op;
    let entries = store.scan();
    let exists = |id: &str| entries.iter().any(|entry| entry.id == id);
    let need_id = |field: &str, id: Option<&String>| -> Result<String, String> {
        let id = id
            .map(|id| id.trim().trim_matches(|c| c == '[' || c == ']').to_string())
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                format!(
                    "remember: {} needs `{field}`, an entry id like p12",
                    args.action
                )
            })?;
        let shaped = id.len() > 1
            && matches!(id.as_bytes()[0], b'p' | b'g')
            && id[1..].bytes().all(|byte| byte.is_ascii_digit());
        if !shaped {
            // The common slip: the fact itself in the id field.
            Err(format!(
                "remember: `{field}` takes an entry id like p12, not text. The fact goes in \
                 `text`; to replace an entry, pass its id in `id` and the new fact in `text` \
                 (action supersede)"
            ))
        } else if exists(&id) {
            Ok(id)
        } else {
            Err(format!(
                "remember: no entry {id}. search_memory finds entries and their ids; the \
                 memory index in your context lists the rest"
            ))
        }
    };
    let one_line = |field: &str| -> Result<String, String> {
        let text = args
            .text
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| {
                format!(
                    "remember: {} needs `{field}`, the fact in one line",
                    args.action
                )
            })?;
        if text.lines().count() > 1 {
            return Err(
                "remember: one fact per call, one line; call remember again for the next fact"
                    .into(),
            );
        }
        Ok(text.to_string())
    };
    let topic = |file: &str| -> String {
        let file = file.trim();
        if file.ends_with(".md") {
            file.to_string()
        } else {
            format!("{file}.md")
        }
    };
    let evidence_words = |paths: &[String]| -> String {
        if paths.is_empty() {
            return String::new();
        }
        let kept: Vec<String> = store
            .evidence_for(paths)
            .into_iter()
            .map(|cited| cited.path)
            .collect();
        let dropped: Vec<&str> = paths
            .iter()
            .map(String::as_str)
            .filter(|path| {
                !kept
                    .iter()
                    .any(|kept| path.trim_start_matches("./") == kept)
            })
            .collect();
        let mut words = String::new();
        if !kept.is_empty() {
            words.push_str(&format!("; evidence: {} (hashed)", kept.join(", ")));
        }
        if !dropped.is_empty() {
            words.push_str(&format!(
                "; not evidence: {} (not a file in the workspace)",
                dropped.join(", ")
            ));
        }
        words
    };
    let new_id = |before: &[gertie_core::MemoryEntry]| -> Option<String> {
        store
            .scan()
            .into_iter()
            .find(|entry| !before.iter().any(|old| old.id == entry.id))
            .map(|entry| entry.id)
    };
    let apply = |ops: Vec<Op>| -> Result<gertie_core::memory::ApplyReport, String> {
        let report = store
            .apply(&ops, 4096)
            .map_err(|error| format!("remember: {error}"))?;
        if !report.skipped.is_empty() {
            return Err(format!(
                "remember: not applied: {}",
                report.skipped.join("; ")
            ));
        }
        Ok(report)
    };
    let flagged_words = |report: &gertie_core::memory::ApplyReport| -> String {
        let flagged: Vec<String> = report
            .flagged
            .iter()
            .flat_map(|(_, ids)| ids.iter().cloned())
            .collect();
        if flagged.is_empty() {
            String::new()
        } else {
            format!(
                "; flagged for recheck (they rested on it): {}",
                flagged.join(", ")
            )
        }
    };

    match args.action.as_str() {
        "add" => {
            let text = one_line("text")?;
            let file = topic(args.file.as_deref().ok_or_else(|| {
                "remember: add needs `file`, a topic file like gotchas.md".to_string()
            })?);
            if tier == Tier::Global {
                gertie_core::memory::add_global(store, &file, &text, None)
                    .map_err(|error| format!("remember: {error}"))?;
                let id = new_id(&entries).unwrap_or_default();
                return Ok(format!("added {id} to global/{file}"));
            }
            apply(vec![Op::Add {
                file: file.clone(),
                text,
                source: None,
                evidence: args.evidence.clone(),
            }])
            .map_err(|error| format!("{error}. A topic file is a plain name like gotchas.md"))?;
            let id = new_id(&entries).unwrap_or_default();
            Ok(format!(
                "added {id} to {file}{}",
                evidence_words(&args.evidence)
            ))
        }
        "supersede" => {
            let id = need_id("id", args.id.as_ref())?;
            if let Some(by) = args.by.as_ref() {
                let by = need_id("by", Some(by))?;
                let report = apply(vec![Op::Supersede {
                    id: id.clone(),
                    by: Some(by.clone()),
                }])?;
                return Ok(format!("superseded {id} by {by}{}", flagged_words(&report)));
            }
            let text = one_line("text")?;
            let file = args.file.as_deref().map(topic).unwrap_or_else(|| {
                entries
                    .iter()
                    .find(|entry| entry.id == id)
                    .map(|entry| entry.file.clone())
                    .unwrap_or_else(|| "decisions.md".into())
            });
            let report = apply(vec![
                Op::Add {
                    file: file.clone(),
                    text,
                    source: None,
                    evidence: args.evidence.clone(),
                },
                Op::Supersede {
                    id: id.clone(),
                    by: None,
                },
            ])?;
            let added = new_id(&entries).unwrap_or_default();
            Ok(format!(
                "added {added} to {file}{} and superseded {id} by it{}",
                evidence_words(&args.evidence),
                flagged_words(&report)
            ))
        }
        "update" => {
            let id = need_id("id", args.id.as_ref())?;
            let text = one_line("text")?;
            apply(vec![Op::Update {
                id: id.clone(),
                text,
            }])?;
            Ok(format!(
                "updated {id}'s wording (votes and evidence kept); if the fact itself \
                 changed, supersede instead"
            ))
        }
        "archive" => {
            let id = need_id("id", args.id.as_ref())?;
            apply(vec![Op::Archive { id: id.clone() }])?;
            Ok(format!(
                "archived {id}: it stays in the file as history, out of the index"
            ))
        }
        "link" => {
            let from = need_id("id", args.id.as_ref())?;
            let to = need_id("to", args.to.as_ref())?;
            let relation = args
                .relation
                .as_deref()
                .map(|relation| relation.trim().to_ascii_lowercase())
                .unwrap_or_default();
            if !gertie_core::memory::RELATIONS.contains(&relation.as_str()) || from == to {
                return Err(format!(
                    "remember: link needs two different entries and a relation, one of {}",
                    gertie_core::memory::RELATIONS.join(", ")
                ));
            }
            apply(vec![Op::Link {
                from: from.clone(),
                to: to.clone(),
                relation: relation.clone(),
            }])?;
            Ok(format!("linked {from} {relation} {to}"))
        }
        other => Err(format!(
            "remember: unknown action {other:?}; one of add, supersede, update, archive, link"
        )),
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

/// What a file write did to the entries, for its result: the model
/// reads exactly which ids it created or changed (#37).
fn landed(tier: Tier, before: &str, after: &str) -> String {
    let before = gertie_core::memory::entries_in(before, tier);
    let after = gertie_core::memory::entries_in(after, tier);
    let new: Vec<&str> = after
        .iter()
        .filter(|(id, _)| !before.iter().any(|(old, _)| old == id))
        .map(|(id, _)| id.as_str())
        .collect();
    let edited: Vec<&str> = after
        .iter()
        .filter(|(id, line)| before.iter().any(|(old, was)| old == id && was != line))
        .map(|(id, _)| id.as_str())
        .collect();
    let mut words = Vec::new();
    if !new.is_empty() {
        words.push(format!("new entries {}", new.join(", ")));
    }
    if !edited.is_empty() {
        words.push(format!("edited {}", edited.join(", ")));
    }
    if words.is_empty() {
        String::new()
    } else {
        format!(": {}", words.join("; "))
    }
}

/// The file name the store records entries under.
fn file_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string()
}

/// create: the file must not exist yet, parents are created, the whole
/// content passes the normalizer (one entrance, one cap).
fn create(
    store: &gertie_core::MemoryStore,
    tier: Tier,
    path: &Path,
    args: &MemoryArgs,
) -> ToolResult {
    refuse_memory_md(path)?;
    let content = args
        .content
        .as_deref()
        .ok_or_else(|| "memory: create needs content".to_string())?;
    if path.exists() {
        return Err(format!("memory: {} already exists", path.display()));
    }
    let content = store.normalize_edit(tier, &file_label(path), "", content)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("memory: {}: {error}", parent.display()))?;
    }
    std::fs::write(path, &content)
        .map_err(|error| format!("memory: {}: {error}", path.display()))?;
    Ok(format!(
        "created {}{}",
        path.display(),
        landed(tier, "", &content)
    ))
}

/// str_replace: the anchor must occur exactly once, so the edit can
/// never land somewhere the model did not look at.
fn str_replace(
    store: &gertie_core::MemoryStore,
    tier: Tier,
    path: &Path,
    args: &MemoryArgs,
) -> ToolResult {
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
    // The replacement wraps too: a str_replace that adds content is
    // a memory action and must land as grammar (the last unwrapped
    // command; the per-line mint form would collide on multi-line
    // replacements, so this rides the batch).
    let raw = text.replacen(old_str, new_str, 1);
    let edited = store.normalize_edit(tier, &file_label(path), &text, &raw)?;
    std::fs::write(path, &edited)
        .map_err(|error| format!("memory: {}: {error}", path.display()))?;
    Ok(format!(
        "edited {}{}",
        path.display(),
        landed(tier, &text, &edited)
    ))
}

/// insert: the new text lands after the given 1-based line, so a
/// merge is always a splice, never a whole-file rewrite.
fn insert(
    store: &gertie_core::MemoryStore,
    tier: Tier,
    path: &Path,
    args: &MemoryArgs,
) -> ToolResult {
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
    // Every write goes through the store's normalizer, so the tool
    // path and the ops path produce the same shaped store.
    let mut edited: Vec<&str> = Vec::with_capacity(lines.len() + 1);
    edited.extend(&lines[..at]);
    let added = if new_str.is_empty() {
        ""
    } else {
        new_str.trim_end_matches('\n')
    };
    edited.push(added);
    edited.extend(&lines[at..]);
    let mut raw = edited.join("\n");
    if text.ends_with('\n') {
        raw.push('\n');
    }
    let out = store.normalize_edit(tier, &file_label(path), &text, &raw)?;
    std::fs::write(path, &out).map_err(|error| format!("memory: {}: {error}", path.display()))?;
    Ok(format!(
        "inserted after line {at}{}",
        landed(tier, &text, &out)
    ))
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

    fn terse_description(&self) -> String {
        "Search long-term memory; search before deciding something the project may have settled, and before remembering.".into()
    }

    fn description(&self) -> String {
        "Search long-term memory: decisions, gotchas, conventions and \
         preferences from earlier sessions. Search before deciding \
         something the project may have settled already, and before \
         remembering a fact (so you supersede the old entry instead of \
         adding a duplicate). Entries citing a file also arrive on \
         their own when you read or edit that file. Returns each hit's \
         id, file, date and text; an entry marked superseded names the \
         entry to prefer."
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
                    "[{}] {}, added {}\n   {}\n",
                    hit.id,
                    hit.file,
                    hit.added_at.as_deref().unwrap_or("-"),
                    hit.text
                ));
                if !hit.links.is_empty() {
                    let links = hit
                        .links
                        .iter()
                        .map(|link| format!("{} {}", link.relation, link.to))
                        .collect::<Vec<_>>()
                        .join(", ");
                    out.push_str(&format!("   links: {links}\n"));
                }
                if let Some(successor) = &hit.superseded_by {
                    out.push_str(&format!(
                        "   superseded by [{successor}]: prefer that entry\n"
                    ));
                }
                if !hit.recheck.is_empty() {
                    out.push_str(&format!(
                        "   rested on {}, since corrected (re-check before relying on it)\n",
                        hit.recheck.join(", ")
                    ));
                }
                if !hit.evidence_changed.is_empty() {
                    out.push_str(&format!(
                        "   evidence changed since written: {} (re-check before relying on it)\n",
                        hit.evidence_changed.join(", ")
                    ));
                }
                out.push('\n');
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
        // A loaded machine (the whole workspace testing in parallel)
        // can take longer than one short sleep: keep polling, with a
        // ceiling that still catches a future that never settles.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let std::task::Poll::Ready(result) = future.as_mut().poll(&mut cx) {
                return result;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "tool future never settled"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
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
    fn the_round_trip_sequence_mints_unique_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(gertie_core::MemoryStore::open(
            tmp.path().join("project"),
            tmp.path().join("global"),
            Arc::new(gertie_core::SessionStore::open(std::path::Path::new(":memory:")).unwrap()),
        ));
        let tool = Memory::new(store.clone());
        // The exact sequence: create (two lines -> two entries), then
        // insert one more between them.
        let _ = run_tool(
            &tool,
            json!({"command": "create", "path": "global/notes.md", "content": "first\nsecond\n"}),
        );
        for line in std::fs::read_to_string(tmp.path().join("global/notes.md"))
            .unwrap()
            .lines()
        {
            eprintln!("after create: {line}");
        }
        let _ = run_tool(
            &tool,
            json!({"command": "insert", "path": "global/notes.md", "insert_line": 1, "new_str": "between"}),
        );
        let after_insert = std::fs::read_to_string(tmp.path().join("global/notes.md")).unwrap();
        // The ids in the file after create+insert: three entries, all
        // distinct. The round-trip's UNIQUE failure claimed a repeat.
        let ids: Vec<String> = after_insert
            .lines()
            .filter_map(|line| {
                let rest = line.trim_start().strip_prefix("- [")?;
                rest.split(']').next().map(|id| id.to_string())
            })
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(ids.len(), sorted.len(), "duplicate ids: {ids:?}");
        // The round-trip's remaining steps: rename, then delete. The
        // UNIQUE failure fired at one of these syncs.
        // Back-to-back mints go through the batch (one scan,
        // sequential ids); the per-line form is single-shot by
        // contract and would collide here.
        let minted = store
            .entry_lines(gertie_core::Tier::Global, "direct a\ndirect b")
            .unwrap();
        let a_id = minted[0].split(']').next().unwrap().to_string();
        let b_id = minted[1].split(']').next().unwrap().to_string();
        assert_ne!(a_id, b_id, "batch mints collided: {minted:?}");
        run_tool(
            &tool,
            json!({"command": "rename", "path": "global/notes.md", "new_path": "memo.md"}),
        )
        .unwrap_or_else(|error| panic!("rename failed: {error}"));
        run_tool(
            &tool,
            json!({"command": "delete", "path": "global/memo.md"}),
        )
        .unwrap_or_else(|error| panic!("delete failed: {error}"));
    }

    #[test]
    fn rename_then_delete_never_collide() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(gertie_core::MemoryStore::open(
            tmp.path().join("project"),
            tmp.path().join("global"),
            Arc::new(gertie_core::SessionStore::open(std::path::Path::new(":memory:")).unwrap()),
        ));
        let tool = Memory::new(store.clone());
        run_tool(
            &tool,
            json!({"command": "create", "path": "global/notes.md", "content": "first\nsecond\n"}),
        )
        .unwrap_or_else(|error| panic!("create: {error}"));
        run_tool(&tool, json!({"command": "insert", "path": "global/notes.md", "insert_line": 1, "new_str": "between"}))
            .unwrap_or_else(|error| panic!("insert: {error}"));
        // The two steps after, exactly as the round-trip runs them.
        run_tool(
            &tool,
            json!({"command": "rename", "path": "global/notes.md", "new_path": "memo.md"}),
        )
        .unwrap_or_else(|error| panic!("rename: {error}"));
        run_tool(
            &tool,
            json!({"command": "delete", "path": "global/memo.md"}),
        )
        .unwrap_or_else(|error| panic!("delete: {error}"));
    }

    /// The tool path and the ops path produce the same shaped store:
    /// a model that hand-writes lines through the memory tool gets
    /// grammar-wrapped entries (fresh id, counters, provenance, date),
    /// not prose the store cannot index. Found live when the first
    /// hand-made supersede produced grammar-less tombstones.
    #[test]
    fn tool_writes_carry_the_entry_grammar() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(gertie_core::MemoryStore::open(
            tmp.path().join("p"),
            tmp.path().join("g"),
            Arc::new(gertie_core::SessionStore::open(std::path::Path::new(":memory:")).unwrap()),
        ));
        let tool = Memory::new(store.clone());
        let path = tmp.path().join("p/gotchas.md");
        // The model's raw prose, no grammar: exactly what the live
        // session wrote before this fix.
        let out = run_tool(
            &tool,
            json!({"command": "create", "path": "project/gotchas.md",
                   "content": "# Gotchas\n\nthe flake hides in the teardown order"}),
        )
        .unwrap();
        assert!(out.contains("created"));
        let text = std::fs::read_to_string(&path).unwrap();
        // The prose line became a grammar entry; the heading survives.
        let entry = text
            .lines()
            .find(|line| line.contains("flake hides"))
            .unwrap();
        assert!(entry.starts_with("- [p"), "wrapped in grammar: {entry}");
        assert!(entry.contains("helpful=0"), "counters ride: {entry}");
        assert!(entry.contains("added:"), "the date rides: {entry}");
        // And the store can see it: search finds the wrapped text.
        let hits = store.search("flake hides", 5).unwrap();
        assert_eq!(hits.len(), 1, "indexable, not prose: {hits:?}");
        // Grammar lines the model DID write pass through untouched.
        std::fs::write(&path, "- [p42] helpful=1 harmful=0 hand-written\n").unwrap();
        let out = run_tool(
            &tool,
            json!({"command": "insert", "path": "project/gotchas.md",
                   "new_str": "another raw line", "insert_line": 1}),
        )
        .unwrap();
        assert!(out.contains("inserted"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("- [p42] helpful=1 harmful=0 hand-written"),
            "the model's own grammar survives: {text}"
        );
        assert!(
            text.lines()
                .any(|line| line.starts_with("- [p") && line.contains("another raw line")),
            "the raw line wrapped too: {text}"
        );
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
                    evidence: Vec::new(),
                }],
                4096,
            )
            .unwrap();
        let tool = SearchMemory::new(memory.clone());
        let out = run_search(&tool, json!({"query": "teardown"})).unwrap();
        assert!(out.contains("[p1]"));
        assert!(out.contains("gotchas.md"));
        assert!(out.contains("the flake is the teardown"));
        let none = run_search(&tool, json!({"query": "xyzzynope"})).unwrap();
        assert!(none.contains("no memory matches"));

        // A superseded entry still matches, with its successor named:
        // the model retrieves the stale belief *and the way out*.
        // The successor must exist: supersede records a replacement,
        // not a dangling pointer.
        memory
            .apply(
                &[gertie_core::Op::Add {
                    file: "decisions.md".into(),
                    text: "the teardown hook owns the retry window".into(),
                    source: Some("12".into()),
                    evidence: Vec::new(),
                }],
                4096,
            )
            .unwrap();
        let successor = {
            let facts = memory.scan();
            facts
                .iter()
                .find(|entry| entry.text.contains("retry window"))
                .map(|entry| entry.id.clone())
                .expect("the successor entry exists")
        };
        memory
            .apply(
                &[gertie_core::Op::Supersede {
                    id: "p1".into(),
                    by: Some(successor),
                }],
                4096,
            )
            .unwrap();
        let stale = run_search(&tool, json!({"query": "teardown"})).unwrap();
        assert!(
            stale.contains("superseded by"),
            "the readout must ride the hit: {stale}"
        );
    }

    /// The write path's normalizer (the fix-1 class): what models
    /// actually send through create/insert/str_replace must land as
    /// clean entries, never as wrapped wrappers. Each case below is a
    /// shape the old line-by-line wrap mangled.
    fn grammar_fixture() -> (tempfile::TempDir, Arc<gertie_core::MemoryStore>, Memory) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(gertie_core::MemoryStore::open(
            tmp.path().join("project"),
            tmp.path().join("global"),
            Arc::new(gertie_core::SessionStore::open(std::path::Path::new(":memory:")).unwrap()),
        ));
        let tool = Memory::new(store.clone());
        (tmp, store, tool)
    }

    fn file_lines(tmp: &tempfile::TempDir, name: &str) -> Vec<String> {
        std::fs::read_to_string(tmp.path().join("project").join(name))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn file_writes_name_the_entries_they_landed() {
        let (tmp, _store, tool) = grammar_fixture();
        let out = run_tool(
            &tool,
            json!({"command": "create", "path": "project/gotchas.md",
                   "content": "# Gotchas\n\nfirst fact\nsecond fact\n"}),
        )
        .unwrap();
        assert!(out.contains("new entries p1, p2"), "{out}");
        let out = run_tool(
            &tool,
            json!({"command": "str_replace", "path": "project/gotchas.md",
                   "old_str": "first fact", "new_str": "first fact, refined"}),
        )
        .unwrap();
        assert!(out.contains("edited p1"), "{out}");
        let _ = tmp;
    }

    #[test]
    fn create_keeps_headings_and_strips_the_models_bullets() {
        let (tmp, store, tool) = grammar_fixture();
        run_tool(
            &tool,
            json!({"command": "create", "path": "project/gotchas.md",
                   "content": "# Gotchas\n\n- the pool is sized for postgres\n* warm it at boot\n"}),
        )
        .unwrap();
        let lines = file_lines(&tmp, "gotchas.md");
        assert_eq!(
            lines[0], "# Gotchas",
            "the heading is not an entry: {lines:?}"
        );
        let entries = store.scan();
        let texts: Vec<&str> = entries.iter().map(|entry| entry.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["the pool is sized for postgres", "warm it at boot"],
            "the bullet is markdown, not the fact: {lines:?}"
        );
    }

    #[test]
    fn a_line_already_in_grammar_is_not_wrapped_again() {
        let (tmp, store, tool) = grammar_fixture();
        run_tool(
            &tool,
            json!({"command": "create", "path": "project/gotchas.md",
                   "content": "- [p3] helpful=0 harmful=0 already an entry [source: hand; added: 2026-11-26]\n"}),
        )
        .unwrap();
        let lines = file_lines(&tmp, "gotchas.md");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(
            lines[0].matches("helpful=").count(),
            1,
            "one entry, not an entry inside an entry: {lines:?}"
        );
        assert_eq!(store.scan()[0].text, "already an entry");
    }

    #[test]
    fn a_word_fix_edits_the_entry_in_place() {
        let (tmp, store, tool) = grammar_fixture();
        std::fs::create_dir_all(tmp.path().join("project")).unwrap();
        std::fs::write(
            tmp.path().join("project/gotchas.md"),
            "# Gotchas\n\n- [p4] helpful=2 harmful=0 teh pool is sized for postgres [source: 12; added: 2026-11-01]\n",
        )
        .unwrap();
        run_tool(
            &tool,
            json!({"command": "str_replace", "path": "project/gotchas.md",
                   "old_str": "teh", "new_str": "the"}),
        )
        .unwrap();
        let lines = file_lines(&tmp, "gotchas.md");
        assert_eq!(
            lines[2],
            "- [p4] helpful=2 harmful=0 the pool is sized for postgres [source: 12; added: 2026-11-01]",
            "the word changed, nothing else: {lines:?}"
        );
        assert_eq!(store.scan().len(), 1);
    }

    #[test]
    fn an_edit_keeps_votes_provenance_and_evidence() {
        let (tmp, store, tool) = grammar_fixture();
        std::fs::create_dir_all(tmp.path().join("project")).unwrap();
        std::fs::write(tmp.path().join("pool.rs"), "fn pool() {}\n").unwrap();
        let original = "- [p4] helpful=2 harmful=1 the pool is sized for postgres \
                        [evidence: pool.rs@000000000000] [source: 12; added: 2026-11-01]";
        std::fs::write(
            tmp.path().join("project/gotchas.md"),
            format!("{original}\n"),
        )
        .unwrap();
        // The model rewrites the whole line: new text, but also new
        // counters and a fresh-looking hash (which would launder the
        // stale-evidence flag).
        run_tool(
            &tool,
            json!({"command": "str_replace", "path": "project/gotchas.md",
                   "old_str": original,
                   "new_str": "- [p4] helpful=9 harmful=0 the pool is sized for sqlite [evidence: pool.rs@ffffffffffff]"}),
        )
        .unwrap();
        let entry = store.scan().pop().unwrap();
        assert_eq!(entry.id, "p4");
        assert_eq!(entry.text, "the pool is sized for sqlite");
        assert_eq!(
            (entry.helpful, entry.harmful),
            (2, 1),
            "votes are the human's"
        );
        assert_eq!(entry.source_session.as_deref(), Some("12"));
        assert_eq!(
            entry.evidence[0].hash, "000000000000",
            "the stale flag survives"
        );
    }

    #[test]
    fn plain_text_over_an_entry_line_updates_that_entry() {
        let (tmp, store, tool) = grammar_fixture();
        std::fs::create_dir_all(tmp.path().join("project")).unwrap();
        let original = "- [p4] helpful=2 harmful=0 the pool is sized for postgres [source: 12; added: 2026-11-01]";
        std::fs::write(
            tmp.path().join("project/gotchas.md"),
            format!("{original}\n"),
        )
        .unwrap();
        run_tool(
            &tool,
            json!({"command": "str_replace", "path": "project/gotchas.md",
                   "old_str": original, "new_str": "the pool is sized for sqlite"}),
        )
        .unwrap();
        let entries = store.scan();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].id, "p4", "the same entry, not a new one");
        assert_eq!(entries[0].text, "the pool is sized for sqlite");
        assert_eq!(entries[0].helpful, 2);
    }

    #[test]
    fn removing_an_entry_is_refused_with_the_way_to_retire_it() {
        let (tmp, store, tool) = grammar_fixture();
        std::fs::create_dir_all(tmp.path().join("project")).unwrap();
        let original = "- [p4] helpful=0 harmful=0 old advice [source: 12; added: 2026-11-01]";
        std::fs::write(
            tmp.path().join("project/gotchas.md"),
            format!("{original}\n"),
        )
        .unwrap();
        let error = run_tool(
            &tool,
            json!({"command": "str_replace", "path": "project/gotchas.md",
                   "old_str": original, "new_str": ""}),
        )
        .unwrap_err();
        assert!(
            error.contains("p4") && error.contains("valid until"),
            "{error}"
        );
        assert_eq!(store.scan().len(), 1, "the file is untouched");
    }

    #[test]
    fn a_colliding_id_is_reminted_and_invented_hashes_are_replaced() {
        let (tmp, store, tool) = grammar_fixture();
        std::fs::create_dir_all(tmp.path().join("project")).unwrap();
        std::fs::write(tmp.path().join("pool.rs"), "fn pool() {}\n").unwrap();
        std::fs::write(
            tmp.path().join("project/a.md"),
            "- [p1] helpful=0 harmful=0 first [source: added: 2026-11-01]\n",
        )
        .unwrap();
        run_tool(
            &tool,
            json!({"command": "create", "path": "project/b.md",
                   "content": "- [p1] helpful=5 harmful=0 the pool warms at boot [evidence: pool.rs@abc123def456, gone.rs@abc123def456]\npool size is 8 [evidence: pool.rs]\n"}),
        )
        .unwrap();
        let entries = store.scan();
        let mut ids: Vec<&str> = entries.iter().map(|entry| entry.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 3, "unique ids: {entries:?}");
        let real = gertie_core::blob_hash(b"fn pool() {}\n");
        for text in ["the pool warms at boot", "pool size is 8"] {
            let entry = entries.iter().find(|entry| entry.text == text).unwrap();
            assert_eq!(entry.helpful, 0, "a new entry starts unvoted: {entry:?}");
            assert_eq!(
                entry.evidence.len(),
                1,
                "a missing file is not evidence: {entry:?}"
            );
            assert_eq!(entry.evidence[0].path, "pool.rs");
            assert_eq!(entry.evidence[0].hash, real, "gertie hashes, not the model");
        }
    }

    #[test]
    fn insert_and_legacy_prose_leave_existing_lines_alone() {
        let (tmp, store, tool) = grammar_fixture();
        std::fs::create_dir_all(tmp.path().join("project")).unwrap();
        std::fs::write(
            tmp.path().join("project/notes.md"),
            "# Notes\n\nsome legacy prose\n- [p2] helpful=1 harmful=0 kept [source: added: 2026-11-01]\n",
        )
        .unwrap();
        run_tool(
            &tool,
            json!({"command": "insert", "path": "project/notes.md", "insert_line": 4,
                   "new_str": "- a fresh fact"}),
        )
        .unwrap();
        let lines = file_lines(&tmp, "notes.md");
        assert_eq!(lines[2], "some legacy prose");
        assert_eq!(
            lines[3],
            "- [p2] helpful=1 harmful=0 kept [source: added: 2026-11-01]"
        );
        assert!(
            lines[4].starts_with("- [p3] helpful=0 harmful=0 a fresh fact"),
            "{lines:?}"
        );
        assert_eq!(store.scan().len(), 2);
    }

    fn remember(tool: &Remember, arguments: serde_json::Value) -> ToolResult {
        let text = arguments.to_string();
        let mut future = tool.run(&text);
        settle(&mut future)
    }

    fn seeded(lines: &str) -> (tempfile::TempDir, Arc<gertie_core::MemoryStore>, Remember) {
        let (tmp, store, _) = grammar_fixture();
        std::fs::create_dir_all(tmp.path().join("project")).unwrap();
        if !lines.is_empty() {
            std::fs::write(tmp.path().join("project/gotchas.md"), lines).unwrap();
        }
        store.sync().unwrap();
        let tool = Remember::new(store.clone());
        (tmp, store, tool)
    }

    #[test]
    fn remember_add_hashes_evidence_and_says_what_landed() {
        let (tmp, store, tool) = seeded("");
        std::fs::write(tmp.path().join("pool.rs"), "fn pool() {}\n").unwrap();
        let out = remember(
            &tool,
            json!({"action": "add", "file": "gotchas", "text": "the pool warms at boot",
                   "evidence": ["pool.rs", "gone.rs"]}),
        )
        .unwrap();
        let entry = store.scan().pop().unwrap();
        assert_eq!(entry.file, "gotchas.md", "the .md is the harness's job");
        assert_eq!(entry.evidence.len(), 1);
        assert_eq!(
            entry.evidence[0].hash,
            gertie_core::blob_hash(b"fn pool() {}\n")
        );
        assert!(
            out.contains(&entry.id) && out.contains("gotchas.md"),
            "{out}"
        );
        assert!(
            out.contains("pool.rs") && out.contains("gone.rs"),
            "kept and dropped: {out}"
        );
        let log = std::fs::read_to_string(tmp.path().join("project/MEMORY.md")).unwrap();
        assert!(log.contains("remember add"), "the audit trail: {log}");
    }

    /// #42: the view-only tool reads like the full one, refuses writes
    /// with the way to write, and costs a fraction of the tokens.
    #[test]
    fn the_view_only_tool_reads_and_points_writes_at_remember() {
        let (_tmp, store, _tool) =
            seeded("- [p1] helpful=0 harmful=0 one [source: added: 2026-11-01]\n");
        let full = Memory::new(store.clone());
        let view = Memory::view_only(store);
        let cost = |tool: &Memory| tool.description().len() + tool.parameters().to_string().len();
        assert!(
            cost(&view) * 3 < cost(&full),
            "view only {} vs full {}",
            cost(&view),
            cost(&full)
        );
        assert!(view.parameters()["properties"].get("command").is_none());
        let run = |tool: &Memory, arguments: serde_json::Value| {
            let text = arguments.to_string();
            let mut future = tool.run(&text);
            settle(&mut future)
        };
        let listed = run(&view, json!({"path": "project"})).unwrap();
        assert!(listed.contains("gotchas.md"), "{listed}");
        let read = run(&view, json!({"path": "project/gotchas.md"})).unwrap();
        assert!(read.contains("[p1]"), "{read}");
        let refused = run(
            &view,
            json!({"command": "create", "path": "project/x.md", "content": "- a fact"}),
        )
        .unwrap_err();
        assert!(refused.contains("remember"), "{refused}");
    }

    #[test]
    fn remember_refuses_with_the_fix() {
        let (_tmp, _store, tool) =
            seeded("- [p1] helpful=0 harmful=0 one [source: added: 2026-11-01]\n");
        let multi = remember(
            &tool,
            json!({"action": "add", "file": "gotchas.md", "text": "first fact\nsecond fact"}),
        )
        .unwrap_err();
        assert!(multi.contains("one fact per call"), "{multi}");
        let unknown = remember(&tool, json!({"action": "archive", "id": "p99"})).unwrap_err();
        assert!(
            unknown.contains("p99") && unknown.contains("search_memory"),
            "{unknown}"
        );
        let relation = remember(
            &tool,
            json!({"action": "link", "id": "p1", "to": "p1", "relation": "inspired"}),
        )
        .unwrap_err();
        assert!(
            relation.contains("builds_on"),
            "names the relations: {relation}"
        );
        // What ppq sent on the second live eval, twice: the fact as the id.
        let slip = remember(
            &tool,
            json!({"action": "supersede", "id": "POOL_SIZE was raised 8 to 32 at user request"}),
        )
        .unwrap_err();
        assert!(
            slip.contains("not text") && slip.contains("`text`") && slip.contains("p12"),
            "{slip}"
        );
        let action = remember(&tool, json!({"action": "forget", "id": "p1"})).unwrap_err();
        assert!(action.contains("supersede"), "{action}");
    }

    #[test]
    fn remember_supersede_with_text_adds_the_replacement_and_flags_dependents() {
        let (_tmp, store, tool) = seeded(
            "- [p1] helpful=0 harmful=0 sign releases with the local key [source: added: 2026-11-01]\n\
             - [p2] helpful=0 harmful=0 keep checksums next to the release [links: builds_on p1] [source: added: 2026-11-01]\n",
        );
        let out = remember(
            &tool,
            json!({"action": "supersede", "id": "p1", "text": "signing moved to CI"}),
        )
        .unwrap();
        let entries = store.scan();
        let old = entries.iter().find(|entry| entry.id == "p1").unwrap();
        let new = entries
            .iter()
            .find(|entry| entry.text == "signing moved to CI")
            .unwrap();
        assert_eq!(old.superseded_by.as_deref(), Some(new.id.as_str()));
        assert_eq!(
            new.file, "gotchas.md",
            "the replacement lands beside the old"
        );
        let dependent = entries.iter().find(|entry| entry.id == "p2").unwrap();
        assert_eq!(dependent.recheck, vec!["p1".to_string()]);
        assert!(
            out.contains(&new.id) && out.contains("p2"),
            "names the flag: {out}"
        );
    }

    #[test]
    fn remember_update_archive_and_link() {
        let (_tmp, store, tool) = seeded(
            "- [p1] helpful=3 harmful=0 the pool is tuned [source: added: 2026-11-01]\n\
             - [p2] helpful=0 harmful=0 warm it at boot [source: added: 2026-11-01]\n\
             - [p3] helpful=0 harmful=0 stale advice [source: added: 2026-11-01]\n",
        );
        remember(
            &tool,
            json!({"action": "update", "id": "p1", "text": "the pool is tuned for sqlite"}),
        )
        .unwrap();
        remember(
            &tool,
            json!({"action": "link", "id": "p2", "to": "p1", "relation": "builds_on"}),
        )
        .unwrap();
        remember(&tool, json!({"action": "archive", "id": "p3"})).unwrap();
        let entries = store.scan();
        let p1 = entries.iter().find(|entry| entry.id == "p1").unwrap();
        assert_eq!(
            (p1.text.as_str(), p1.helpful),
            ("the pool is tuned for sqlite", 3)
        );
        let p2 = entries.iter().find(|entry| entry.id == "p2").unwrap();
        assert_eq!(p2.links[0].to, "p1");
        let p3 = entries.iter().find(|entry| entry.id == "p3").unwrap();
        assert!(p3.valid_until.is_some(), "archived, never deleted");
    }

    #[test]
    fn remember_global_tier_adds_a_global_entry() {
        let (_tmp, store, tool) = seeded("");
        remember(
            &tool,
            json!({"action": "add", "tier": "global", "file": "preferences.md",
                   "text": "prefers commits without em dashes"}),
        )
        .unwrap();
        let entry = store.scan().pop().unwrap();
        assert!(entry.id.starts_with('g'), "{entry:?}");
    }
}
