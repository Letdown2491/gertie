//! The status column (issue #33): the open session's durable state
//! beside the transcript, and the riding line's dependency note on the
//! activity row (momentary state, latest turn only).
//!
//! Sections, in column order, each rendered only when it has
//! something to say: WORKING ON (the model's `status` goal, else the
//! latest prompt) with the run clock, NEXT UP (the model's `status`
//! next step), WAITING ON YOU (asks from the other open chats of this
//! chat's project), REMEMBERED (the session's memory writes, newest
//! last), TOUCHED (read/edited per file, from the session's tool
//! calls, edited wins), DEPENDS ON (memory the work consulted, then
//! entries whose evidence shares a file with the work), CORRECTIONS
//! (the walk behind a flagged dependency entry).
//!
//! The facts fold from the session's rows (a background load on open)
//! and then from live events; nothing here asks the model anything.

use super::*;
use gertie_core::{CorrectionStep, MemoryEntry};
use gpui_kit::relative;

/// Grounding rows per tier: the consulted entries, then the
/// file-sharing ones (the weaker signal, so fewer).
const ACTING_SHOWN: usize = 6;
const SHARING_SHOWN: usize = 4;

/// What a session did to a file. Edited wins: a file read and then
/// edited is edited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Touch {
    Read,
    Edited,
}

/// The session's durable state, folded from its rows and live events.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusFacts {
    /// The session's memory actions, newest-first (capped at five):
    /// created/superseded/edited entries, the #20 walk's live view.
    pub memory_actions: Vec<String>,
    /// The model's stated goal (the `status` tool), the latest one.
    pub goal: Option<String>,
    /// The model's stated next step (the `status` tool).
    pub next: Option<String>,
    /// The latest user prompt's first line: the goal's fallback.
    pub last_prompt: Option<String>,
    /// Files in first-touch order, workspace-relative.
    pub touched: Vec<(String, Touch)>,
    /// Memory ids the session's `search_memory` calls returned, in
    /// first-seen order: what the work consulted ("acting on").
    pub consulted: Vec<String>,
}

impl StatusFacts {
    /// Every row of a session, oldest first.
    pub fn from_rows(rows: &[gertie_core::Message], root: &std::path::Path) -> Self {
        let mut facts = Self::default();
        for row in rows {
            match row.role {
                Role::User => facts.fold_user(&row.content),
                Role::Tool => facts.fold_tool(
                    row.tool_name.as_deref().unwrap_or_default(),
                    row.tool_arguments.as_deref().unwrap_or_default(),
                    &row.content,
                    root,
                ),
                _ => {}
            }
        }
        facts
    }

    pub fn fold_user(&mut self, text: &str) {
        let line = display_text(text)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| clip(line, 160));
        if line.is_some() {
            self.last_prompt = line;
        }
    }

    /// One finished tool call. A failed or denied call touched
    /// nothing and said nothing.
    pub fn fold_tool(&mut self, name: &str, arguments: &str, result: &str, root: &std::path::Path) {
        if failed(result) {
            return;
        }
        // Recall on touch (#36): what the harness put in front of the
        // model is consulted, whichever tool carried it and whether or
        // not the model ever searched.
        for id in gertie_core::memory_recall::recalled_ids(result) {
            self.consult(id);
        }
        let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
        let field = |key: &str| {
            args.get(key)
                .and_then(|value| value.as_str())
                .map(|text| gertie_core::strip_invisible(text).trim().to_string())
                .filter(|text| !text.is_empty())
        };
        match name {
            "read_file" | "edit_file" | "write_file" => {
                let Some(path) = field("path").and_then(|raw| normalize_path(root, &raw)) else {
                    return;
                };
                // The existence gate, resolved against the workspace
                // root (never the process CWD: normalize_path returns
                // a root-relative display string, and a relative
                // is_file() asks the wrong directory — the probe
                // caught a real file reporting false). A read or edit
                // needs the file real; write_file creates its target.
                let real = resolve_tool_path(root, std::path::Path::new(&path)).is_file();
                if !real && name != "write_file" {
                    return;
                }
                let touch = if name == "read_file" {
                    Touch::Read
                } else {
                    Touch::Edited
                };
                self.touch(path.clone(), touch);
                // Reading a memory topic file IS a consultation: the
                // model's natural path (it reads the files; it does
                // not call search_memory). The read result carries the
                // entry lines, so the ids come free. This is what
                // makes DEPENDS ON and the riding card fire on the
                // natural path — the live session's gap.
                if path.contains(".gertie/memory") {
                    for id in memory_hit_ids(result) {
                        self.consult(id);
                    }
                }
            }
            // The memory tool's actions are state changes worth
            // surfacing: create/str_replace/insert/delete/rename and
            // supersede ops. Each lands as one action line,
            // newest-first.
            "memory" => {
                let command = field("command").unwrap_or_default();
                let target = field("path").unwrap_or_default();
                // Consultation-shaped commands saw the entries: view
                // returns the file's entry lines (ids free, same
                // mechanism as the search arm), and str_replace /
                // insert had to read the content to anchor on it. The
                // model's natural path is the memory tool, not
                // search_memory — the live test's gap.
                if matches!(command.as_str(), "view" | "str_replace" | "insert") {
                    for id in memory_hit_ids(result) {
                        self.consult(id);
                    }
                }
                if !command.is_empty() {
                    // The verb is gertie's word for what happened;
                    // the file is the entry's basename. "created
                    // gotchas.md", not "create project/gotchas.md".
                    let verb = match command.as_str() {
                        "create" => "created",
                        "str_replace" | "insert" => "edited",
                        "delete" => "removed",
                        "rename" => "renamed",
                        other => other,
                    };
                    let file = target.rsplit('/').next().unwrap_or(&target);
                    // Newest first, capped: the old push-then-pop
                    // dropped every action after the fifth.
                    if verb != "view" {
                        self.memory_actions
                            .insert(0, clip(&format!("{verb} {file}"), 80));
                    }
                }
            }
            // The structured write path (#35): its actions are memory
            // actions, and the entry it superseded or updated was seen.
            "remember" => {
                let action = field("action").unwrap_or_default();
                let verb = match action.as_str() {
                    "add" => "created",
                    "supersede" => "superseded",
                    "update" => "edited",
                    "archive" => "archived",
                    "link" => "linked",
                    other => other,
                };
                let target = field("id").or_else(|| field("file")).unwrap_or_default();
                if let Some(id) = field("id") {
                    self.consult(id);
                }
                if !verb.is_empty() {
                    self.memory_actions
                        .insert(0, clip(&format!("{verb} {target}"), 80));
                }
            }
            "search_memory" => {
                for id in memory_hit_ids(result) {
                    self.consult(id);
                }
            }
            "status" => {
                if let Some(goal) = field("goal") {
                    self.goal = Some(clip(&goal, 240));
                }
                if let Some(next) = field("next") {
                    self.next = Some(clip(&next, 160));
                }
            }
            // Bash touches files too, and in agent sessions it is the
            // most common way: `sed -i`, redirects, `mv`/`cp`. The
            // extraction is a heuristic on the command text, so it
            // misses indirect writes (a script's inner redirect) and
            // over-captures a word that merely looks like a path; a
            // wrong row in a status list costs a glance, not an
            // action, so the trade favors recall.
            "bash" => {
                let Some(command) = field("command") else {
                    return;
                };
                for (path, touch) in bash_file_targets(&command, root) {
                    self.touch(path, touch);
                }
            }
            _ => {}
        }
    }

    /// One touch, newest-wins ordering: a re-touch moves the file to
    /// the front (the list is rendered top-down, and the file the
    /// agent touched ten seconds ago is the one the user wants
    /// first), and an edit outranks a read.
    fn touch(&mut self, path: String, touch: Touch) {
        match self.touched.iter().position(|(seen, _)| *seen == path) {
            Some(index) => {
                let (_, existing) = &mut self.touched[index];
                if touch == Touch::Edited {
                    *existing = Touch::Edited;
                }
                let entry = self.touched.remove(index);
                self.touched.insert(0, entry);
            }
            None => self.touched.insert(0, (path, touch)),
        }
    }

    /// One consultation, newest-wins ordering: a re-consulted entry
    /// moves to the front, matching `touch`'s treatment of files.
    fn consult(&mut self, id: String) {
        if let Some(index) = self.consulted.iter().position(|seen| seen == &id) {
            let entry = self.consulted.remove(index);
            self.consulted.insert(0, entry);
        } else {
            self.consulted.insert(0, id);
        }
    }

    /// GOAL's text: the model's own statement wins, the prompt is the
    /// fallback.
    pub fn goal_text(&self) -> Option<&str> {
        self.goal.as_deref().or(self.last_prompt.as_deref())
    }

    pub fn touch_of(&self, path: &str) -> Option<Touch> {
        self.touched
            .iter()
            .find(|(seen, _)| seen == path)
            .map(|(_, touch)| *touch)
    }
}

/// A tool result that did nothing: the loop's error and denial shapes.
fn failed(result: &str) -> bool {
    result.starts_with("error:") || result.starts_with("denied by user")
}

/// The ids a `search_memory` result names, its `[p12] file, added …`
/// hit lines (the format the tool writes, `gertie-tools` memory.rs).
pub fn memory_hit_ids(result: &str) -> Vec<String> {
    result
        .lines()
        .filter_map(|line| {
            // View results carry the file's entry lines, which begin
            // with the "- " bullet; search hits begin at the bracket.
            let line = line.trim_start_matches("- ").trim_start();
            let rest = line.strip_prefix('[')?;
            let (id, after) = rest.split_once(']')?;
            let valid =
                !id.is_empty() && id.len() <= 16 && id.chars().all(|c| c.is_ascii_alphanumeric());
            (valid && after.starts_with(' ')).then(|| id.to_string())
        })
        .collect()
}

/// A tool path as a normalized workspace-relative path: `./a/../b.rs`
/// and `/root/b.rs` both read `b.rs`. Paths outside the workspace keep
/// their absolute form. Lexical only (no filesystem reads), so a file
/// that no longer exists still normalizes.
pub fn normalize_path(root: &std::path::Path, raw: &str) -> Option<String> {
    use std::path::Component;
    let joined = resolve_tool_path(root, std::path::Path::new(raw));
    let mut clean = std::path::PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            other => clean.push(other.as_os_str()),
        }
    }
    let root_clean = {
        let mut clean_root = std::path::PathBuf::new();
        for component in root.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    clean_root.pop();
                }
                other => clean_root.push(other.as_os_str()),
            }
        }
        clean_root
    };
    let shown = match clean.strip_prefix(&root_clean) {
        Ok(relative) => relative.to_path_buf(),
        Err(_) => clean,
    };
    let text = shown.to_string_lossy().into_owned();
    (!text.is_empty()).then_some(text)
}

use gertie_core::text::ellipsize as clip;

/// DEPENDS ON's cache: the rows, keyed by the memory store's
/// fingerprint and the facts' version.
pub(crate) type GroundCache = ((u64, u64), Vec<GroundRow>);

/// One DEPENDS ON row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroundRow {
    pub id: String,
    pub text: String,
    /// The work consulted it (a `search_memory` hit). False: its
    /// evidence shares a file with the work.
    pub acting: bool,
    /// Cited files that changed since the entry was written, each
    /// with what this session did to it: a file the session read or
    /// edited is current in the model's context.
    pub changed: Vec<(String, Option<Touch>)>,
    /// The entry was superseded, by this id when the supersede named one.
    pub superseded: Option<String>,
    /// Why the entry carries recheck flags (the correction walk).
    pub walk: Vec<CorrectionStep>,
    /// The text of every entry the walk names (corrected, successor,
    /// flagged), so the chain renders as entries, not bare ids.
    pub walk_texts: Vec<(String, String)>,
}

impl GroundRow {
    /// Whether a correction or an evidence flag is in play: the riding
    /// line's full form and the column's flag color.
    pub fn flagged(&self) -> bool {
        !self.changed.is_empty() || !self.walk.is_empty() || self.superseded.is_some()
    }

    /// The evidence line: which cited files moved, and whether the
    /// work has seen them since.
    pub fn evidence_words(&self) -> Option<String> {
        self.evidence_parts()
            .map(|(files, rest)| format!("{files} {rest}"))
    }

    /// The evidence line in its two faces: the moved files (rendered
    /// mono) and the rest of the sentence.
    pub fn evidence_parts(&self) -> Option<(String, String)> {
        if self.changed.is_empty() {
            return None;
        }
        let files = self
            .changed
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let verdict = if self.changed.iter().all(|(_, touch)| touch.is_some()) {
            if self
                .changed
                .iter()
                .any(|(_, touch)| *touch == Some(Touch::Edited))
            {
                "edited this session"
            } else {
                "verified current before acting"
            }
        } else {
            "not re-read this session"
        };
        Some((
            files,
            format!("moved since [{}] was written, {verdict}", self.id),
        ))
    }

    /// An entry's text by id, as the walk names it.
    pub fn text_of(&self, id: &str) -> Option<&str> {
        self.walk_texts
            .iter()
            .find(|(seen, _)| seen == id)
            .map(|(_, text)| text.as_str())
    }
}

/// The DEPENDS ON rows: consulted entries first (in consult order),
/// then live entries whose evidence names a touched file. `changed`
/// is the store's evidence check (`MemoryStore::changed_evidence`),
/// injected so the rule is testable without files.
pub fn grounding(
    entries: &[MemoryEntry],
    facts: &StatusFacts,
    changed: impl Fn(&MemoryEntry) -> Vec<String>,
) -> Vec<GroundRow> {
    let row = |entry: &MemoryEntry, acting: bool| {
        let walk = gertie_core::correction_walk(entries, &entry.id);
        let mut walk_texts: Vec<(String, String)> = Vec::new();
        for step in &walk {
            let ids = [
                Some(&step.corrected),
                step.replaced_by.as_ref(),
                Some(&step.flagged),
            ];
            for id in ids.into_iter().flatten() {
                if walk_texts.iter().any(|(seen, _)| seen == id) {
                    continue;
                }
                if let Some(named) = entries.iter().find(|other| &other.id == id) {
                    walk_texts.push((id.clone(), named.text.clone()));
                }
            }
        }
        GroundRow {
            id: entry.id.clone(),
            text: entry.text.clone(),
            acting,
            changed: changed(entry)
                .into_iter()
                .map(|path| {
                    let touch = normalize_relative(&path).and_then(|path| facts.touch_of(&path));
                    (path, touch)
                })
                .collect(),
            superseded: entry
                .valid_until
                .is_some()
                .then(|| entry.superseded_by.clone().unwrap_or_default()),
            walk,
            walk_texts,
        }
    };
    let mut rows: Vec<GroundRow> = facts
        .consulted
        .iter()
        .filter_map(|id| entries.iter().find(|entry| &entry.id == id))
        .take(ACTING_SHOWN)
        .map(|entry| row(entry, true))
        .collect();
    let sharing = entries
        .iter()
        .filter(|entry| entry.valid_until.is_none())
        .filter(|entry| !facts.consulted.contains(&entry.id))
        .filter(|entry| {
            entry.evidence.iter().any(|cited| {
                normalize_relative(&cited.path).is_some_and(|path| facts.touch_of(&path).is_some())
            })
        })
        .take(SHARING_SHOWN)
        .map(|entry| row(entry, false));
    rows.extend(sharing);
    rows
}

/// An evidence path (already workspace-relative) in the same lexical
/// form `normalize_path` gives tool paths: file-level equality, never
/// a prefix match.
fn normalize_relative(path: &str) -> Option<String> {
    normalize_path(std::path::Path::new(""), path)
}

/// An ask pending in another open chat of this chat's project, as the
/// column shows it. Questions answer inline; permission asks only
/// jump to their chat (see what you approve).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectAsk {
    pub session: SessionId,
    pub session_title: String,
    pub ask: u64,
    /// The question, or the permission card's action label.
    pub question: String,
    /// The clickable answers; empty for a permission ask.
    pub options: Vec<String>,
    pub worker: bool,
}

impl ProjectAsk {
    pub fn is_question(&self) -> bool {
        !self.options.is_empty()
    }
}

/// WAITING ON YOU's reach: the other open chats' asks, kept only for
/// chats filed in the shown chat's project. An unfiled chat reaches
/// nothing (projects are optional; the sidebar marks cover the rest).
/// Each other chat comes as its project and its pending asks.
pub fn project_asks_for(
    shown_project: Option<i64>,
    others: impl IntoIterator<Item = (Option<i64>, Vec<ProjectAsk>)>,
) -> Vec<ProjectAsk> {
    let Some(project) = shown_project else {
        return Vec::new();
    };
    others
        .into_iter()
        .filter(|(theirs, _)| *theirs == Some(project))
        .flat_map(|(_, asks)| asks)
        .collect()
}

/// The collapsed summary, `run 12:30 · 3 touched · 1 ask`: the header
/// chip reads these parts verbatim.
pub fn summary_parts(run: Option<String>, touched: usize, asks: usize) -> Vec<String> {
    let mut parts = Vec::new();
    if let Some(run) = run {
        parts.push(format!("run {run}"));
    }
    if touched > 0 {
        parts.push(format!("{touched} touched"));
    }
    if asks > 0 {
        parts.push(format!("{asks} ask{}", if asks == 1 { "" } else { "s" }));
    }
    parts
}

impl ChatView {
    /// Rebuilds the facts from the open session's rows, off the frame:
    /// the whole session, not the transcript's loaded page.
    pub(super) fn rebuild_status_facts(&mut self, cx: &mut Context<Self>) {
        self.status_facts = StatusFacts::default();
        self.status_version += 1;
        self.status_collapsed = self
            .session_id
            .map(|id| self.store.store_flag(&status_flag(id)))
            .unwrap_or(self.status_collapsed);
        let Some(session_id) = self.session_id else {
            return;
        };
        let store = self.store.clone();
        let root = self.workspace_root.clone();
        cx.spawn(async move |this, cx| {
            // `cx.spawn` runs on the UI thread: the read and the fold
            // go to the background executor.
            let facts = cx
                .background_executor()
                .spawn(async move {
                    let rows = store.load_messages(session_id).unwrap_or_default();
                    StatusFacts::from_rows(&rows, &root)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // A switch away meanwhile: the facts are someone else's.
                if this.session_id == Some(session_id) {
                    this.status_facts = facts;
                    this.status_version += 1;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// A live tool result, folded in (the rebuild covers reloads).
    pub(super) fn fold_status_tool(&mut self, name: &str, arguments: &str, result: &str) {
        let root = self.workspace_root.clone();
        self.status_facts.fold_tool(name, arguments, result, &root);
        self.reverify_on_reread(name, arguments, result, &root);
        self.status_version += 1;
    }

    /// The evidence flag's automation: when the session's work re-reads
    /// a file that a consulted entry cites, the reread IS the re-check
    /// (the content is in the model's context), so the store re-records
    /// the hash and the flag clears without a click or a tool call. An
    /// edit counts too: folding happens after the result, so the hash
    /// lands on the file's post-edit state. A failed reread does not:
    /// the content never reached the model's context, so the cite
    /// keeps its stale flag. A stale fact should still supersede; this
    /// is for facts that survived the refactor.
    fn reverify_on_reread(
        &mut self,
        name: &str,
        arguments: &str,
        result: &str,
        root: &std::path::Path,
    ) {
        if !matches!(name, "read_file" | "edit_file" | "write_file" | "bash") {
            return;
        }
        // A failed tool call is not a reread: nothing arrived in the
        // model's context to re-verify against.
        if result.starts_with("error:") {
            return;
        }
        let Some(store) = self.memory_store.clone() else {
            return;
        };
        // The reread paths: the file tools name one file in their
        // arguments; bash names any number across the whole command
        // (sed, grep, cat, python reading a file), already extracted
        // and gate-checked by the TOUCHED state machine.
        let showns: Vec<String> = if name == "bash" {
            let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) else {
                return;
            };
            let Some(command) = args.get("command").and_then(|value| value.as_str()) else {
                return;
            };
            bash_file_targets(command, root)
                .into_iter()
                .map(|(path, _)| path)
                .collect()
        } else {
            let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) else {
                return;
            };
            let Some(raw) = args.get("path").and_then(|value| value.as_str()) else {
                return;
            };
            match normalize_path(root, raw) {
                Some(shown) => vec![shown],
                None => vec![],
            }
        };
        let _ = result;
        let entries = store.scan();
        for id in self.status_facts.consulted.clone() {
            let Some(entry) = entries.iter().find(|entry| entry.id == id) else {
                continue;
            };
            let hits = entry.evidence.iter().any(|cited| {
                showns.contains(&cited.path) && store.changed_evidence(entry).contains(&cited.path)
            });
            if hits {
                // Re-verify only the cites the session actually re-read:
                // a second stale cite of this entry, one the model never
                // re-saw here, keeps its flag.
                store.reverify_evidence_paths(&id, &showns);
            }
        }
    }

    pub(super) fn fold_status_user(&mut self, text: &str) {
        self.status_facts.fold_user(text);
        self.status_version += 1;
    }

    /// Re-checks the evidence on the next frame: the tab came back
    /// into view (files may have moved while it was away).
    pub(super) fn invalidate_grounding(&mut self) {
        self.status_version += 1;
    }

    /// The DEPENDS ON rows, cached: the evidence check hashes files,
    /// so it runs when the facts or the store change (a tool finished,
    /// the tab came back, a memory write), never per frame.
    pub(crate) fn ground_rows(&self) -> Vec<GroundRow> {
        let Some(store) = self.memory_store.as_ref() else {
            return Vec::new();
        };
        if self.status_facts.consulted.is_empty() && self.status_facts.touched.is_empty() {
            return Vec::new();
        }
        let key = (store.fingerprint(), self.status_version);
        if let Some((seen, rows)) = self.status_ground.borrow().as_ref()
            && *seen == key
        {
            return rows.clone();
        }
        let entries = store.scan();
        let rows = grounding(&entries, &self.status_facts, |entry| {
            store.changed_evidence(entry)
        });
        self.status_ground.replace(Some((key, rows.clone())));
        rows
    }

    /// This chat's pending cards, for the shell to show in the other
    /// chats of the project. The mode confirm is this chat's own
    /// business and never leaves it.
    pub fn pending_asks(&self) -> Vec<ProjectAsk> {
        let Some(session) = self.session_id else {
            return Vec::new();
        };
        let title = self.tab_title().to_string();
        self.turns
            .iter()
            .filter_map(|turn| match turn {
                Turn::Tool {
                    call,
                    status: ToolStatus::Pending(ask),
                    worker,
                    ..
                } if call.name != "mode_confirm" => {
                    let (question, options) = match transcript::ask_face(call) {
                        Some(face) if call.name == "ask_user" && !face.options.is_empty() => {
                            (face.question, face.options)
                        }
                        _ => (
                            crate::tools_ui::action_label(&call.name, &call.arguments, true),
                            Vec::new(),
                        ),
                    };
                    Some(ProjectAsk {
                        session,
                        session_title: title.clone(),
                        ask: *ask,
                        question,
                        options,
                        worker: *worker,
                    })
                }
                _ => None,
            })
            .collect()
    }

    /// How many cards wait on the human here: the shell's cue to
    /// repaint the other chats' columns.
    pub fn pending_ask_count(&self) -> usize {
        self.turns
            .iter()
            .filter(|turn| {
                matches!(
                    turn,
                    Turn::Tool {
                        status: ToolStatus::Pending(_),
                        ..
                    }
                )
            })
            .count()
    }

    /// The project a conversation is filed in, as this view's list
    /// knows it (the shell asks the view on screen, whose list is the
    /// freshest: moves happen in its sidebar).
    pub fn project_of(&self, session: SessionId) -> Option<i64> {
        self.sessions
            .iter()
            .find(|summary| summary.id == session)
            .and_then(|summary| summary.project_id)
    }

    /// This chat's own project: the session's, or the project a blank
    /// chat will be filed in.
    pub fn project_id(&self) -> Option<i64> {
        match self.session_id {
            Some(id) => self.project_of(id),
            None => self.pending_project,
        }
    }

    /// The shell's push: the project's pending asks, every frame.
    pub fn set_project_asks(&mut self, asks: Vec<ProjectAsk>) {
        self.project_asks = asks;
    }

    /// An answer clicked in another chat's column, applied here, to
    /// the owning card: the same `decide_tool` path a click on the
    /// card takes, so the tool result (the record) is identical. The
    /// card says where the answer came from. Questions only: a
    /// permission ask is never approved from elsewhere.
    pub fn answer_from_column(&mut self, ask: u64, answer: String, cx: &mut Context<Self>) -> bool {
        let found = self
            .turns
            .iter()
            .enumerate()
            .find_map(|(index, turn)| match turn {
                Turn::Tool {
                    call,
                    status: ToolStatus::Pending(id),
                    ..
                } if *id == ask && call.name == "ask_user" => Some((index, call.id.clone())),
                _ => None,
            });
        let Some((index, call_id)) = found else {
            return false;
        };
        self.answered_from_column.insert(call_id);
        self.decide_tool(
            index,
            PermissionDecision::Deny {
                feedback: Some(answer),
            },
            cx,
        );
        true
    }

    /// The riding card (#33 decision 1, #34): the latest turn's
    /// momentary state, the mockup's `.riding`. One line: the pulsing
    /// dot, the verb (what the work is doing now), and the why (the
    /// memory entry the work consulted). The full form, a second line
    /// in the flag color, only while a correction or evidence flag is
    /// in play on that entry; the card's edge and dot turn the flag
    /// color with it. Goal and next live in the column, never here:
    /// the two surfaces do not duplicate.
    pub(crate) fn render_riding_card(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let verb = self.activity_line();
        let riding = self.riding_grounding();
        let flag = riding.as_ref().and_then(|(_, _, flag)| flag.clone());
        let edge = if flag.is_some() {
            theme.danger
        } else {
            theme.blue
        };
        // The pulse, time-derived like the spinner it replaces: four
        // steps on the working tick's 2 fps, one 2s breath, so nothing
        // animates at rest (the card only renders while a turn runs).
        const PULSE: [f32; 4] = [1.0, 0.65, 0.3, 0.65];
        let pulse = PULSE[(self.activity_at.elapsed().as_millis() as usize / 500) % PULSE.len()];
        let line = h_flex()
            .gap(px(10.))
            .items_center()
            .min_w_0()
            .child(
                div()
                    .size(px(7.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(edge)
                    .opacity(pulse),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .max_w(relative(0.6))
                    .truncate()
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    // Activity strings carry `code` spans (tool labels
                    // quote paths and commands), same treatment as
                    // the cards.
                    .child(crate::markdown::inline_element(&verb, cx)),
            )
            .when_some(riding.as_ref(), |line, (id, text, _)| {
                line.child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .items_center()
                        .text_color(theme.muted_foreground)
                        .child(div().flex_shrink_0().child("· depends on"))
                        .child(
                            div()
                                .flex_shrink_0()
                                .font_family(theme.mono_font_family.clone())
                                .text_size(px(11.))
                                .text_color(theme.foreground)
                                .child(SharedString::from(format!("[{id}]"))),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(format!("\u{201c}{text}\u{201d}"))),
                        ),
                )
            });
        v_flex()
            .id("riding-card")
            .w_full()
            .mt(px(10.))
            .gap_1()
            .px(px(14.))
            .py_2()
            .rounded(px(6.))
            .bg(theme.secondary)
            .border_l(px(3.))
            .border_color(edge)
            .text_size(px(12.5))
            .child(line)
            .when_some(flag, |card, flag| {
                card.child(
                    div()
                        .id("riding-flag")
                        .pl(px(17.))
                        .text_size(px(12.))
                        .text_color(theme.danger)
                        .child(SharedString::from(flag))
                        .test_support(),
                )
            })
            .test_support()
            .into_any_element()
    }

    /// The riding card's grounding: the newest consulted entry, and,
    /// when a correction or evidence flag is in play on it, the flag's
    /// words for the full form. None when the work consulted nothing.
    pub(crate) fn riding_grounding(&self) -> Option<(String, String, Option<String>)> {
        let ground = self.ground_rows();
        let row = self
            .status_facts
            .consulted
            .iter()
            .rev()
            .find_map(|id| ground.iter().find(|row| row.acting && &row.id == id))?;
        let flag = if !row.flagged() {
            None
        } else if let Some(step) = row.walk.first() {
            Some(step.sentence())
        } else if let Some(words) = row.evidence_words() {
            Some(format!("evidence changed: {words}"))
        } else {
            row.superseded.as_ref().map(|by| {
                if by.is_empty() {
                    format!("[{}] was superseded", row.id)
                } else {
                    format!("[{}] was superseded by [{by}]", row.id)
                }
            })
        };
        Some((row.id.clone(), clip(&row.text, 80), flag))
    }
}

impl ChatView {
    /// The column's facts as the tests read them: the goal text, the
    /// next step, touched files with their word, DEPENDS ON's ids.
    #[allow(clippy::type_complexity)]
    pub fn status_for_test(
        &self,
    ) -> (
        Option<String>,
        Option<String>,
        Vec<(String, &'static str)>,
        Vec<String>,
    ) {
        (
            self.status_facts.goal_text().map(str::to_string),
            self.status_facts.next.clone(),
            self.status_facts
                .touched
                .iter()
                .map(|(path, touch)| {
                    (
                        path.clone(),
                        match touch {
                            Touch::Read => "read",
                            Touch::Edited => "edited",
                        },
                    )
                })
                .collect(),
            self.ground_rows().into_iter().map(|row| row.id).collect(),
        )
    }

    /// Folds a tool result the way a live turn does, for the riding
    /// card's positive-path test.
    pub fn fold_status_tool_for_test(&mut self, name: &str, arguments: &str, result: &str) {
        self.fold_status_tool(name, arguments, result);
    }

    /// Turns memory on for this view (the fixtures build it off), so
    /// recall on touch rides the turn.
    pub fn enable_memory_for_test(&mut self) {
        self.memory_enabled = true;
    }

    /// How many cards here were answered from another chat's column.
    pub fn answered_from_column_for_test(&self) -> usize {
        self.answered_from_column.len()
    }

    /// The riding line's grounding id and its flag words, if any.
    pub fn riding_for_test(&self) -> Option<(String, Option<String>)> {
        self.riding_grounding().map(|(id, _, flag)| (id, flag))
    }
}

/// The store flag holding a conversation's folded column.
fn status_flag(session: SessionId) -> String {
    format!("status_collapsed:{}", session.0)
}

/// The full sidebar, opened from the rail (#54): the rail is the
/// default, so the stored choice is the expansion.
pub(crate) const SIDEBAR_EXPANDED_FLAG: &str = "sidebar_expanded";

#[cfg(test)]
/// A tempdir root carrying the real files the tests' folds name:
/// the existence gates demand them (a constant `/work/repo` root
/// turned every fold into a no-op when the gates landed).
fn root_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "fn a() {}").unwrap();
    std::fs::write(root.join("src/b.rs"), "fn b() {}").unwrap();
    std::fs::write(root.join("src/teardown.rs"), "fn t() {}").unwrap();
    (tmp, root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touched_files_fold_with_edited_winning_and_failures_ignored() {
        let (_tmp, root) = root_fixture();
        let mut facts = StatusFacts::default();
        facts.fold_tool("read_file", r#"{"path":"src/a.rs"}"#, "fn a() {}", &root);
        facts.fold_tool("read_file", r#"{"path":"./src/b.rs"}"#, "fn b() {}", &root);
        facts.fold_tool(
            "edit_file",
            &format!(r#"{{"path":"{}"}}"#, root.join("src/a.rs").display()),
            "edited",
            &root,
        );
        facts.fold_tool("read_file", r#"{"path":"src/a.rs"}"#, "fn a() {}", &root);
        facts.fold_tool(
            "write_file",
            r#"{"path":"src/c.rs"}"#,
            "error: denied by user",
            &root,
        );
        facts.fold_tool(
            "edit_file",
            r#"{"path":"src/b.rs"}"#,
            "denied by user",
            &root,
        );
        facts.fold_tool("grep", r#"{"path":"src"}"#, "hits", &root);
        assert_eq!(
            facts.touched,
            vec![
                ("src/a.rs".to_string(), Touch::Edited),
                ("src/b.rs".to_string(), Touch::Read),
            ]
        );
    }

    #[test]
    fn the_status_tool_sets_goal_and_next_and_the_prompt_is_the_fallback() {
        let (_tmp, root) = root_fixture();
        let mut facts = StatusFacts::default();
        facts.fold_user("\n  Fix the widget_test flake\nmore detail");
        assert_eq!(facts.goal_text(), Some("Fix the widget_test flake"));
        assert_eq!(facts.next, None, "no next step until the model states one");
        facts.fold_tool(
            "status",
            r#"{"goal":"Fix the teardown race","next":"re-run the suite"}"#,
            "status noted",
            &root,
        );
        facts.fold_user("Makes sense, go ahead");
        assert_eq!(facts.goal_text(), Some("Fix the teardown race"));
        facts.fold_tool("status", r#"{"next":"commit"}"#, "status noted", &root);
        assert_eq!(facts.goal.as_deref(), Some("Fix the teardown race"));
        assert_eq!(facts.next.as_deref(), Some("commit"));
        facts.fold_tool("status", r#"{}"#, "error: status: give a goal", &root);
        assert_eq!(facts.next.as_deref(), Some("commit"));
    }

    #[test]
    fn search_memory_hits_are_what_the_work_consulted() {
        let (_tmp, root) = root_fixture();
        let result = "[p12] gotchas.md, added 2026-11-01\n   cite the flake\n   links: builds_on p7\n\n[g3] prefs.md, added -\n   tabs\n";
        assert_eq!(memory_hit_ids(result), vec!["p12", "g3"]);
        let mut facts = StatusFacts::default();
        // The memory tool's view is a consultation too: the model's
        // natural path (the live test's gap — no card fired when the
        // model viewed the files).
        let mut viewed = StatusFacts::default();
        viewed.fold_tool(
            "memory",
            r#"{"command":"view","path":"project/gotchas.md"}"#,
            "# Gotchas\n\n- [p1] helpful=0 harmful=0 the sprite theory [source: hand; added: 2026-11-25]\n- [p2] helpful=0 harmful=0 the assets layer was the fix [source: 44837af; added: 2026-11-25]\n",
            &root,
        );
        assert_eq!(
            viewed.consulted,
            vec!["p2", "p1"],
            "a view saw the entries, newest first"
        );
        // Mutating commands are actions, not consultations.
        let mut created = StatusFacts::default();
        created.fold_tool(
            "memory",
            r#"{"command":"create","path":"project/gotchas.md"}"#,
            "created",
            &root,
        );
        assert!(created.consulted.is_empty(), "create consults nothing");
        facts.fold_tool("search_memory", r#"{"query":"flake"}"#, result, &root);
        facts.fold_tool(
            "search_memory",
            r#"{"query":"x"}"#,
            "[p12] a.md, added -\n",
            &root,
        );
        // Newest-first: the re-consulted p12 moved to the front, g3
        // stays below it (no duplicate from the second hit).
        assert_eq!(facts.consulted, vec!["p12", "g3"]);
        facts.fold_tool(
            "search_memory",
            r#"{"query":"y"}"#,
            "no memory matches",
            &root,
        );
        assert_eq!(facts.consulted.len(), 2);
        facts.fold_tool(
            "search_memory",
            r#"{"query":"z"}"#,
            "[g3] b.md, added -\n",
            &root,
        );
        // A fresh consultation lands at the front.
        assert_eq!(facts.consulted, vec!["g3", "p12"]);
    }

    fn entry(id: &str, evidence: &[&str]) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            file: "notes.md".into(),
            text: format!("entry {id}"),
            helpful: 0,
            harmful: 0,
            source_session: None,
            added_at: None,
            valid_until: None,
            superseded_by: None,
            links: Vec::new(),
            evidence: evidence
                .iter()
                .map(|path| gertie_core::memory::Evidence {
                    path: path.to_string(),
                    hash: "000000000000".into(),
                })
                .collect(),
            recheck: Vec::new(),
        }
    }

    #[test]
    fn grounding_lists_consulted_entries_then_file_level_overlap() {
        let (_tmp, root) = root_fixture();
        let mut facts = StatusFacts::default();
        facts.fold_tool("read_file", r#"{"path":"src/teardown.rs"}"#, "x", &root);
        facts.consulted = vec!["p12".into()];
        let entries = vec![
            entry("p12", &["src/teardown.rs"]),
            entry("p20", &["src/teardown.rs"]),
            // A prefix of a touched path is not the same file.
            entry("p21", &["src/tear"]),
            entry("p22", &["src"]),
            entry("p23", &[]),
        ];
        let rows = grounding(&entries, &facts, |entry| {
            if entry.id == "p12" {
                vec!["src/teardown.rs".into()]
            } else {
                Vec::new()
            }
        });
        let ids: Vec<(&str, bool)> = rows
            .iter()
            .map(|row| (row.id.as_str(), row.acting))
            .collect();
        assert_eq!(ids, vec![("p12", true), ("p20", false)]);
        assert_eq!(
            rows[0].evidence_words().as_deref(),
            Some("src/teardown.rs moved since [p12] was written, verified current before acting")
        );
        assert!(rows[0].flagged());
        assert!(!rows[1].flagged());
    }

    #[test]
    fn evidence_the_work_never_saw_says_so() {
        let facts = StatusFacts {
            consulted: vec!["p12".into()],
            ..Default::default()
        };
        let rows = grounding(&[entry("p12", &["src/a.rs"])], &facts, |_| {
            vec!["src/a.rs".into()]
        });
        assert_eq!(
            rows[0].evidence_words().as_deref(),
            Some("src/a.rs moved since [p12] was written, not re-read this session")
        );
    }

    #[test]
    fn nothing_consulted_or_touched_means_no_grounding() {
        let facts = StatusFacts::default();
        assert!(grounding(&[entry("p1", &["a.rs"])], &facts, |_| Vec::new()).is_empty());
    }

    fn ask(session: u64, id: u64) -> ProjectAsk {
        ProjectAsk {
            session: SessionId(session),
            session_title: format!("chat {session}"),
            ask: id,
            question: "Which database?".into(),
            options: vec!["postgres".into()],
            worker: false,
        }
    }

    #[test]
    fn asks_reach_only_across_the_shown_chats_project() {
        let others = || {
            vec![
                (Some(7), vec![ask(2, 10)]),
                (Some(8), vec![ask(3, 11)]),
                (None, vec![ask(4, 12)]),
                (Some(7), vec![ask(5, 13), ask(5, 14)]),
            ]
        };
        let reached: Vec<u64> = project_asks_for(Some(7), others())
            .iter()
            .map(|ask| ask.ask)
            .collect();
        assert_eq!(reached, vec![10, 13, 14]);
        assert!(
            project_asks_for(None, others()).is_empty(),
            "an unfiled chat reaches nothing"
        );
    }

    #[test]
    fn the_summary_names_only_what_exists() {
        assert_eq!(
            summary_parts(Some("12:30".into()), 3, 1),
            vec!["run 12:30", "3 touched", "1 ask"]
        );
        assert_eq!(summary_parts(None, 0, 2), vec!["2 asks"]);
        assert!(summary_parts(None, 0, 0).is_empty());
    }
}

/// File targets a bash command touches, as `(path, touch)` pairs. The
/// heuristic reads the command's operators: redirects and edit-in-place
/// commands mark edited, plain file arguments mark read. Quoted paths
/// with spaces are handled by the token pattern; a path is only
/// accepted when it names something under the workspace root after
/// normalization (absolute or relative), so `/usr/bin/env bash` never
/// lands in the list.
fn bash_file_targets(command: &str, root: &std::path::Path) -> Vec<(String, Touch)> {
    let mut out: Vec<(String, Touch)> = Vec::new();
    // Left-to-right with a pending classification: an operator names
    // what the NEXT path-like token is (edited for writers, read for
    // readers). `sed -i X` and `> X` put their target one or two
    // tokens after the operator; a scan keyed on "what came before
    // this token" misses both.
    let mut pending: Option<Touch> = None;
    // Creating operators (>, >>, touch, cp/mv targets) name files
    // that need not exist yet; mutating ones (sed -i) require it.
    let mut pending_creates = false;
    for token in command.split_whitespace() {
        let quoted = token.starts_with('"') || token.starts_with('\'');
        let cleaned = token.trim_matches(|c| c == '"' || c == '\'');
        let glued = cleaned.trim_start_matches(['>', '&', '2']);
        if glued != cleaned && glued.len() >= 2 && glued.contains('/') {
            // Stream dumps (2>/dev/null, >&2) edit nothing the user
            // cares about; /tmp is scratch by convention.
            if !is_scratch(glued) && !glued.contains("&&") && path_in_root(root, glued) {
                // Redirect targets are created if absent: containment
                // (under root), not existence, is the gate here.
                if let Some(normalized) = normalize_path(root, glued) {
                    push_unique(&mut out, normalized, Touch::Edited);
                }
            }
            pending = None;
            continue;
        }
        // Stream dumps and scratch targets are never file state:
        // 2>/dev/null edits nothing the user cares about; /tmp is
        // scratch by convention (the hardening probes' own workspace).
        if is_scratch(glued) {
            pending = None;
            continue;
        }
        match cleaned {
            ">" | ">>" | "tee" | "sed" | "mv" | "cp" | "rm" | "touch" => {
                pending = Some(Touch::Edited);
                pending_creates = !matches!(cleaned, "sed");
            }
            "cat" | "head" | "tail" | "less" | "grep" | "stat" | "wc" => {
                pending = Some(Touch::Read)
            }
            _ if cleaned.starts_with('-') => {}
            // A quoted token is an expression for editor ops (sed's
            // s/a/b/ looks like a path but never is one); it stays a
            // candidate for reader ops (cat "my file.md"). Pending
            // survives the skip: the real target follows.
            _ if quoted && pending != Some(Touch::Read) => {}
            _ => {
                if let (Some(touch), true) = (pending, looks_like_path(cleaned)) {
                    pending = None;
                    // The existence gate: a status list answers "what
                    // did the agent touch", and a token that names no
                    // real file touches nothing — script fragments
                    // (text.replace(old,), error strings, sed
                    // expressions) die here instead of rendering as
                    // rows. Only real, in-workspace files pass.
                    let exists_ok = pending_creates || is_real_workspace_file(root, cleaned);
                    // Build output and logs are not file state the
                    // user cares about when EDITED: a build run
                    // should not push out the files that matter.
                    // Reads stay: reading a log is sometimes the
                    // work itself (a crash diagnosis).
                    let not_artifact = touch != Touch::Edited || !is_artifact_path(cleaned);
                    if !exists_ok || !not_artifact {
                        continue;
                    }
                    if let Some(normalized) = normalize_path(root, cleaned) {
                        push_unique(&mut out, normalized, touch);
                    }
                }
            }
        }
    }
    out
}

/// A token is plausibly a path: it carries a slash or a dot. Flags,
/// URLs, and bare words are not.
fn looks_like_path(token: &str) -> bool {
    !token.starts_with('-')
        && !token.contains("://")
        && (token.contains('/') || token.contains('.'))
}

/// The token names a file that actually exists under the workspace
/// root: the extractor's recall-vs-precision trade resolves to
/// precision, because a 584-row list of script fragments (the hand
/// test's finding) is worse than a 10-row list of real files.
fn is_real_workspace_file(root: &std::path::Path, token: &str) -> bool {
    let joined = resolve_tool_path(root, std::path::Path::new(token));
    joined.is_file() && path_in_root(root, token)
}

/// The path (absolute or root-relative) stays under the workspace
/// root: the containment the tool layer enforces, mirrored here so
/// `../escape` and absolute paths outside root never land.
fn path_in_root(root: &std::path::Path, token: &str) -> bool {
    let joined = resolve_tool_path(root, std::path::Path::new(token));
    let Ok(joined) = joined.canonicalize() else {
        // Non-existent (a redirect target): lexical containment only.
        return joined.starts_with(root);
    };
    let Ok(root_canon) = root.canonicalize() else {
        return false;
    };
    joined.starts_with(root_canon)
}

fn push_unique(out: &mut Vec<(String, Touch)>, path: String, touch: Touch) {
    if !out.iter().any(|(seen, _)| *seen == path) {
        out.push((path, touch));
    }
}

/// True when the token lands outside the user's project and outside
/// the device tree: /tmp and /var/tmp are scratch by convention.
fn is_scratch(token: &str) -> bool {
    token.starts_with("/dev/") || token.starts_with("/tmp/") || token.starts_with("/var/tmp/")
}

/// Build output and throwaway state, wherever it lands: target/,
/// node_modules/, log files, lock files' temp siblings. The TOUCHED
/// gate drops EDITED hits here (a build is not the work; a READ of a
/// log can be), and strips the shell's `;`-glued tokens the heuristics
/// would otherwise keep (`build.log;` from a trailing semicolon).
fn is_artifact_path(token: &str) -> bool {
    let token = token.trim_end_matches(';');
    let file = token.rsplit('/').next().unwrap_or(token);
    let logged = {
        let lower = file.to_ascii_lowercase();
        lower.ends_with(".log") || lower.ends_with(".log.tmp") || lower.ends_with(".lock")
    };
    token
        .split('/')
        .any(|segment| matches!(segment, "target" | "node_modules" | ".git"))
        || logged
}

#[test]
fn newest_wins_ordering_moves_retouched_files_front() {
    let (_tmp, root) = root_fixture();
    let mut facts = StatusFacts::default();
    facts.fold_tool("read_file", r#"{"path":"src/a.rs"}"#, "ok", &root);
    facts.fold_tool("read_file", r#"{"path":"src/b.rs"}"#, "ok", &root);
    // b is newest: it renders first.
    assert_eq!(facts.touched[0].0, "src/b.rs");
    facts.fold_tool("edit_file", r#"{"path":"src/a.rs"}"#, "ok", &root);
    // a was re-touched (edited): front, and upgraded.
    assert_eq!(facts.touched[0].0, "src/a.rs");
    assert_eq!(facts.touched[0].1, Touch::Edited);
}
#[cfg(test)]
mod memory_action_tests {
    use super::*;

    #[test]
    fn memory_actions_are_newest_first_capped_and_skip_views() {
        let root = std::path::Path::new("/nonexistent");
        let mut facts = StatusFacts::default();
        for index in 0..7 {
            facts.fold_tool(
                "memory",
                &format!(r#"{{"command":"create","path":"project/t{index}.md"}}"#),
                "created",
                root,
            );
        }
        facts.fold_tool(
            "memory",
            r#"{"command":"view","path":"project/t1.md"}"#,
            "x",
            root,
        );
        facts.fold_tool(
            "remember",
            r#"{"action":"supersede","id":"p12","text":"new"}"#,
            "added p13",
            root,
        );
        assert_eq!(
            facts.memory_actions.len(),
            8,
            "no data cap: all 7 creates + the supersede"
        );
        assert_eq!(facts.memory_actions[0], "superseded p12");
        assert_eq!(facts.memory_actions[1], "created t6.md");
        assert_eq!(facts.memory_actions[7], "created t0.md", "oldest last");
        assert!(facts.consulted.contains(&"p12".to_string()));
    }
}

#[cfg(test)]
mod bash_tests {
    use super::*;

    #[test]
    fn build_artifacts_do_not_count_as_touched_edits() {
        // The screenshot's case: a build run flooded TOUCHED with
        // target/ paths and log files, crowding out the files that
        // matter. Writes into build output are not the work.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("target/debug/out.log"), "").unwrap();
        let targets = bash_file_targets(
            "cargo build 2> target/debug/out.log && cp src/main.rs src/main.rs.bak",
            root,
        );
        assert!(
            !targets
                .iter()
                .any(|(path, _)| path.contains("target/") || path.ends_with(".log")),
            "build debris stays out: {targets:?}"
        );
        // Reading a log is sometimes the work itself (a crash
        // diagnosis), so reads keep their recall.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("crash.log"), "x").unwrap();
        let targets = bash_file_targets("tail crash.log", root);
        assert_eq!(targets.len(), 1, "a read of a log is signal: {targets:?}");
    }

    #[test]
    fn redirects_and_edits_classify_as_edited() {
        // The existence gate needs real files: a tempdir root with a
        // real file and a real subdir.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        // Redirects create their target: containment (not existence)
        // is the gate there.
        let targets = bash_file_targets("echo hi > out.txt", root);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].1, Touch::Edited);
        // sed -i edits in place: the file must exist.
        let targets = bash_file_targets("sed -i 's/a/b/' src/main.rs", root);
        assert_eq!(targets[0].1, Touch::Edited);
        assert!(targets[0].0.contains("main.rs"));
    }

    #[test]
    fn reads_classify_as_read_and_noise_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("README.md"), "x").unwrap();
        let targets = bash_file_targets("cat README.md", root);
        assert_eq!(targets.len(), 1, "the real file passes: {targets:?}");
        assert_eq!(targets[0].1, Touch::Read);
        // Flags, bare words, and URLs are not files.
        assert!(bash_file_targets("cargo build --release", root).is_empty());
        assert!(bash_file_targets("curl https://example.com/x", root).is_empty());
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    #[test]
    fn script_fragments_never_land_in_the_list() {
        let root = std::path::Path::new("/tmp/gertie-gates");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/real.rs"), "fn main() {}\n").unwrap();
        // The hand test's garbage: tokens with dots/slashes that name
        // nothing.
        let targets =
            bash_file_targets("python3 -c \"text.replace(old, new)\" && echo s/a/b", root);
        assert!(
            targets.iter().all(|(path, _)| !path.contains("replace")),
            "script fragments must die: {targets:?}"
        );
        // The real file passes.
        let targets = bash_file_targets("cat src/real.rs", root);
        assert_eq!(targets.len(), 1);
        assert!(targets[0].0.contains("real.rs"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn escapes_and_absolute_outside_paths_are_refused() {
        let root = std::path::Path::new("/tmp/gertie-gates2");
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("in.md"), "x").unwrap();
        assert!(bash_file_targets("cat ../outside.md", root).is_empty());
        assert!(bash_file_targets("cat /etc/passwd", root).is_empty());
        assert!(!bash_file_targets("cat in.md", root).is_empty());
        std::fs::remove_dir_all(root).ok();
    }
}

#[cfg(test)]
mod grep_repro_tests {
    use super::*;

    /// The hand test's garbage rows, reproduced: grep patterns with
    /// dots/slashes pass looks_like_path and (pre-gate) landed as
    /// "read" rows. The existence gate should kill every one.
    #[test]
    fn grep_patterns_never_land_as_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("real.md"), "x").unwrap();
        // The exact commands from the 584-row session.
        for command in [
            r#"grep -n '"FILES"\|TOUCHED' gertie/src/chat/status.rs | head -6"#,
            r#"grep -n "FAILED\|FAILED\." /tmp/t.log | head -6"#,
            r#"grep -n "4.5\|budget" research/M6-PLAN.md"#,
            r#"grep -n "text.replace(old2," gertie/src/chat.rs"#,
        ] {
            let targets = bash_file_targets(command, root);
            for (path, _) in &targets {
                assert!(
                    path.contains("status.rs") || path.contains("M6-PLAN"),
                    "a grep fragment landed as a path: {path:?} from {command}"
                );
            }
        }
    }
}
